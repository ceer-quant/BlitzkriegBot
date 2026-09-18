//! End-to-end for C ABI **v2** (E7 / issue #38): load the example user-layer
//! strategy shared library through the full-featured ABI, register it into a
//! real [`Engine`] exactly as the service does, and drive it with full-depth
//! books / round context / hot parameters. Entries come back as kernel-sized
//! `OrderRequest`s, closes as drained exit intents; confirmation/diagnostics
//! prove the dylib sees the whole ladder; a missing-version library is rejected
//! at negotiation.
//!
//! Requires the library to be built first (CI builds it):
//!   (cd user_layer/strategies && cargo build --release)
//! Set `BK_REQUIRE_DYLIB=1` to turn "dylib absent" from a skip into a hard fail.
#![cfg(feature = "strategy-loading")]

use blitzkrieg_core::engine::{DataEvent, Engine, EngineConfig};
use blitzkrieg_core::model::CryptoMarket;
use blitzkrieg_core::scanner::ScannerConfig;
use blitzkrieg_core::signal::{SpreadArbConfig, TrendConfig};
use blitzkrieg_core::strategy_engine::loader::load_foreign;
use rust_decimal_macros::dec;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn dylib_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("user_layer")
        .join("strategies")
        .join("target")
        .join("release")
}

fn lib_path_named(base_name: &str) -> Option<PathBuf> {
    let base = dylib_dir();
    for ext in ["dylib", "so", "dll"] {
        let name = if ext == "dll" {
            format!("{base_name}.dll")
        } else {
            format!("lib{base_name}.{ext}")
        };
        let p = base.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[allow(dead_code)]
fn lib_path() -> Option<PathBuf> {
    lib_path_named("dog_strategy")
}

fn require_named_lib(base_name: &str) -> PathBuf {
    match lib_path_named(base_name) {
        Some(p) => p,
        None => {
            let msg =
                "build the v2 dylibs first: (cd user_layer/strategies && cargo build --release)"
                    .to_string();
            if std::env::var_os("BK_REQUIRE_DYLIB").is_some() {
                panic!("{msg} (looked in {})", dylib_dir().display());
            }
            eprintln!("skipping: {msg}");
            std::process::exit(0);
        }
    }
}

/// Skip (or, under BK_REQUIRE_DYLIB=1, hard-fail) when the cdylib was not built.
fn require_lib() -> PathBuf {
    require_named_lib("dog_strategy")
}

fn engine_cfg() -> EngineConfig {
    EngineConfig {
        mean_reversion: blitzkrieg_core::signal::MeanReversionConfig::default(),
        scanner: ScannerConfig {
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            min_time_left_sec: 0,
        },
        trend: TrendConfig {
            confirm_sec: 5,
            ratio: dec!(0.5),
            min_price: dec!(0.5),
            broken_price: dec!(0.35),
            window_floor_ms: 0,
        },
        spread_arb: SpreadArbConfig {
            trend_max_entry_price: dec!(0.45),
            ..Default::default()
        },
        trend_follow: Default::default(),
        max_orderbook_stale_ms: 8000,
        momentum_window_sec: 30,
        momentum_tol_pct: dec!(0.03),
        size_usd: dec!(2.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
        strategy_sizes: std::collections::HashMap::new(),
    }
}

fn market(end_ms: i64) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond".into(),
        question_id: "q".into(),
        up_token_id: "up".into(),
        down_token_id: "down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end_ms,
        round_slot: 2,
        neg_risk: true,
        question: "BTC up or down".into(),
    }
}

/// A second market, so a cycle can host two strategies without them competing
/// for the same token (one entry per token per cycle is not waivable).
fn eth_market(end_ms: i64) -> CryptoMarket {
    CryptoMarket {
        asset: "ETH".into(),
        condition_id: "cond_eth".into(),
        question_id: "q_eth".into(),
        up_token_id: "eth-up".into(),
        down_token_id: "eth-down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end_ms,
        round_slot: 2,
        neg_risk: true,
        question: "ETH up or down".into(),
    }
}

#[test]
fn loads_and_drives_the_v2_dog_strategy_dylib() {
    let path = require_lib();
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    assert_eq!(loaded.name, "dog_strategy");
    assert_eq!(loaded.version, "0.2.0");

    let mut engine = Engine::new(engine_cfg());
    engine
        .register_user_strategy(
            Box::new(loaded.strategy),
            format!("dylib:{}", path.display()),
        )
        .unwrap();
    assert!(engine.set_strategy_enabled("dog_strategy", true));
    let expected_source = format!("dylib:{}", path.display());
    assert_eq!(
        engine.strategy_source("dog_strategy"),
        Some(expected_source.as_str())
    );

    let now = 1_800_000i64;
    engine.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });

    // Deep two-sided book, but mid 0.50 is ABOVE the 0.43 dip ceiling → idle.
    engine.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.49), dec!(60)), (dec!(0.48), dec!(60))],
        asks: vec![(dec!(0.51), dec!(60)), (dec!(0.52), dec!(60))],
        now_ms: now + 1_000,
    });
    assert!(engine.evaluate(now + 1_000).is_empty());
    // Two levels each side → the dylib confirms the token (proves full ladder).
    assert!(
        engine.confirmed_tokens().contains("up"),
        "{:?}",
        engine.confirmed_tokens()
    );
    let diag = engine.confirmed_diagnostics(now + 1_000);
    assert!(
        diag.iter()
            .any(|d| d.get("symbol").and_then(|s| s.as_str()) == Some("up")),
        "{diag:?}"
    );

    // Dip to mid 0.42 with >=50 bid depth → one BUY entry priced at best ask.
    engine.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.41), dec!(60)), (dec!(0.40), dec!(60))],
        asks: vec![(dec!(0.43), dec!(60)), (dec!(0.44), dec!(60))],
        now_ms: now + 2_000,
    });
    let orders = engine.evaluate(now + 2_000);
    assert_eq!(orders.len(), 1, "{orders:?}");
    let o = &orders[0];
    assert_eq!(o.strategy, "dog_strategy");
    assert_eq!(o.token_id, "up");
    assert_eq!(o.asset, "BTC");
    assert_eq!(o.direction, "up");
    assert_eq!(o.price, dec!(0.43));
    assert_eq!(o.size, dec!(10));
    assert!(o.internal_key.starts_with("dog_strategy:BTC:up:"));

    // Bid recovers to the 0.60 take-profit → the dylib emits a close intent.
    engine.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.60), dec!(60)), (dec!(0.59), dec!(60))],
        asks: vec![(dec!(0.62), dec!(60)), (dec!(0.63), dec!(60))],
        now_ms: now + 3_000,
    });
    let exits = {
        // evaluate() gathers the dylib's exit intents even with no new entries.
        assert!(engine.evaluate(now + 3_000).is_empty());
        engine.drain_strategy_exits()
    };
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(exits[0].token_id, "up");
    assert_eq!(exits[0].reason, "dog_tp");
    // Drained once.
    assert!(engine.drain_strategy_exits().is_empty());
}

