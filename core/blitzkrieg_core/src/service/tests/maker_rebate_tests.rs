#![cfg(test)]
//! Maker-rebate modeling (`--maker-rebate`, almach-benchmark REPORT §5 失真
//! 点 5) and the pure-maker execution switch (`--entry-maker-timeout-ms < 0`).
//!
//! Three pins:
//! 1. a MAKER fill under `model_maker_rebate` credits cash AND accrues the
//!    rebate onto the position's flows, and the closed trade's `net` includes
//!    it — with `balance == seed + Σ net` still an identity;
//! 2. a negative entry maker timeout never arms an escalation clock: the
//!    resting maker leg survives past its timeout untouched (the pure-maker
//!    posture the operator switch promises);
//! 3. a snapshot written before `makerRebateUsd` existed loads unchanged —
//!    the serde default keeps the pre-rebate shape compatible.

use super::*;
use crate::model::{FillStatus, OrderRole};
use crate::risk::RiskConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const SEED: Decimal = dec!(1000);

/// A dry core with the rebate model armed and the exit engine quiet, so the
/// fixtures' arithmetic stays exactly the fills they place.
fn rebate_core() -> Core {
    Core::new(CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        model_maker_rebate: true,
        fee_schedule_replay: true,
        ..Default::default()
    })
}

fn req(side: Side, mode: FillPolicy, price: Decimal, size: Decimal, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side,
        mode,
        price,
        size,
        internal_key: key.into(),
        strategy: "acc".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

fn fill_as(
    order_id: &str,
    trade: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    maker: bool,
) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: "tok".into(),
        side,
        price,
        size,
        status: FillStatus::Confirmed,
        ts_ms: 0,
        tx_hash: None,
        maker: Some(maker),
        account_id: crate::model::default_account_id(),
    }
}

/// ① A MAKER fill earns `0.20 × taker-fee-equivalent(notional)` as cash AND
/// as a flow accrual; the closed trade's net carries it.
#[test]
fn a_maker_fill_credits_the_modeled_rebate_into_cash_and_net() {
    let mut c = rebate_core();
    // Rest a maker BUY at 0.40 while the ask (0.45) is away.
    let (id, _) = c
        .place(
            req(Side::Buy, FillPolicy::Maker, dec!(0.40), dec!(10), "entry"),
            0,
            1_000,
        )
        .unwrap();
    // The venue reports the fill AS MAKER (a resting order that got hit).
    c.ingest_fill(
        fill_as(&id, "t1", Side::Buy, dec!(0.40), dec!(10), true),
        1_100,
    )
    .unwrap();

    let pos = &c.positions().open_positions()[0];
    // 10 shares × 0.40 = $4.00 notional. The shipped legacy schedule prices
    // 0.40 at `0.125 × (0.4·0.6)²` per share; the rebate is 20% of that
    // taker-equivalent, computed through the ONE curve the kernel charges.
    let per_share = crate::exit_policy::fee_schedule().fee_per_share(dec!(0.40));
    let expected = dec!(0.20) * per_share * dec!(10);
    assert_eq!(pos.flows.entry_fee_usd, Decimal::ZERO, "maker pays no fee");
    assert_eq!(pos.flows.maker_rebate_usd, expected, "rebate accrued");
    assert!(expected > Decimal::ZERO);

    // The cash ledger credited the same amount in the same breath.
    assert_eq!(
        c.ledger().balance(),
        SEED - dec!(4) + expected,
        "entry cost leaves, the modeled rebate arrives as venue cash"
    );

    // Exit as a maker too (no exit fee), so the trade's net is
    // proceeds − basis + rebate and nothing else. The key is a close intent
    // (`exit:`), so the entry gates ("Already in BTC") let it through.
    let (sid, _) = c
        .place(
            req(
                Side::Sell,
                FillPolicy::Maker,
                dec!(0.60),
                dec!(10),
                "exit:tok",
            ),
            0,
            2_000,
        )
        .unwrap();
    c.book_snapshot("tok", vec![(dec!(0.60), dec!(100))], vec![], 2_100);
    let _ = c.tick(2_200);
    c.ingest_fill(
        fill_as(&sid, "t2", Side::Sell, dec!(0.60), dec!(10), true),
        2_300,
    )
    .unwrap();

    let closed = &c.positions().closed_positions()[0];
    // BOTH maker legs earned the rebate (entry at 0.40 AND exit at 0.60 — the
    // symmetric curve prices both notionals the same), and the trade carries
    // the sum.
    let exit_rebate =
        dec!(0.20) * crate::exit_policy::fee_schedule().fee_per_share(dec!(0.60)) * dec!(10);
    let total_rebate = expected + exit_rebate;
    assert_eq!(closed.maker_rebate_usd, total_rebate, "attribution carried");
    assert_eq!(
        closed.net_pnl_usd,
        dec!(6) - dec!(4) + total_rebate,
        "net claims the rebate with a plus sign"
    );
    // The E17 identity survives the rebate: balance == seed + Σ net.
    assert_eq!(
        c.ledger().balance(),
        SEED + closed.net_pnl_usd,
        "the trade record and the cash ledger show one number"
    );
}

