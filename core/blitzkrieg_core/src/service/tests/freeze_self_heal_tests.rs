#![cfg(test)]

//! 冻结自愈（2026-10-28 真线事故）：
//! 1. 良性关闭拒单（venue 说 "Insufficient position" —— 超时的退出 POST 实际
//!    已成交）不再喂冻结 streak，退出意图继续退避重试；
//! 2. 三条自动冻结路径（拒单 streak / 自检失败 / sweep 失败 streak）登记在
//!    `auto_frozen`，场地恢复（接单 / reconcile 成功）后自动解冻；手动冻结
//!    永不自动解。

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

fn core() -> Core {
    Core::new(CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        ..Default::default()
    })
}

fn close_req(key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Sell,
        mode: FillPolicy::Taker,
        price: dec!(0.4),
        size: dec!(10),
        internal_key: key.into(),
        strategy: "s1".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

fn open_req(key: &str) -> OrderRequest {
    let mut r = close_req(key);
    r.side = Side::Buy;
    r.internal_key = key.into();
    r
}

/// The venue's honest refusal of an exit whose place-POST actually filled:
/// the canonical polymarket CLOB text from the 2026-10-28 incident.
fn insufficient_position_err() -> crate::model::CoreError {
    crate::model::CoreError::new(
        blitzkrieg_market_api::CoreErrorCode::VenueError,
        "Insufficient position for limit sell order",
    )
}

fn sweep_err() -> crate::model::CoreError {
    crate::model::CoreError::new(
        blitzkrieg_market_api::CoreErrorCode::VenueError,
        "sweep timed out",
    )
}

/// A close intent refused with "Insufficient position" is BENIGN bookkeeping
/// drift (the timed-out exit POST actually filled): it must not feed either
/// freeze streak, however many times it repeats — the account keeps trading.
/// The intent itself still backoffs (next_ok_ms advances).
#[test]
fn close_intent_insufficient_position_does_not_feed_freeze_streak() {
    let mut c = core();
    for i in 0..FREEZE_ON_REJECTS as i64 + 2 {
        let (id, _) = c
            .place_pending(close_req("exit:ladder:tok"), i * 1_000 + 1)
            .unwrap();
        c.reject_live_result(&id, Some(insufficient_position_err()), i * 1_000 + 100)
            .unwrap();
    }
    assert!(
        c.account_freeze_reason("default").is_none(),
        // Static message (no {:?} interpolation): CodeQL's cleartext-logging
        // check flags measured-value interpolation in test asserts (#693's
        // fix, same family); the reason itself is on the line above.
        "benign close rejections must never freeze the account"
    );
    assert!(
        !c.is_killed(),
        "the global kill switch stays quiet on benign close rejections"
    );
    let stats = c.engine_stats();
    assert!(
        stats["accountFreezes"]
            .as_object()
            .is_none_or(|m| m.is_empty()),
        "the freeze book stays empty: {stats}"
    );
    // The intent still saw every rejection (per-strategy accounting unchanged).
    assert_eq!(stats["venueRejected"], FREEZE_ON_REJECTS as u64 + 2);
    // Backoff is alive: the intent's cooldown carried every rejection, so a
    // re-place one tick after the LAST rejection is locally paused (the exit
    // lane consults `placement_blocked` before each POST). The attempt cap is
    // deliberately NOT part of the close path (#174: unhedged exposure keeps
    // retrying), so the assertion rides the backoff clock itself.
    let last_reject = (FREEZE_ON_REJECTS as i64 + 1) * 1_000 + 100;
    let backoff = reject_backoff_ms(FREEZE_ON_REJECTS + 2);
    assert!(
        c.placement_blocked("exit:ladder:tok", last_reject + 1),
        "the benign rejections still bought the intent a real backoff"
    );
    assert!(
        !c.placement_blocked("exit:ladder:tok", last_reject + backoff + 1),
        "and the backoff expires"
    );
}

/// The exemption is for CLOSE intents only: an OPEN intent refused with the
/// same "Insufficient position" text is a real gate signal and keeps the
/// existing counting path — 5 consecutive refusals still freeze the account.
#[test]
fn open_intent_insufficient_position_still_counts() {
    let mut c = core();
    for i in 0..FREEZE_ON_REJECTS as i64 {
        let (id, _) = c
            .place_pending(open_req(&format!("k{i}")), i * 1_000 + 1)
            .unwrap();
        c.reject_live_result(&id, Some(insufficient_position_err()), i * 1_000 + 100)
            .unwrap();
    }
    assert!(
        c.account_freeze_reason("default")
            .is_some_and(|r| r.contains("consecutive venue rejections")),
        // Static message: same CodeQL cleartext-logging family as above.
        "an open intent's refusal streak must still freeze the account"
    );
}

/// Auto-frozen (sweep-failure streak on a venue-bound account) + the venue
/// accepts an order again (`confirm_live` → mark_live): the freeze lifts
/// itself, the auto ledger clears, and the account can place NEW orders again.
#[test]
fn auto_frozen_account_lifted_when_venue_accepts() {
    let mut c = venue_bound_fixture();
    for i in 0..SWEEP_FAILURE_FREEZE as i64 {
        c.on_venue_reconcile_failed("polymarket", sweep_err(), 1_000 + i * 100);
    }
    assert!(
        c.account_freeze_reason("poly-acct").is_some(),
        // Static message: same CodeQL cleartext-logging family as above.
        "precondition: 3 sweep failures auto-froze poly-acct"
    );
    // A close flows through the freeze (the #174 hatch) and the venue takes it.
    let mut close = close_req("flatten:manual:tok");
    close.account_id = crate::account::AccountId::from("poly-acct");
    let (id, _) = c
        .place_pending(close, 2_000)
        .unwrap_or_else(|e| panic!("a close flows through the freeze: {e}"));
    c.confirm_live(&id, 2_100).unwrap();
    assert!(
        c.account_freeze_reason("poly-acct").is_none(),
        // Static message: same CodeQL cleartext-logging family as above.
        "venue acceptance must lift the auto freeze"
    );
    // A NEW order now passes the gate again — full trading capability back.
    let mut entry = open_req("fresh-entry");
    entry.account_id = crate::account::AccountId::from("poly-acct");
    c.place_pending(entry, 2_200)
        .unwrap_or_else(|e| panic!("the lifted account trades again: {e}"));
}

/// An operator's `freeze_account` (the manual route, no `auto_frozen` entry)
/// is a verdict, not a symptom: no amount of venue health lifts it.
#[test]
fn manual_freeze_not_lifted_by_venue_health() {
    let mut c = core();
    c.freeze_account("default", "operator: investigating".into());
    let (id, _) = c
        .place_pending(close_req("flatten:manual:tok"), 1_000)
        .unwrap_or_else(|e| panic!("a close flows through a manual freeze: {e}"));
    c.confirm_live(&id, 1_100).unwrap();
    assert!(
        c.account_freeze_reason("default").is_some(),
        "venue acceptance must NOT lift a manual freeze"
    );
    // Even a successful sweep stays hands-off.
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 1_000_000,
    })
    .unwrap();
    assert!(
        c.account_freeze_reason("default").is_some(),
        "a successful reconcile must NOT lift a manual freeze either"
    );
}