#[test]
fn hot_params_reach_the_dylib_on_the_next_evaluation() {
    let path = require_lib();
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    let mut engine = Engine::new(engine_cfg());
    engine
        .register_user_strategy(Box::new(loaded.strategy), "dylib:hot".into())
        .unwrap();
    assert!(engine.set_strategy_enabled("dog_strategy", true));

    let now = 1_800_000i64;
    engine.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });
    // mid 0.47 > default ceiling 0.43 → no entry.
    engine.on_data(DataEvent::Book {
        token_id: "down".into(),
        bids: vec![(dec!(0.46), dec!(60))],
        asks: vec![(dec!(0.48), dec!(60))],
        now_ms: now + 1_000,
    });
    assert!(engine.evaluate(now + 1_000).is_empty());

    // Hot-swap the entry ceiling up to 0.50 → next evaluate must fire. The bag is
    // addressed by STRATEGY (E2-c) and carries this library's own knob name; the
    // strategy resolves only its own cell, so nothing leaks across namespaces.
    let mut p = blitzkrieg_core::shadow_evolution::StrategyParams::new();
    p.set("trendMaxEntryPrice", dec!(0.50));
    let registry = blitzkrieg_core::shadow_evolution::ParamRegistry::new();
    registry.publish("dog_strategy", p);
    engine.set_hot_params(Some(Arc::new(registry)));
    let orders = engine.evaluate(now + 2_000);
    assert_eq!(orders.len(), 1, "{orders:?}");
    assert_eq!(orders[0].token_id, "down");
    assert_eq!(orders[0].price, dec!(0.48));
}

