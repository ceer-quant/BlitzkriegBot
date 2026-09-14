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
use blitzkrieg_core::shadow_evolution::MutableParams;
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

fn lib_path() -> Option<PathBuf> {
    let base = dylib_dir();
    for name in ["libdog_strategy.dylib", "libdog_strategy.so", "dog_strategy.dll"] {
        let p = base.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Skip (or, under BK_REQUIRE_DYLIB=1, hard-fail) when the cdylib was not built.
fn require_lib() -> PathBuf {
    match lib_path() {
        Some(p) => p,
        None => {
            let msg = "build the v2 dylib first: (cd user_layer/strategies && cargo build --release)";
            if std::env::var_os("BK_REQUIRE_DYLIB").is_some() {
                panic!("{msg} (looked in {})", dylib_dir().display());
            }
            eprintln!("skipping: {msg}");
            std::process::exit(0);
        }
    }
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
        spread_arb: SpreadArbConfig { trend_max_entry_price: dec!(0.45), ..Default::default() },
        max_orderbook_stale_ms: 8000,
        momentum_window_sec: 30,
        momentum_tol_pct: dec!(0.03),
        size_usd: dec!(2.5),
        min_shares: dec!(10),
        max_shares: dec!(10),
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

#[test]
fn loads_and_drives_the_v2_dog_strategy_dylib() {
    let path = require_lib();
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    assert_eq!(loaded.name, "dog_strategy");
    assert_eq!(loaded.version, "0.2.0");

    let mut engine = Engine::new(engine_cfg());
    // The builtin must not fire in this replay: no 12-sample UP trend.
    assert!(engine.set_strategy_enabled("spread_arb", false));
    engine
        .register_user_strategy(Box::new(loaded.strategy), format!("dylib:{}", path.display()))
        .unwrap();
    assert!(engine.set_strategy_enabled("dog_strategy", true));
    let expected_source = format!("dylib:{}", path.display());
    assert_eq!(engine.strategy_source("dog_strategy"), Some(expected_source.as_str()));

    let now = 1_800_000i64;
    engine.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });

    // Deep two-sided book, but mid 0.50 is ABOVE the 0.43 dip ceiling → idle.
    engine.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.49), dec!(60)), (dec!(0.48), dec!(60))],
        asks: vec![(dec!(0.51), dec!(60)), (dec!(0.52), dec!(60))],
        now_ms: now + 1_000,
    });
    assert!(engine.evaluate(now + 1_000).is_empty());
    // Two levels each side → the dylib confirms the token (proves full ladder).
    assert!(engine.confirmed_tokens().contains("up"), "{:?}", engine.confirmed_tokens());
    let diag = engine.confirmed_diagnostics(now + 1_000);
    assert!(diag.iter().any(|d| d.get("symbol").and_then(|s| s.as_str()) == Some("up")), "{diag:?}");

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
    assert!(engine.set_strategy_enabled("spread_arb", false));
    engine
        .register_user_strategy(Box::new(loaded.strategy), "dylib:hot".into())
        .unwrap();
    assert!(engine.set_strategy_enabled("dog_strategy", true));

    let now = 1_800_000i64;
    engine.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });
    // mid 0.47 > default ceiling 0.43 → no entry.
    engine.on_data(DataEvent::Book {
        token_id: "down".into(),
        bids: vec![(dec!(0.46), dec!(60))],
        asks: vec![(dec!(0.48), dec!(60))],
        now_ms: now + 1_000,
    });
    assert!(engine.evaluate(now + 1_000).is_empty());

    // Hot-swap the entry ceiling up to 0.50 → next evaluate must fire.
    let mut p = MutableParams::default();
    p.trend_max_entry_price = dec!(0.50);
    engine.set_hot_params(Arc::new(arc_swap::ArcSwap::from_pointee(p)));
    let orders = engine.evaluate(now + 2_000);
    assert_eq!(orders.len(), 1, "{orders:?}");
    assert_eq!(orders[0].token_id, "down");
    assert_eq!(orders[0].price, dec!(0.48));
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
    // A shared object that is NOT a strategy: libc itself exports plenty but
    // never bk_strategy_abi_version, so it is rejected as v1/pre-v2 rather than
    // misread. Locating libc is best-effort; skip where unavailable.
    let candidates = [
        "/usr/lib/x86_64-linux-gnu/libc.so.6",
        "/usr/lib/libc.so.6",
        "/lib/x86_64-linux-gnu/libc.so.6",
        "/usr/lib/aarch64-linux-gnu/libc.so.6",
    ];
    let Some(c) = candidates.into_iter().find(|c| Path::new(c).exists()) else {
        eprintln!("skipping: no libc candidate on this host (macOS system libs are not plain .so)");
        return;
    };
    use blitzkrieg_core::strategy_engine::loader::LoadOutcome;
    let msg = match load_foreign(Path::new(c)) {
        Err(LoadOutcome::Failed { reason, .. }) | Err(LoadOutcome::Rejected { reason, .. }) => reason,
        Ok(_) => panic!("non-strategy library must be rejected"),
        // load_foreign never returns an Err(Loaded), but the enum allows it.
        Err(LoadOutcome::Loaded { name, .. }) => panic!("impossible load outcome for {name}"),
    };
    assert!(msg.contains("abi_version") || msg.contains("v1"), "unexpected: {msg}");
}
