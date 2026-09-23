//! E7 parity harness (issue #38 acceptance): the SAME deterministic strategy
//! algorithm (`parity_logic`) compiled in-tree as a full `EngineStrategy` and
//! out-of-tree behind C ABI v2 (`parity_strategy` cdylib) must behave
//! signal-for-signal identically on an identical replay.
//!
//! Two fresh [`Engine`]s are driven through the same `DataEvent` sequence and
//! compared per evaluation cycle on:
//!   - entry `OrderRequest`s (token/price/size/strategy/asset/direction/key),
//!   - drained close intents (token/reason),
//!   - trend breaks surfaced by `on_data` (token/price),
//!   - confirmed-token sets,
//!   - canonicalized per-strategy diagnostics JSON,
//!   - round-reset behaviour (the slot in the internal key changes).
//!
//! Build first (CI does): (cd user_layer/parity_strategy && cargo build --release)
//! `BK_REQUIRE_DYLIB=1` makes a missing cdylib a hard failure instead of a skip.
#![cfg(feature = "strategy-loading")]

use blitzkrieg_core::engine::{DataEvent, Engine, EngineConfig};
use blitzkrieg_core::model::{CryptoMarket, OrderRequest, OrderbookSnapshot, SignalDirection};
use blitzkrieg_core::scanner::ScannerConfig;
use blitzkrieg_core::shadow_evolution::{ParamRegistry, StrategyParams};
use blitzkrieg_core::signal::{SpreadArbConfig, TradeSignal, TrendConfig};
use blitzkrieg_core::strategies::{EngineStrategy, StrategyCtx, StrategyExitIntent};
use blitzkrieg_core::strategy_engine::loader::load_foreign;
use parity_logic::{PBook, PLevel, PMarket, ParityStrategy};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

fn dylib_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("user_layer")
        .join("parity_strategy")
        .join("target")
        .join("release")
}

fn parity_lib() -> PathBuf {
    let base = dylib_dir();
    for name in [
        "libparity_strategy.dylib",
        "libparity_strategy.so",
        "parity_strategy.dll",
    ] {
        let p = base.join(name);
        if p.exists() {
            return p;
        }
    }
    let msg =
        "build the parity cdylib first: (cd user_layer/parity_strategy && cargo build --release)";
    if std::env::var_os("BK_REQUIRE_DYLIB").is_some() {
        panic!("{msg} (looked in {})", base.display());
    }
    eprintln!("skipping: {msg}");
    std::process::exit(0);
}

fn engine_cfg() -> EngineConfig {
    EngineConfig {
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
        max_orderbook_stale_ms: 8000,
        momentum_window_sec: 30,
        momentum_tol_pct: dec!(0.03),
        size_usd: dec!(2.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
        size_pct: dec!(0),
        strategy_sizes: std::collections::HashMap::new(),
    }
}

fn market(end_ms: i64, slot: i64) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: "cond".into(),
        question_id: "q".into(),
        up_token_id: "up".into(),
        down_token_id: "down".into(),
        up_price: dec!(0.6),
        down_price: dec!(0.4),
        expires_at_ms: end_ms,
        round_slot: slot,
        neg_risk: true,
        question: "BTC up or down".into(),
    }
}

/// A registry holding the parity strategy's own hot values, exactly as the
/// evolution manager would publish them (E2-c): addressed by strategy name, so
/// the in-tree side and the dylib side are fed by the same shape the kernel uses.
fn hot_registry(cap: Decimal) -> ParamRegistry {
    let mut p = StrategyParams::new();
    p.set("trendMaxEntryPrice", cap);
    p.set("exitAbove", dec!(0.60));
    let r = ParamRegistry::new();
    r.publish("parity", p);
    r
}

/// In-tree twin of the external parity cdylib: the SAME algorithm crate behind
/// the SAME full EngineStrategy contract, only the loading differs.
struct InTreeParity {
    inner: ParityStrategy,
    exits: Vec<StrategyExitIntent>,
    breaks: Vec<(String, Decimal)>,
    /// This strategy's OWN cell in the per-strategy registry (E2-c): a parity
    /// instance reads only its own namespace, never another strategy's.
    hot: Option<Arc<arc_swap::ArcSwap<StrategyParams>>>,
    last_hot: Option<String>,
}

impl InTreeParity {
    fn new() -> Self {
        Self {
            inner: ParityStrategy::new("parity"),
            exits: Vec::new(),
            breaks: Vec::new(),
            hot: None,
            last_hot: None,
        }
    }