/// An in-tree strategy that records the round timing exactly as the trait
/// delivers it, for the foreign/in-tree clock-parity assertion.
struct ClockRecorder {
    last: std::sync::Arc<std::sync::Mutex<Option<(i64, i64, i64)>>>,
}
impl blitzkrieg_core::strategies::EngineStrategy for ClockRecorder {
    fn name(&self) -> &str {
        "clock_recorder"
    }
    fn on_book(
        &mut self,
        _token_id: &str,
        _snap: &blitzkrieg_core::model::OrderbookSnapshot,
        _now_ms: i64,
    ) {
    }
    fn find_candidates(
        &mut self,
        _ctx: &blitzkrieg_core::strategies::StrategyCtx<'_>,
    ) -> Vec<blitzkrieg_core::signal::TradeSignal> {
        Vec::new()
    }
    fn on_round(&mut self, slot: i64, time_left_sec: i64, now_ms: i64) {
        *self.last.lock().unwrap() = Some((slot, time_left_sec, now_ms));
    }
}

#[test]
fn the_dylib_sees_the_same_round_clock_as_an_in_tree_strategy() {
    // E-parity, verifiable not aspirational: on ONE engine, the foreign dylib's
    // on_round timing (reported back through its diagnostics) and an in-tree
    // strategy's on_round timing must be EQUAL bit for bit — same slot, same
    // time_left_sec, same now_ms. Both must equal the scanner's computation.
    let path = require_lib();
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    let mut engine = Engine::new(engine_cfg());
    engine
        .register_user_strategy(
            Box::new(loaded.strategy),
            format!("dylib:{}", path.display()),
        )
        .unwrap();
    let in_tree_clock = std::sync::Arc::new(std::sync::Mutex::new(None));
    engine
        .register_user_strategy(
            Box::new(ClockRecorder {
                last: std::sync::Arc::clone(&in_tree_clock),
            }),
            "in-tree:clock".into(),
        )
        .unwrap();
    assert!(engine.set_strategy_enabled("dog_strategy", true));
    assert!(engine.set_strategy_enabled("clock_recorder", true));

    // An expiry that leaves a real, non-zero time budget: the scanner computes
    // time_left_sec from the round state, not zeros.
    let now = 1_795_000i64;
    engine.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });

    let diag = engine.confirmed_diagnostics(now);
    let foreign_clock = diag
        .iter()
        .find_map(|d| d.get("roundClock"))
        .unwrap_or_else(|| panic!("dylib must report its round clock: {diag:?}"));
    let (slot, time_left, now_seen) = (
        foreign_clock.get("slot").and_then(|v| v.as_i64()),
        foreign_clock.get("timeLeftSec").and_then(|v| v.as_i64()),
        foreign_clock.get("nowMs").and_then(|v| v.as_i64()),
    );
    assert_eq!(slot, Some(2), "{foreign_clock}");
    assert_eq!(now_seen, Some(now), "{foreign_clock}");
    assert_eq!(
        time_left,
        Some(5),
        "the dylib must see the scanner's REAL time budget, not zeros: {foreign_clock}"
    );
    // The in-tree recorder's on_round clock must match the dylib's EXACTLY.
    assert_eq!(
        *in_tree_clock.lock().unwrap(),
        Some((slot.unwrap(), time_left.unwrap(), now_seen.unwrap())),
        "in-tree and dylib round clocks must be bit-identical"
    );
}

#[test]
fn the_dylib_declares_its_evolvable_knobs_over_the_optional_symbol() {
    // E2-c (#28): `bk_strategy_evolvable_knobs` is OPTIONAL — its absence means
    // "not evolvable", explicitly, which is why adding it needs no ABI bump. When
    // present, the load report carries the knobs AND the domain the kernel
    // enforces, and `on_hot_params` receives exactly this bag serialized.
    let path = require_lib();
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    let knobs = &loaded.evolvable_knobs;
    assert_eq!(knobs.len(), 1, "the dog strategy declares exactly one knob");
    assert_eq!(knobs[0].name, "trendMaxEntryPrice");
    assert_eq!(knobs[0].value, dec!(0.43));
    assert!(
        knobs[0].contains(dec!(0.50)),
        "the kernel's override must be inside the domain"
    );
    assert!(
        !knobs[0].contains(dec!(0.99)),
        "and an out-of-domain value must be rejectable"
    );
}