/// ①b The model is off by default: the same maker fill books no rebate and no
/// counterfactual money (every existing deployment byte-identical).
#[test]
fn without_the_model_no_rebate_is_booked() {
    let mut c = Core::new(CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: SEED,
        auto_exits_enabled: false,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    });
    let (id, _) = c
        .place(
            req(Side::Buy, FillPolicy::Maker, dec!(0.40), dec!(10), "entry"),
            0,
            1_000,
        )
        .unwrap();
    c.ingest_fill(
        fill_as(&id, "t1", Side::Buy, dec!(0.40), dec!(10), true),
        1_100,
    )
    .unwrap();
    let pos = &c.positions().open_positions()[0];
    assert_eq!(pos.flows.maker_rebate_usd, Decimal::ZERO);
    assert_eq!(c.ledger().balance(), SEED - dec!(4), "cost only, no credit");
}

/// ② A negative entry maker timeout never arms the escalation clock: the
/// resting maker leg is still Live (untouched) past its timeout — the
/// pure-maker posture — instead of being cancelled and chased as a taker.
#[test]
fn a_negative_entry_timeout_never_escalates_the_maker_leg() {
    let mut c = rebate_core();
    c.config_mut().model_maker_rebate = false; // irrelevant to this pin
    // A crossing-at-rest book with the ask far above the limit: the maker
    // rests; a positive timeout would cancel + chase it at the sweep.
    c.book_snapshot("tok", vec![], vec![(dec!(0.90), dec!(100))], 900);
    let (id, _) = c
        .place(
            req(
                Side::Buy,
                FillPolicy::MakerThenTaker,
                dec!(0.40),
                dec!(10),
                "entry",
            ),
            -1,
            1_000,
        )
        .unwrap();
    assert!(c.ome().get(&id).unwrap().status.is_live(), "it rests");

    // Way past any sane timeout: no escalation target is due, the leg keeps
    // resting and no taker chase exists.
    c.tick(1_000 + 60_000).unwrap();
    let o = c.ome().get(&id).unwrap();
    assert!(o.status.is_live(), "the pure-maker leg was never cancelled");
    assert_eq!(o.escalate_at_ms, None, "no escalation clock was armed");
    assert!(
        !c.ome()
            .live_orders()
            .iter()
            .any(|o| o.internal_key.ends_with(":escalated")),
        "no chase leg exists"
    );
    assert_eq!(c.positions().open_positions().len(), 0);
}

/// ②b The positive control: the SAME leg with a positive timeout escalates —
/// the contrast that makes the negative-value pin above mean something.
#[test]
fn a_positive_timeout_escalates_the_same_leg() {
    let mut c = rebate_core();
    c.config_mut().model_maker_rebate = false;
    c.book_snapshot("tok", vec![], vec![(dec!(0.90), dec!(100))], 900);
    let (id, _) = c
        .place(
            req(
                Side::Buy,
                FillPolicy::MakerThenTaker,
                dec!(0.40),
                dec!(10),
                "entry",
            ),
            1_000,
            1_000,
        )
        .unwrap();
    assert!(c.ome().get(&id).unwrap().escalate_at_ms.is_some());
    c.tick(1_000 + 2_000).unwrap();
    assert!(
        !c.ome().get(&id).unwrap().status.is_live(),
        "the timed maker leg was escalated away"
    );
}

