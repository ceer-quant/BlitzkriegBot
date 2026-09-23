//! End-to-end for the complete-set pair strategy (`pair_arb`) through the v2
//! dylib ABI: register it into a real [`Engine`], feed a round whose two legs'
//! BIDS sum below a dollar, and require BOTH legs out as kernel-sized orders in
//! the SAME evaluation cycle — resting at those bids.
//!
//! This is the plumbing proof for the frozen-corpus replay: if the engine can
//! turn a discounted pair into two resting orders here, a replay that produces
//! none is reporting a market with no discount, not a broken pipe.
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
use std::path::PathBuf;

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

fn require_named_lib(base_name: &str) -> PathBuf {
    match lib_path_named(base_name) {
        Some(p) => p,
        None => {
            let msg = "build the v2 dylibs first: \
                       (cd user_layer/strategies && cargo build --release)"
                .to_string();
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
        spread_arb: SpreadArbConfig::default(),
        max_orderbook_stale_ms: 8000,
        momentum_window_sec: 30,
        momentum_tol_pct: dec!(0.03),
        size_usd: dec!(2.5),
        min_shares: dec!(1),
        max_shares: dec!(100),
        // The absolute budget is the one in force: the pair's own share count
        // (from `notional_cap_usd`) is what decides the leg size here.
        size_pct: dec!(0),
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

/// A discounted complete set on the BID side: 0.44 + 0.48 = 0.92 → a 8¢ locked
/// edge per share-pair, fee-free because both legs are maker fills. Both legs
/// must come back as orders resting AT THOSE BIDS, with the SAME share count.
#[test]
fn the_pair_strategy_emits_both_legs_for_a_discounted_complete_set() {
    let path = require_named_lib("pair_arb_strategy");
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    assert_eq!(loaded.name, "pair_arb");

    let mut engine = Engine::new(engine_cfg());
    engine
        .register_user_strategy(
            Box::new(loaded.strategy),
            format!("dylib:{}", path.display()),
        )
        .unwrap();
    assert!(engine.set_strategy_enabled("pair_arb", true));

    let now = 1_800_000i64;
    let end_ms = now + 600_000; // 600s left, above the 300s floor
    engine.on_data(DataEvent::RoundMarkets {
        markets: vec![market(end_ms)],
        now_ms: now,
    });

    // Two-sided books; bid sums to 0.92 → a discount the strategy must rest at.
    engine.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.44), dec!(60)), (dec!(0.43), dec!(60))],
        asks: vec![(dec!(0.46), dec!(60)), (dec!(0.47), dec!(60))],
        now_ms: now + 1_000,
    });
    engine.on_data(DataEvent::Book {
        token_id: "down".into(),
        bids: vec![(dec!(0.48), dec!(60)), (dec!(0.47), dec!(60))],
        asks: vec![(dec!(0.50), dec!(60)), (dec!(0.51), dec!(60))],
        now_ms: now + 1_000,
    });

    let orders = engine.evaluate(now + 1_000);
    assert_eq!(orders.len(), 2, "expected both legs: {orders:?}");
    let up = orders
        .iter()
        .find(|o| o.token_id == "up")
        .unwrap_or_else(|| panic!("no up leg in {orders:?}"));
    let down = orders
        .iter()
        .find(|o| o.token_id == "down")
        .unwrap_or_else(|| panic!("no down leg in {orders:?}"));
    assert_eq!(up.strategy, "pair_arb");
    assert_eq!(down.strategy, "pair_arb");
    assert_eq!(up.direction, "up");
    assert_eq!(down.direction, "down");
    // The maker design: each leg rests at its OWN BID, not at the ask.
    assert_eq!(up.price, dec!(0.44));
    assert_eq!(down.price, dec!(0.48));
    // Equal shares on both legs — the complete-set invariant.
    assert_eq!(up.size, down.size, "{up:?} {down:?}");

    // Same cycle only: no repeat while the same round stays quoted.
    assert!(
        engine.evaluate(now + 2_000).is_empty(),
        "a pair must not re-enter inside its own round"
    );
}

/// Bids that sum to a dollar or more leave nothing locked: stay out.
#[test]
fn the_pair_strategy_stays_out_when_the_set_costs_a_dollar_or_more() {
    let path = require_named_lib("pair_arb_strategy");
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    let mut engine = Engine::new(engine_cfg());
    engine
        .register_user_strategy(Box::new(loaded.strategy), "dylib:par".into())
        .unwrap();
    assert!(engine.set_strategy_enabled("pair_arb", true));

    let now = 1_800_000i64;
    engine.on_data(DataEvent::RoundMarkets {
        markets: vec![market(now + 600_000)],
        now_ms: now,
    });
    engine.on_data(DataEvent::Book {
        token_id: "up".into(),
        bids: vec![(dec!(0.51), dec!(60))],
        asks: vec![(dec!(0.53), dec!(60))],
        now_ms: now + 1_000,
    });
    engine.on_data(DataEvent::Book {
        token_id: "down".into(),
        bids: vec![(dec!(0.50), dec!(60))],
        asks: vec![(dec!(0.52), dec!(60))],
        now_ms: now + 1_000,
    });
    // 0.51 + 0.50 = 1.01 → no locked edge.
    assert!(engine.evaluate(now + 1_000).is_empty());
}

/// The declared hold-to-settlement reaches the kernel through the optional
/// symbol, and the strategy's own expiry floor is honoured.
#[test]
fn the_pair_strategy_declares_hold_to_settlement_and_an_expiry_floor() {
    let path = require_named_lib("pair_arb_strategy");
    let loaded = load_foreign(&path).unwrap_or_else(|e| panic!("load failed: {e:?}"));
    assert_eq!(loaded.name, "pair_arb");
    // Loader-level declarations resolved from the optional symbols.
    let exempt = loaded.gate_exemptions;
    assert!(exempt.timing, "pair legs need the timing window waived");
    assert!(exempt.momentum, "a pair has no single direction to align");
    assert_eq!(
        exempt.timing_floor_sec(180),
        300,
        "the declared floor is the strategy's own min_time_left_sec, not the kernel default"
    );
    assert!(loaded.holds_to_settlement);
}