#[test]
fn the_dylib_sees_only_priceable_books_in_its_eval_ctx() {
    // E7 (#38): the OPTIONAL `bk_strategy_bind_eval_ctx` symbol gives a foreign
    // library the SAME freshness gate an in-tree strategy gets from
    // `StrategyCtx::fresh_book`. The kernel binds round view + PRICEABLE books
    // (non-empty, within the staleness budget) around evaluate and diagnostics;
    // a token without a bound row is "not priceable now", with stale and
    // missing indistinguishable — exactly the trait-side semantics.
    let path = require_lib();
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    let mut engine = Engine::new(engine_cfg());
    engine
        .register_user_strategy(
            Box::new(loaded.strategy),
            format!("dylib:{}", path.display()),
        )
        .unwrap();
    assert!(engine.set_strategy_enabled("dog_strategy", true));

    let now = 1_800_000i64;
    engine.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000)],
        now_ms: now,
    });

    // The dog strategy gates entries on book data gathered via on_book and
    // still sees the round context; the binder must therefore leave its
    // decisions unchanged (parity = no behavioural difference), and a STALE
    // book must disappear from the bound context like an in-tree fresh_book
    // would drop it.
    engine.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.41), dec!(60)), (dec!(0.40), dec!(60))],
        asks: vec![(dec!(0.43), dec!(60)), (dec!(0.44), dec!(60))],
        now_ms: now + 1_000,
    });
    let orders = engine.evaluate(now + 2_000);
    assert_eq!(
        orders.len(),
        1,
        "fresh book: entry fires as before {orders:?}"
    );
    assert_eq!(orders[0].price, dec!(0.43));
}

#[test]
fn the_dylib_reports_its_config_view_over_the_optional_symbol() {
    // E-parity: the OPTIONAL `bk_strategy_config_view` symbol lets ANY library
    // report the config currently in force — the generalization of the in-tree
    // `spread_arb_view`. The dog library declares no config view (null =
    // "nothing declared"); the parity library does, and its report must track
    // a hot parameter actually applied.
    let path = require_lib();
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    let mut engine = Engine::new(engine_cfg());
    engine
        .register_user_strategy(
            Box::new(loaded.strategy),
            format!("dylib:{}", path.display()),
        )
        .unwrap();
    assert!(engine.set_strategy_enabled("dog_strategy", true));

    // The dog library exports no config view: the engine reports nothing.
    assert!(
        engine.strategy_config_views().is_empty(),
        "{:?}",
        engine.strategy_config_views()
    );
}

/// A minimal in-tree strategy that declares NOTHING (no gate exemption), so the
/// gate test has a "fully gated" counterpart to the dylib's `timing` declaration.
/// The kernel ships no strategy itself (PR-B), so the counterpart is defined
/// here, in the test. It fires on the token it is handed (DOWN side of the
/// ETH market, which the dip-buying dylib ignores), so its candidate is a
/// pure probe of the shared timing gate.
struct PlainDip {
    token: &'static str,
    asset: &'static str,
}

impl blitzkrieg_core::strategies::EngineStrategy for PlainDip {
    fn name(&self) -> &str {
        "plain_dip"
    }
    fn on_book(
        &mut self,
        _token_id: &str,
        _snap: &blitzkrieg_core::model::OrderbookSnapshot,
        _now_ms: i64,
    ) {
    }
    fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
    fn find_candidates(
        &mut self,
        ctx: &blitzkrieg_core::strategies::StrategyCtx<'_>,
    ) -> Vec<blitzkrieg_core::signal::TradeSignal> {
        let Some(book) = ctx.fresh_book(self.token) else {
            return Vec::new();
        };
        vec![blitzkrieg_core::signal::TradeSignal {
            strategy: self.name().to_string(),
            asset: self.asset.to_string(),
            direction: blitzkrieg_core::model::SignalDirection::Down,
            token_id: self.token.to_string(),
            condition_id: "cond".into(),
            price: book.best_bid,
            reason: "plain probe".into(),
        }]
    }
}

