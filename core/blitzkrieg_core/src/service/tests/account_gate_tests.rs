#![cfg(test)]

//! #1045 acceptance: the single in-memory account gate.
//!
//! 1. trip → unfreeze → entries allowed for the rest of the day (unfreeze clears
//!    the session trip latch; `emit_daily_trip` never re-lifts a stale latch —
//!    cash truth wins over a remembered breaker);
//! 2. trip → UTC day roll → gate auto-clears, entries re-open;
//! 3. a legacy risk-state file carrying `accountFreezes`/`auto_frozen` parses
//!    without error and replays nothing — a clean boot starts trading.

use super::*;
use crate::position::{EquityBasis, PositionConfig};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal_macros::dec;

const SEED: rust_decimal::Decimal = dec!(1000);

fn gate_core() -> Core {
    Core::new(CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(10_000),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        positions: PositionConfig {
            max_daily_loss_equity_pct: dec!(5), // cap = 5% of opening equity = 50
            ..Default::default()
        },
        ..Default::default()
    })
}

fn acct_id() -> crate::model::AccountId {
    crate::model::default_account_id()
}

fn buy_req(key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(1),
        size: dec!(1000),
        internal_key: key.into(),
        strategy: "s1".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: acct_id(),
    }
}

fn entry_req(key: &str) -> OrderRequest {
    let mut r = buy_req(key);
    r.size = dec!(10);
    r.price = dec!(0.4);
    r
}

/// Book a 600 loss on `default` against a 50 cap (5% of the 1000 seed), latch
/// the trip this session (as `note_daily_drawdown` would), then announce it via
/// `emit_daily_trip` (this is what every tick does). The loss is booked
/// straight into the day's realized bucket so the test does not depend on the
/// engine cycle matching a buy against a sell.
fn trip_daily_loss(c: &mut Core, at_ms: i64) {
    let acct = crate::model::default_account_id();
    c.positions
        .roll_daily("default", at_ms, SEED, EquityBasis::Dry);
    c.positions.set_daily_realized_for_test(&acct, dec!(-600));
    c.positions
        .set_daily_tripped_for_test(&acct, at_ms, dec!(50));
    // Announce the trip (every tick does this): lifts a DailyLoss gate
    // expiring at the next UTC day boundary.
    c.emit_daily_trip();
    assert!(
        c.account_gate_blocked("default", at_ms + 500),
        "daily loss trip must gate entries"
    );
}

#[test]
fn daily_loss_trip_unfreeze_allows_entries_rest_of_day() {
    let mut c = gate_core();
    let t0 = 1_700_000_000_000;
    trip_daily_loss(&mut c, t0);
    assert!(
        c.account_gate_blocked("default", t0 + 500),
        "trip gates entries"
    );

    // Unfreeze clears the gate AND the session trip latch — entries allowed for
    // the rest of the day. Unfreeze never erases the realized loss, but the
    // daily-loss gate is not re-armed until the UTC day rolls.
    let r = c
        .unfreeze_account("default", "op", t0 + 600)
        .expect("unfreeze");
    assert!(r.unfrozen, "gate was cleared");
    assert!(
        r.previous_reason.is_some(),
        "the receipt names the previous gate"
    );
    assert!(r.entries_allowed, "entries allowed at the unfreeze instant");
    assert!(
        !c.account_gate_blocked("default", t0 + 700),
        "entries allowed after unfreeze"
    );

    // A subsequent emit_daily_trip does NOT re-lift the gate: the latch was
    // cleared by unfreeze, and emit_daily_trip only gates newly-latched trips.
    c.emit_daily_trip();
    assert!(
        !c.account_gate_blocked("default", t0 + 800),
        "emit_daily_trip does not re-latch after unfreeze"
    );
    assert!(
        c.place_pending(entry_req("entry"), t0 + 900).is_ok(),
        "an unfrozen account trades again"
    );
}

#[test]
fn daily_loss_gate_auto_clears_at_utc_day_roll() {
    let mut c = gate_core();
    let t0 = 1_700_000_000_000; // day 19675
    trip_daily_loss(&mut c, t0);
    assert!(c.account_gate_blocked("default", t0 + 500));

    // Roll into the next UTC day (day 19676): the DailyLoss gate expires at the
    // boundary and is cleared by `roll_daily_budget` — no API call needed.
    let t1 = 1_700_006_401_000; // t0's next UTC day boundary + 1s
    c.roll_daily_budget(t1);
    assert!(
        !c.account_gate_blocked("default", t1),
        "DailyLoss gate auto-cleared at the UTC day boundary"
    );
    // Entries re-open immediately (the ledger is untouched; only the gate cleared).
    assert!(
        c.place_pending(entry_req("entry"), t1).is_ok(),
        "entries re-open after the daily loss gate expires"
    );
}

#[test]
fn legacy_risk_state_freezes_are_ignored_at_boot() {
    use std::path::PathBuf;

    let root = std::env::temp_dir().join(format!(
        "test-1045-legacy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("temp root creates");
    let path: PathBuf = root.join("positions.risk-state.json");

    // An OLD risk-state file: carries the retired `accountFreezes` /
    // `auto_frozen` cells plus a limit. serde ignores unknown keys; the boot
    // restore must not replay any freeze.
    std::fs::write(
        &path,
        r#"{
  "savedAtMs": 1000,
  "limits": { "maxOrderNotional": "250" },
  "accountFreezes": { "default": "stale manual hold" },
  "auto_frozen": { "default": 1000 }
}"#,
    )
    .unwrap();

    assert!(matches!(
        crate::service::risk_admission::load_risk_state_file(&path),
        crate::service::risk_admission::RiskStateLoad::Ok(_)
    ));

    let mut c = crate::service::Core::with_account_book(
        crate::service::CoreConfig {
            mode: Mode::Dry,
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: SEED,
            auto_exits_enabled: false,
            risk_state_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        },
        crate::account::AccountLedgers::new(
            vec![crate::account::Account {
                id: acct_id(),
                name: "default".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: crate::account::AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                opening_equity: None,
                updated_at_ms: 0,
            }],
            acct_id(),
        ),
    );

    // The limit restored (file parsed), but NO freeze was replayed.
    assert_eq!(c.risk.config().max_order_notional, dec!(250));
    assert!(!c.is_killed());
    assert!(!c.account_gate_blocked("default", now_ms()));
    assert!(c.account_freeze_reason("default", now_ms()).is_none());
    // And the account trades: entry passes every gate.
    assert!(
        c.place_pending(entry_req("entry"), now_ms()).is_ok(),
        "a clean boot trades even with a stale freeze book on disk"
    );

    drop(c);
    let _ = std::fs::remove_dir_all(&root);
}