    fn push_hot_if_changed(&mut self) {
        let Some(h) = &self.hot else { return };
        // Same no-float JSON the kernel hands the dylib (foreign.rs): this
        // strategy's own knob bag, serialized as decimal strings.
        let json = serde_json::to_string(&**h.load()).unwrap_or_default();
        if self.last_hot.as_deref() == Some(json.as_str()) {
            return;
        }
        self.inner.apply_hot_json(&json);
        self.last_hot = Some(json);
    }
}

fn snapshot_to_pbook(token: &str, s: &OrderbookSnapshot) -> PBook {
    let levels = |v: &[(Decimal, Decimal)]| {
        v.iter()
            .map(|(p, sz)| PLevel {
                price: p.to_string(),
                size: sz.to_string(),
            })
            .collect()
    };
    PBook {
        symbol: token.to_string(),
        bids: levels(&s.bids),
        asks: levels(&s.asks),
        best_bid: s.best_bid.to_string(),
        best_ask: s.best_ask.to_string(),
        mid: s.mid_price.to_string(),
        bid_depth: s.bid_depth.to_string(),
        ask_depth: s.ask_depth.to_string(),
        obi: s.obi.to_string(),
        spread: s.spread.to_string(),
        spread_pct: s.spread_pct.to_string(),
    }
}

impl EngineStrategy for InTreeParity {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, _now_ms: i64) {
        self.inner
            .observe(token_id, snapshot_to_pbook(token_id, snap));
    }

    fn on_round(&mut self, slot: i64, _time_left_sec: i64, _now_ms: i64) {
        self.inner.reset_round(slot);
    }

    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        std::mem::take(&mut self.breaks)
    }

    fn confirmed_tokens(&self) -> HashSet<String> {
        self.inner.confirmed_tokens().into_iter().collect()
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        self.push_hot_if_changed();
        let markets: Vec<PMarket> = ctx
            .markets()
            .iter()
            .map(|m| PMarket {
                up_token: m.up_token_id.clone(),
                down_token: m.down_token_id.clone(),
            })
            .collect();
        let d = self.inner.evaluate(&markets);
        for b in d.breaks {
            let price = Decimal::from_str(&b.broken_price).unwrap_or(Decimal::ZERO);
            self.breaks.push((b.token, price));
        }
        for e in d.exits {
            self.exits.push(StrategyExitIntent {
                token_id: e.token,
                reason: e.reason,
            });
        }
        let mut out = Vec::new();
        for e in d.entries {
            let Some(m) = ctx
                .markets()
                .iter()
                .find(|m| m.up_token_id == e.token || m.down_token_id == e.token)
            else {
                continue;
            };
            let (direction, token) = if m.up_token_id == e.token {
                (SignalDirection::Up, m.up_token_id.clone())
            } else {
                (SignalDirection::Down, m.down_token_id.clone())
            };
            out.push(TradeSignal {
                strategy: self.name().to_string(),
                asset: m.asset.clone(),
                direction,
                token_id: token,
                condition_id: m.condition_id.clone(),
                price: Decimal::from_str(&e.price).expect("parity price"),
                reason: e.reason,
                shares: None,
            });
        }
        out
    }

    fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent> {
        std::mem::take(&mut self.exits)
    }

    fn diagnostics(&self, _ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        self.inner.diagnostics()
    }

    fn set_hot_params(&mut self, registry: Option<Arc<ParamRegistry>>) {
        self.last_hot = None;
        // The parity strategy declares `trendMaxEntryPrice` / `exitAbove`, so its
        // own cell exists; a strategy that declared nothing would get `None`.
        // `None` detaches the overlay (evolution disabled).
        self.hot = registry.as_ref().and_then(|r| r.handle_for(self.name()));
    }

    /// The config currently in force (E-parity): the in-tree side reports the
    /// same JSON the cdylib's `bk_strategy_config_view` symbol serializes.
    fn config_view_json(&self) -> Option<String> {
        Some(self.inner.config_view().to_string())
    }
}