#[test]
fn the_dylib_gate_declaration_reaches_the_engine_and_is_honoured() {
    // E2-b (#27): the OPTIONAL `bk_strategy_gate_exemptions` symbol crosses the
    // C ABI. The dog strategy declares `timing` — so a round whose timing window
    // is shut still lets ITS entry through, while a strategy that declares
    // nothing stays blocked. External is only a loading difference.
    let path = require_lib();
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    assert_eq!(
        loaded.gate_exemptions,
        blitzkrieg_core::strategies::GateExemptions {
            timing: true,
            momentum: false
        },
        "the load report must carry the library's declaration"
    );

    let mut cfg = engine_cfg();
    // Shut the timing window for every strategy that did not declare it.
    cfg.scanner.min_round_age_sec = 10_000;
    let mut engine = Engine::new(cfg);
    engine
        .register_user_strategy(
            Box::new(loaded.strategy),
            format!("dylib:{}", path.display()),
        )
        .unwrap();
    // The non-declaring counterpart trades the OTHER token (eth-down, which
    // the dip buyer ignores at mid 0.55).
    engine
        .register_user_strategy(
            Box::new(PlainDip {
                token: "eth-down",
                asset: "ETH",
            }),
            "test".into(),
        )
        .unwrap();
    assert!(engine.set_strategy_enabled("dog_strategy", true));
    assert!(engine.set_strategy_enabled("plain_dip", true));
    assert_eq!(
        engine.strategy_gate_exemptions("dog_strategy"),
        Some(blitzkrieg_core::strategies::GateExemptions {
            timing: true,
            momentum: false
        })
    );
    assert_eq!(
        engine.strategy_gate_exemptions("plain_dip"),
        Some(blitzkrieg_core::strategies::GateExemptions::none()),
        "a strategy that declares nothing is fully gated"
    );

    let now = 1_800_000i64;
    // Two markets: BTC for the plain strategy, ETH for the dylib.
    engine.on_data(DataEvent::RoundMarkets {
        markets: vec![market(1_800_000), eth_market(1_800_000)],
        now_ms: now,
    });
    // A dip deep enough for both ceilings with >= 50 bid depth.
    for token in ["up", "eth-up"] {
        engine.on_data(DataEvent::Book {
            token_id: token.into(),
            bids: vec![(dec!(0.41), dec!(60)), (dec!(0.40), dec!(60))],
            asks: vec![(dec!(0.43), dec!(60)), (dec!(0.44), dec!(60))],
            now_ms: now + 11_000,
        });
    }
    // eth-down at mid 0.55 (above the dylib's 0.43 buy ceiling, so dog ignores
    // it; plain_dip emits a candidate that the shut timing gate blocks).
    engine.on_data(DataEvent::Book {
        token_id: "eth-down".into(),
        bids: vec![(dec!(0.54), dec!(60))],
        asks: vec![(dec!(0.56), dec!(60))],
        now_ms: now + 11_000,
    });

    let orders = engine.evaluate(now + 12_000);
    // The declaring dylib enters on BOTH markets (it is asset-agnostic and
    // exempt from the shut timing window); the non-declaring strategy gets no
    // order through anywhere.
    assert_eq!(
        orders.len(),
        2,
        "only the declaring strategy may enter: {orders:?}"
    );
    assert!(
        orders.iter().all(|o| o.strategy == "dog_strategy"),
        "only the declaring strategy may enter: {orders:?}"
    );
    assert_eq!(
        orders[0].asset, "BTC",
        "the dylib's BTC entry went through the shut window under its declaration"
    );
    assert_eq!(
        orders[1].asset, "ETH",
        "the dylib's ETH entry went through the shut window under its declaration"
    );
    assert!(
        engine
            .last_blocked()
            .iter()
            .any(|b| b.strategy == "plain_dip"),
        "the non-declaring strategy stays gated"
    );

    let ex = engine.last_exemptions();
    assert_eq!(ex.len(), 2, "{ex:?}");
    assert!(
        ex.iter()
            .all(|e| e.strategy == "dog_strategy" && e.gate == "timing")
    );
    assert!(ex.iter().any(|e| {
        e.audit_line()
            .contains("本单因策略 dog_strategy 豁免门禁 timing")
    }));
}