/// ③ A snapshot written before the rebate field existed loads: the serde
/// default (zero) keeps the pre-rebate JSONL byte-compatible, and the loaded
/// position still closes with the same net it would have had.
#[test]
fn a_pre_rebate_snapshot_loads_with_the_default_zero() {
    // An OpenPosition line the pre-rebate writer put down (camelCase wire):
    // no makerRebateUsd key anywhere.
    let legacy = format!(
        r#"{{"id":"hft-1","strategy":"acc","asset":"BTC","direction":"up",
        "tokenId":"tok","conditionId":"cond","entryPrice":"0.40",
        "currentPrice":"0.60","prevPrice":"0.40","shares":"10","costUsd":"4",
        "wasMakerEntry":true,"entryFeePct":"0","targetExitPrice":null,
        "enteredAtMs":1000,"expiresAtMs":91000,
        "accountId":"{acct}","venue":"","partnerTokenId":"",
        "lastBookTs":0,
        "state":{{"high_pnl_pct":"0","low_pnl_pct":"0","was_ever_positive":false,
        "high_water_mark":"0.4","hwm_confirm_count":0,"confirmed_high":"0.4",
        "last_bid_price":"0.4","bid_unchanged_since":0,"last_quote_at_ms":0,
        "last_progress_at":0,"last_progress_pct":"0","initial_depth":"0"}},
        "flows":{{"entryCostUsd":"4","entryFeeUsd":"0","proceedsUsd":"0",
        "exitFeeUsd":"0","openedShares":"10","soldShares":"0"}}}}"#,
        acct = crate::model::default_account_id().as_str(),
    );
    let pos: crate::position::OpenPosition =
        serde_json::from_str(&legacy).expect("the pre-rebate line must load");
    assert_eq!(
        pos.flows.maker_rebate_usd,
        Decimal::ZERO,
        "the missing field defaults to zero, not an error"
    );

    // The same JSON + the new key round-trips the loaded amount verbatim.
    let with_rebate = legacy.replace(
        "\"soldShares\":\"0\"",
        "\"soldShares\":\"0\",\"makerRebateUsd\":\"0.012\"",
    );
    let rebated: crate::position::OpenPosition =
        serde_json::from_str(&with_rebate).expect("the new field loads when present");
    assert_eq!(rebated.flows.maker_rebate_usd, dec!(0.012));

    // A ClosedPosition line (snake_case wire — no rename_all) from the
    // pre-rebate writer loads too.
    let closed_legacy = format!(
        r#"{{"id":"c1","strategy":"acc","asset":"BTC","direction":"up",
        "token_id":"tok","condition_id":"cond","entry_price":"0.40",
        "exit_price":"0.60","shares":"10","cost_usd":"4",
        "was_maker_entry":true,"was_maker_exit":true,
        "entry_fee_pct":"0","exit_fee_pct":"0","pnl_usd":"2","pnl_pct":"50",
        "net_pnl_usd":"2","net_pnl_pct":"50","high_pnl_pct":"52",
        "low_pnl_pct":"-3","hold_time_sec":64,"exit_reason":"take_profit",
        "entered_at_ms":1000,"exited_at_ms":65000,
        "entry_role":"maker","exit_role":"maker","dust_shares":"0",
        "account_id":"{acct}","venue":""}}"#,
        acct = crate::model::default_account_id().as_str(),
    );
    let closed: crate::position::ClosedPosition =
        serde_json::from_str(&closed_legacy).expect("the pre-rebate closed line must load");
    assert_eq!(closed.maker_rebate_usd, Decimal::ZERO);

    // And the round-trip through the manager: accruing then closing books the
    // rebate exactly once (the no-double-count pin).
    let pm = &mut crate::position::PositionManager::new(crate::position::PositionConfig::default());
    let opened = pm.open(
        crate::position::OpenParams {
            strategy: "acc".into(),
            asset: "BTC".into(),
            direction: crate::model::SignalDirection::Up,
            token_id: "tok".into(),
            condition_id: "cond".into(),
            account_id: crate::model::default_account_id(),
            venue: String::new(),
            partner_token_id: String::new(),
            entry_price: dec!(0.40),
            expires_at_ms: 91_000,
            was_maker: true,
            target_exit_price: None,
            round_slot: 1,
            round_duration_sec: 900,
        },
        1_000,
    );
    pm.apply_entry_fill(
        &opened.id,
        dec!(10),
        dec!(0.40),
        Decimal::ZERO,
        OrderRole::Maker,
    );
    pm.accrue_maker_rebate(&opened.id, dec!(0.012));
    pm.apply_exit_fill(
        &opened.id,
        dec!(10),
        dec!(0.60),
        Decimal::ZERO,
        OrderRole::Maker,
        Decimal::ZERO,
    );
    let closed = pm
        .close(
            &opened.id,
            dec!(0.60),
            crate::model::ExitReason::Manual,
            true,
            2_000,
        )
        .unwrap();
    assert_eq!(closed.maker_rebate_usd, dec!(0.012));
    assert_eq!(closed.net_pnl_usd, dec!(2) + dec!(0.012));
}