enum Step {
    Data(DataEvent),
    Eval(i64),
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CycleSnapshot {
    /// Normalized entry orders, in emission order.
    entries: Vec<(String, String, String, String, String, String, String)>,
    exits: Vec<(String, String)>,
    confirmed: Vec<String>,
    diagnostics: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RunResult {
    /// Breaks returned by on_data, in arrival order: (token, price string).
    breaks: Vec<(String, String)>,
    cycles: Vec<CycleSnapshot>,
}

fn entry_key(o: &OrderRequest) -> (String, String, String, String, String, String, String) {
    (
        o.token_id.clone(),
        o.price.to_string(),
        o.size.to_string(),
        o.strategy.clone(),
        o.asset.clone(),
        o.direction.clone(),
        o.internal_key.clone(),
    )
}

fn run_replay(steps: &[Step], engine: &mut Engine) -> RunResult {
    let mut out = RunResult::default();
    for step in steps {
        match step {
            Step::Data(ev) => {
                for (token, price) in engine.on_data(ev.clone()) {
                    out.breaks.push((token, price.to_string()));
                }
            }
            Step::Eval(now) => {
                let orders = engine.evaluate(*now);
                for o in &orders {
                    engine.note_order_placed(&o.token_id);
                }
                let exits = engine
                    .drain_strategy_exits()
                    .into_iter()
                    .map(|e| (e.token_id, e.reason))
                    .collect();
                let mut confirmed: Vec<String> = engine.confirmed_tokens().into_iter().collect();
                confirmed.sort();
                let mut diagnostics: Vec<String> = engine
                    .confirmed_diagnostics(*now)
                    .iter()
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .collect();
                diagnostics.sort();
                out.cycles.push(CycleSnapshot {
                    entries: orders.iter().map(entry_key).collect(),
                    exits,
                    confirmed,
                    diagnostics,
                });
            }
        }
    }
    out
}

fn replay() -> Vec<Step> {
    let t0 = 1_000_000i64;
    vec![
        // Round 1: clock slot at t0=1_000_000 is 1 (boundary at 1_800_000).
        Step::Data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000, 1)],
            now_ms: t0,
        }),
        // Deep neutral book above the 0.50 hot ceiling → no entry; confirmed.
        Step::Data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![
                (dec!(0.51), dec!(60)),
                (dec!(0.50), dec!(60)),
                (dec!(0.49), dec!(60)),
            ],
            asks: vec![
                (dec!(0.53), dec!(60)),
                (dec!(0.54), dec!(60)),
                (dec!(0.55), dec!(60)),
            ],
            now_ms: t0 + 1_000,
        }),
        Step::Eval(t0 + 1_000),
        // Dip: mid 0.42, deep two-sided → one BUY at the 0.43 best ask.
        Step::Data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![
                (dec!(0.41), dec!(100)),
                (dec!(0.40), dec!(100)),
                (dec!(0.39), dec!(100)),
            ],
            asks: vec![
                (dec!(0.43), dec!(100)),
                (dec!(0.44), dec!(100)),
                (dec!(0.45), dec!(100)),
            ],
            now_ms: t0 + 2_000,
        }),
        Step::Eval(t0 + 2_000),
        // Same book re-evaluated: no repeat entry (strategy state machine).
        Step::Eval(t0 + 2_500),
        // DOWN dips too but the book is shallow → blocked, and not confirmed.
        Step::Data(DataEvent::Book {
            token_id: "down".into(),
            bids: vec![(dec!(0.40), dec!(10))],
            asks: vec![(dec!(0.42), dec!(100)), (dec!(0.43), dec!(100))],
            now_ms: t0 + 3_000,
        }),
        Step::Eval(t0 + 3_000),
        // UP collapses through the 0.30 break level (still in position).
        Step::Data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.28), dec!(100)), (dec!(0.27), dec!(100))],
            asks: vec![(dec!(0.30), dec!(100)), (dec!(0.31), dec!(100))],
            now_ms: t0 + 4_000,
        }),
        Step::Eval(t0 + 4_000),
        // The break is delivered to the host on the next data callback; a
        // repeat collapse must not double-fire it.
        Step::Data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.28), dec!(100)), (dec!(0.27), dec!(100))],
            asks: vec![(dec!(0.30), dec!(100)), (dec!(0.31), dec!(100))],
            now_ms: t0 + 4_100,
        }),
        // Recovery while in position → take-profit close intent.
        Step::Data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.61), dec!(100)), (dec!(0.60), dec!(100))],
            asks: vec![(dec!(0.63), dec!(100)), (dec!(0.64), dec!(100))],
            now_ms: t0 + 5_000,
        }),
        Step::Eval(t0 + 5_000),
        // Round 2 (clock slot 2): on_round resets per-round state; the slot
        // embedded in the internal key must change.
        Step::Data(DataEvent::RoundMarkets {
            markets: vec![market(2_700_000, 2)],
            now_ms: t0 + 900_000,
        }),
        Step::Data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.41), dec!(100)), (dec!(0.40), dec!(100))],
            asks: vec![(dec!(0.43), dec!(100)), (dec!(0.44), dec!(100))],
            now_ms: t0 + 901_000,
        }),
        Step::Data(DataEvent::Book {
            token_id: "down".into(),
            bids: vec![(dec!(0.43), dec!(100)), (dec!(0.42), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100)), (dec!(0.46), dec!(100))],
            now_ms: t0 + 901_000,
        }),
        Step::Eval(t0 + 901_000),
    ]
}