#[test]
fn policy_rejects_bad_paths() {
    use blitzkrieg_core::strategy_engine::loader::policy_allows;
    assert!(policy_allows(Path::new("user_layer/strategies/secret_key.dylib")).is_err());
    assert!(policy_allows(Path::new("user_layer/strategies/strategy.toml")).is_err());
    assert!(policy_allows(Path::new("strategies/dog_strategy.dylib")).is_ok());
}

#[test]
fn a_library_without_the_version_symbol_fails_negotiation() {
    // A real shared object that is NOT a strategy: libc exports plenty but never
    // bk_strategy_abi_version, so after dlopen it is rejected as v1/pre-v2
    // rather than misread. The on-disk name is `libc.so.6`, which the path
    // policy would reject on extension; copy it to a `.so` temp path so the test
    // actually exercises the VERSION negotiation step. Best-effort: skip on
    // hosts without such a file (e.g. macOS, whose system libs are not plain .so).
    // A real shared object that is NOT a strategy: it exports plenty but never
    // bk_strategy_abi_version, so after dlopen it is rejected as v1/pre-v2
    // rather than misread. On Linux the on-disk name is `libc.so.6`, which the
    // extension policy would reject, so it is copied to a `.so` temp path; on
    // macOS libSystem is already a `.dylib` and passes the policy as-is.
    let candidates: &[(&str, bool)] = &[
        ("/usr/lib/x86_64-linux-gnu/libc.so.6", true),
        ("/usr/lib/libc.so.6", true),
        ("/lib/x86_64-linux-gnu/libc.so.6", true),
        ("/usr/lib/aarch64-linux-gnu/libc.so.6", true),
        ("/usr/lib/libSystem.B.dylib", false),
        ("/usr/lib/libSystem.dylib", false),
    ];
    let Some((src, needs_copy)) = candidates.iter().find(|(c, _)| Path::new(c).exists()) else {
        eprintln!("skipping: no non-strategy shared library on this host");
        return;
    };
    let tmp;
    let target = if *needs_copy {
        tmp = std::env::temp_dir().join(format!(
            "bk_non_strategy_{}_{}.so",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::copy(src, &tmp).expect("copy system lib to temp .so");
        tmp.as_path()
    } else {
        Path::new(src)
    };

    use blitzkrieg_core::strategy_engine::loader::LoadOutcome;
    let result = load_foreign(target);
    if *needs_copy {
        let _ = std::fs::remove_file(target);
    }
    let msg = match result {
        Err(LoadOutcome::Failed { reason, .. }) => reason,
        Err(LoadOutcome::Rejected { reason, .. }) => {
            panic!("expected a negotiation failure, policy rejected first: {reason}")
        }
        Err(LoadOutcome::Loaded { name, .. }) => panic!("impossible load outcome for {name}"),
        Ok(_) => panic!("non-strategy shared object must fail negotiation"),
    };
    assert!(
        msg.contains("abi_version") || msg.contains("v1"),
        "unexpected: {msg}"
    );
}

// ── Bit-equivalence parity: 3 shipped cdylibs vs Test Adapters ──────────────

#[test]
fn parity_spread_arb_cdylib_matches_adapter() {
    let path = require_named_lib("spread_arb_strategy");
    let loaded = load_foreign(&path).expect("load spread_arb_strategy");
    assert_eq!(loaded.name, "spread_arb");
    assert_eq!(
        loaded.gate_exemptions,
        blitzkrieg_core::strategies::GateExemptions::none()
    );

    let trend = TrendConfig {
        confirm_sec: 5,
        ratio: dec!(0.5),
        min_price: dec!(0.5),
        broken_price: dec!(0.35),
        window_floor_ms: 0,
    };
    let spread = SpreadArbConfig {
        trend_min_price: dec!(0.50),
        trend_confirm_sec: 5,
        trend_broken_price: dec!(0.35),
        trend_entry_price: dec!(0.45),
        trend_entry_factor: dec!(0.9),
        trend_max_entry_price: dec!(0.45),
    };

    let mut cfg = engine_cfg();
    cfg.trend = trend.clone();
    cfg.spread_arb = spread.clone();

    let mut engine_adapter = Engine::new(cfg.clone());
    engine_adapter
        .register_user_strategy(
            Box::new(
                blitzkrieg_core::strategies::test_support::TestSpreadArb::new(
                    trend.clone(),
                    spread.clone(),
                ),
            ),
            "test:adapter".into(),
        )
        .unwrap();
    assert!(engine_adapter.set_strategy_enabled("spread_arb", true));

    let mut engine_dylib = Engine::new(cfg);
    engine_dylib
        .register_user_strategy(Box::new(loaded.strategy), "dylib:spread_arb".into())
        .unwrap();
    assert!(engine_dylib.set_strategy_enabled("spread_arb", true));

    let now = 1_800_000i64;
    for eng in [&mut engine_adapter, &mut engine_dylib] {
        eng.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // Confirm UP trend: 12 ticks of mid >= 0.50
        for i in 0..12 {
            eng.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1_000,
            });
        }
    }

    assert_eq!(
        engine_adapter.confirmed_tokens(),
        engine_dylib.confirmed_tokens()
    );
    assert_eq!(
        engine_adapter.confirmed_diagnostics(now + 12_000),
        engine_dylib.confirmed_diagnostics(now + 12_000)
    );

    // Dip to 0.42: triggers entry for both
    for eng in [&mut engine_adapter, &mut engine_dylib] {
        eng.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.41), dec!(100))],
            asks: vec![(dec!(0.43), dec!(100))],
            now_ms: now + 13_000,
        });
    }
    let orders_a = engine_adapter.evaluate(now + 13_000);
    let orders_d = engine_dylib.evaluate(now + 13_000);
    assert_eq!(orders_a.len(), 1);
    assert_eq!(orders_d.len(), 1);
    assert_eq!(orders_a[0].token_id, orders_d[0].token_id);
    assert_eq!(orders_a[0].price, orders_d[0].price);
    assert_eq!(orders_a[0].direction, orders_d[0].direction);
    assert_eq!(orders_a[0].strategy, orders_d[0].strategy);

    // Break: mid drops to 0.30 (< 0.35 broken_price)
    for eng in [&mut engine_adapter, &mut engine_dylib] {
        let breaks = eng.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.28), dec!(100))],
            asks: vec![(dec!(0.30), dec!(100))],
            now_ms: now + 14_000,
        });
        assert_eq!(breaks.len(), 1);
        assert_eq!(breaks[0].0, "up");
    }

    // Config view parity
    assert_eq!(
        engine_adapter.strategy_config_views(),
        engine_dylib.strategy_config_views()
    );
}