/// P0 (v0.4.2 audit): the operator freezing an account the kernel had ALREADY
/// auto-frozen must outrank the self-heal. `freeze_account` is idempotent, so
/// the old manual route left the `auto_frozen` stamp in place and the next
/// healthy-venue sweep resumed the account — the kill switch was silently
/// lifted ten minutes after the operator pulled it.
///
/// Negative: on the pre-fix code the reason is gone after the sweep.
#[test]
fn manual_freeze_after_an_auto_freeze_is_never_self_healed() {
    let mut c = core();
    c.freeze_account_auto("default", "venue rejecting (3)".into(), 1_000_000);
    assert!(c.account_freeze_reason("default").is_some(), "auto-frozen");

    // The operator now pulls the kill switch on the SAME account.
    c.freeze_account_manual("default", "operator: investigating".into());

    // Far past the debounce, with positive venue evidence.
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 1_000_000 + FREEZE_AUTO_LIFT_MIN_MS + 60_000,
    })
    .unwrap();

    assert!(
        c.account_freeze_reason("default").is_some(),
        "an operator freeze must never be lifted by the self-heal"
    );
}

/// The reconcile auto-lift is debounced: a successful sweep inside
/// FREEZE_AUTO_LIFT_MIN_MS of the freeze leaves it on (the outage may still
/// be flickering); only after the ten quiet minutes does the sweep lift it.
#[test]
fn reconcile_lift_respects_debounce() {
    let mut c = core();
    c.freeze_account_auto(
        "default",
        "reconciliation sweep failed (3): x".into(),
        1_000_000,
    );
    assert!(c.account_freeze_reason("default").is_some());

    // 9 minutes later: sweep healthy again, but too fresh — stays frozen.
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 1_000_000 + FREEZE_AUTO_LIFT_MIN_MS - 60_000,
    })
    .unwrap();
    assert!(
        c.account_freeze_reason("default").is_some(),
        "inside the debounce window a successful sweep must not lift the freeze"
    );
    // ≥10 minutes after the freeze: the same evidence lifts it.
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 1_000_000 + FREEZE_AUTO_LIFT_MIN_MS,
    })
    .unwrap();
    assert!(
        c.account_freeze_reason("default").is_none(),
        // Static message: same CodeQL cleartext-logging family as above.
        "past the debounce window a successful sweep lifts the auto freeze"
    );
}

/// After an auto-lift, a fresh failure re-freezes with the NEW trigger's
/// reason — the heal must not leave a stale cause in the panel's freeze book.
#[test]
fn re_freeze_after_auto_lift_carries_new_reason() {
    let mut c = core();
    c.freeze_account_auto("default", "cause A: sweep failed".into(), 1_000);
    assert!(c.account_freeze_reason("default").is_some());
    c.auto_lift_if_venue_healthy("default", "venue accepted order", 2_000);
    assert!(c.account_freeze_reason("default").is_none());

    c.freeze_account_auto("default", "cause B: self-check failed".into(), 3_000);
    assert_eq!(
        c.account_freeze_reason("default"),
        Some("cause B: self-check failed"),
        "the re-freeze must carry the new trigger's reason, not the stale one"
    );
}

/// The `two_venue_book_fixture` shape, module-local (fixtures are deliberately
/// not shared): ONE ACTIVE account bound to `polymarket`, so the sweep-failure
/// streak freezes the account book (not the global gate) — the exact shape the
/// auto-lift must heal.
fn venue_bound_fixture() -> Core {
    let book = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: crate::account::AccountId::from("poly-acct"),
            name: "polymarket 主账户".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: "polymarket".into(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: 0,
        }],
        crate::account::AccountId::from("poly-acct"),
    );
    let cfg = CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        // Same discipline as the sibling fixtures: the cwd-relative default
        // order log must not collect these tests' orders.
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    Core::with_account_book(cfg, book)
}