#[test]
fn in_tree_and_dylib_parity_match_signal_for_signal() {
    let path = parity_lib();
    let steps = replay();

    // ── Side A: in-tree EngineStrategy over parity_logic ─────────────────────
    let mut in_tree = Engine::new(engine_cfg());
    in_tree
        .register_user_strategy(Box::new(InTreeParity::new()), "in-tree:parity_logic".into())
        .unwrap();
    assert!(in_tree.set_strategy_enabled("parity", true));
    in_tree.set_hot_params(Some(Arc::new(hot_registry(dec!(0.50)))));
    let a = run_replay(&steps, &mut in_tree);

    // ── Side B: the external parity_strategy cdylib via C ABI v2 ─────────────
    let loaded =
        load_foreign(&path).unwrap_or_else(|e| panic!("load {} failed: {e:?}", path.display()));
    assert_eq!(loaded.name, "parity");
    let mut foreign = Engine::new(engine_cfg());
    foreign
        .register_user_strategy(
            Box::new(loaded.strategy),
            format!("dylib:{}", path.display()),
        )
        .unwrap();
    assert!(foreign.set_strategy_enabled("parity", true));
    foreign.set_hot_params(Some(Arc::new(hot_registry(dec!(0.50)))));
    let b = run_replay(&steps, &mut foreign);

    assert_eq!(a.breaks, b.breaks, "trend-break sequence differs");
    assert_eq!(a.cycles.len(), b.cycles.len());
    for (i, (ca, cb)) in a.cycles.iter().zip(b.cycles.iter()).enumerate() {
        assert_eq!(ca.entries, cb.entries, "entry orders differ at cycle {i}");
        assert_eq!(ca.exits, cb.exits, "exit intents differ at cycle {i}");
        assert_eq!(
            ca.confirmed, cb.confirmed,
            "confirmed tokens differ at cycle {i}"
        );
        assert_eq!(
            ca.diagnostics, cb.diagnostics,
            "diagnostics differ at cycle {i}"
        );
    }

    // Spot-check the behaviour actually exercised (so this test cannot pass on
    // an empty replay): entry, break, exit, round-reset slot and both tokens.
    let flat_entries: Vec<_> = a.cycles.iter().flat_map(|c| c.entries.iter()).collect();
    assert!(
        flat_entries
            .iter()
            .any(|e| e.0 == "up" && e.1 == "0.43" && e.5 == "up" && e.6 == "parity:BTC:up:1"),
        "{flat_entries:?}"
    );
    assert!(
        flat_entries
            .iter()
            .any(|e| e.0 == "down" && e.1 == "0.45" && e.5 == "down"),
        "{flat_entries:?}"
    );
    assert!(
        flat_entries.iter().any(|e| e.6 == "parity:BTC:up:2"),
        "round reset not reflected: {flat_entries:?}"
    );
    assert!(
        a.breaks.contains(&("up".to_string(), "0.30".to_string())),
        "parity break missing: {:?}",
        a.breaks
    );
    assert!(
        a.cycles
            .iter()
            .any(|c| c.exits == vec![("up".to_string(), "parity_tp".to_string())])
    );
    assert!(
        a.cycles
            .iter()
            .any(|c| c.confirmed == vec!["up".to_string()])
    );

    // Config-in-force parity (E-parity, `bk_strategy_config_view`): the dylib's
    // OPTIONAL symbol and the in-tree twin must report the SAME config — base
    // values overlaid with the hot parameters actually applied above.
    let expected = serde_json::json!({"trendMaxEntryPrice": "0.50", "exitAbove": "0.60"});
    let view_of = |views: &[(String, String)], name: &str| {
        serde_json::from_str::<serde_json::Value>(
            views
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v)
                .unwrap_or_else(|| panic!("{name} reports no config view")),
        )
        .expect("config view is valid JSON")
    };
    assert_eq!(
        view_of(&in_tree.strategy_config_views(), "parity"),
        expected
    );
    assert_eq!(
        view_of(&foreign.strategy_config_views(), "parity"),
        expected
    );
}