#[test]
fn parity_trend_follow_cdylib_matches_adapter() {
    let path = require_named_lib("trend_follow_strategy");
    let loaded = load_foreign(&path).expect("load trend_follow_strategy");
    assert_eq!(loaded.name, "trend_follow");
    assert_eq!(
        loaded.gate_exemptions,
        blitzkrieg_core::strategies::GateExemptions::none()
    );

    let cfg = strategy_logic::TrendFollowConfig::default();

    let mut engine_adapter = Engine::new(engine_cfg());
    engine_adapter
        .register_user_strategy(
            Box::new(blitzkrieg_core::strategies::test_support::TestTrendFollow::new(cfg.clone())),
            "test:adapter".into(),
        )
        .unwrap();
    assert!(engine_adapter.set_strategy_enabled("trend_follow", true));

    let mut engine_dylib = Engine::new(engine_cfg());
    engine_dylib
        .register_user_strategy(Box::new(loaded.strategy), "dylib:trend_follow".into())
        .unwrap();
    assert!(engine_dylib.set_strategy_enabled("trend_follow", true));

    let now = 1_800_000i64;
    for eng in [&mut engine_adapter, &mut engine_dylib] {
        eng.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // Initial price: 0.56 (spread 0.01: 0.56 / 0.57, spread_pct ~1.77% <= 3.0%)
        eng.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.56), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now,
        });
        // Move up to 0.60 / 0.61 over 10s: mid goes from 0.565 to 0.605 (> min_move_pct 3.0%, >= min_confirm_price 0.55)
        eng.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.60), dec!(100))],
            asks: vec![(dec!(0.61), dec!(100))],
            now_ms: now + 10_000,
        });
    }

    assert_eq!(
        engine_adapter.confirmed_tokens(),
        engine_dylib.confirmed_tokens()
    );
    assert_eq!(
        engine_adapter.confirmed_diagnostics(now + 10_000),
        engine_dylib.confirmed_diagnostics(now + 10_000)
    );

    let orders_a = engine_adapter.evaluate(now + 10_000);
    let orders_d = engine_dylib.evaluate(now + 10_000);
    assert_eq!(orders_a.len(), 1);
    assert_eq!(orders_d.len(), 1);
    assert_eq!(orders_a[0].token_id, orders_d[0].token_id);
    assert_eq!(orders_a[0].price, orders_d[0].price);
    assert_eq!(orders_a[0].direction, orders_d[0].direction);
    assert_eq!(orders_a[0].strategy, orders_d[0].strategy);

    // Break: mid drops to 0.40 (< 0.45 break_price)
    for eng in [&mut engine_adapter, &mut engine_dylib] {
        let breaks = eng.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.39), dec!(100))],
            asks: vec![(dec!(0.41), dec!(100))],
            now_ms: now + 14_000,
        });
        assert_eq!(breaks.len(), 1);
        assert_eq!(breaks[0].0, "up");
    }

    // Config view parity
    assert_eq!(
        engine_adapter.strategy_config_views(),
        engine_dylib.strategy_config_views()
    );
}

