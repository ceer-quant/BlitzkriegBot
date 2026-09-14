//! End-to-end: load the example user-layer strategy shared library through the
//! FROZEN C ABI, drive it with ticks, and assert the signals it returns.
//!
//! Requires the library to be built first:
//!   (cd user_layer/strategies && cargo build --release)
//! and the kernel built with `--features strategy-loading`.
#![cfg(feature = "strategy-loading")]

use blitzkrieg_core::strategy_engine::{MarketTick, Signal, Strategy};
use blitzkrieg_core::strategy_engine::loader::DynamicStrategy;
use rust_decimal::Decimal;
use std::path::PathBuf;
use std::str::FromStr;

fn lib_path() -> PathBuf {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("user_layer").join("strategies").join("target").join("release");
    for name in ["libdog_strategy.dylib", "libdog_strategy.so", "dog_strategy.dll"] {
        let p = base.join(name);
        if p.exists() {
            return p;
        }
    }
    base.join("libdog_strategy.dylib")
}

fn tick(mid: &str) -> MarketTick {
    MarketTick {
        symbol: "TOKEN".into(),
        asset: "BTC".into(),
        best_bid: Decimal::from_str(mid).unwrap() - Decimal::new(1, 2),
        best_ask: Decimal::from_str(mid).unwrap() + Decimal::new(1, 2),
        mid: Decimal::from_str(mid).unwrap(),
        timestamp_ms: 1,
    }
}

#[test]
fn loads_and_drives_the_user_strategy_dylib() {
    let path = lib_path();
    if !path.exists() {
        eprintln!("skipping: build the dylib first (cd user_layer/strategies && cargo build --release)");
        return;
    }
    let mut s = match DynamicStrategy::load(&path) {
        Ok(s) => s,
        Err(e) => panic!("failed to load {}: {e}", path.display()),
    };
    assert_eq!(s.name(), "dog_strategy");

    // Above the buy level → no signal.
    assert!(s.on_tick(&tick("0.50")).is_none());
    // Dip to/below 0.43 → Buy at the tick mid.
    match s.on_tick(&tick("0.42")) {
        Some(Signal::Buy { symbol, price, size }) => {
            assert_eq!(symbol, "TOKEN");
            assert_eq!(price, Decimal::from_str("0.42").unwrap());
            assert_eq!(size, Decimal::from(10));
        }
        other => panic!("expected a Buy signal, got {other:?}"),
    }
    // on_round must not panic (optional hook).
    s.on_round(42);
}

#[test]
fn policy_rejects_bad_paths() {
    use blitzkrieg_core::strategy_engine::loader::{load_strategy, LoadOutcome};
    let out = load_strategy(std::path::Path::new("user_layer/strategies/secret_key.dylib"));
    assert!(matches!(out, LoadOutcome::Rejected { .. }), "got {out:?}");
}