#[test]
fn parity_mean_reversion_cdylib_matches_adapter() {
    let path = require_named_lib("mean_reversion_strategy");
    let loaded = load_foreign(&path).expect("load mean_reversion_strategy");
    assert_eq!(loaded.name, "mean_reversion");
    assert_eq!(
        loaded.gate_exemptions,
        blitzkrieg_core::strategies::GateExemptions {
            timing: false,
            momentum: true,
        }
    );

    let cfg = strategy_logic::MeanReversionConfig::default();

    let mut engine_adapter = Engine::new(engine_cfg());
    engine_adapter
        .register_user_strategy(
            Box::new(
                blitzkrieg_core::strategies::test_support::TestMeanReversion::new(cfg.clone()),
            ),
            "test:adapter".into(),
        )
        .unwrap();
    assert!(engine_adapter.set_strategy_enabled("mean_reversion", true));

    let mut engine_dylib = Engine::new(engine_cfg());
    engine_dylib
        .register_user_strategy(Box::new(loaded.strategy), "dylib:mean_reversion".into())
        .unwrap();
    assert!(engine_dylib.set_strategy_enabled("mean_reversion", true));

    let now = 1_800_000i64;
    for eng in [&mut engine_adapter, &mut engine_dylib] {
        eng.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // High point: 0.50 (spread 0.02: 0.49 / 0.51, spread_pct 4% <= 8%)
        eng.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.49), dec!(100))],
            asks: vec![(dec!(0.51), dec!(100))],
            now_ms: now,
        });
        // Crash to 0.30: drop is (0.30 - 0.50)/0.50 = -40% <= -10%, and mid 0.30 <= max_price 0.35
        // spread 0.02: 0.29 / 0.31, spread_pct 6.67% <= 8%
        eng.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.29), dec!(100))],
            asks: vec![(dec!(0.31), dec!(100))],
            now_ms: now + 5_000,
        });
    }

    assert_eq!(
        engine_adapter.confirmed_tokens(),
        engine_dylib.confirmed_tokens()
    );
    assert_eq!(
        engine_adapter.confirmed_diagnostics(now + 5_000),
        engine_dylib.confirmed_diagnostics(now + 5_000)
    );

    let orders_a = engine_adapter.evaluate(now + 5_000);
    let orders_d = engine_dylib.evaluate(now + 5_000);
    assert_eq!(orders_a.len(), 1);
    assert_eq!(orders_d.len(), 1);
    assert_eq!(orders_a[0].token_id, orders_d[0].token_id);
    assert_eq!(orders_a[0].price, orders_d[0].price);
    assert_eq!(orders_a[0].direction, orders_d[0].direction);
    assert_eq!(orders_a[0].strategy, orders_d[0].strategy);

    // Premise break: price recovers back above cheap zone (> max_price 0.35)
    for eng in [&mut engine_adapter, &mut engine_dylib] {
        let breaks = eng.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.39), dec!(100))],
            asks: vec![(dec!(0.41), dec!(100))],
            now_ms: now + 10_000,
        });
        assert_eq!(breaks.len(), 1);
        assert_eq!(breaks[0].0, "up");
    }

    // Config view parity
    assert_eq!(
        engine_adapter.strategy_config_views(),
        engine_dylib.strategy_config_views()
    );
}
