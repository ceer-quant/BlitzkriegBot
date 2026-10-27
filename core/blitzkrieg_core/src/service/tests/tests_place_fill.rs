#![cfg(test)]

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

#[test]
fn spread_arb_entry_overrides_flow_through_engine_config() {
    // Defaults must stay byte-for-byte the shipped configuration: None
    // overrides leave every SpreadArbConfig field at its crate default.
    let default_cfg = CoreConfig::default().engine_config().spread_arb;
    assert_eq!(default_cfg, crate::signal::SpreadArbConfig::default());
    // An override flows through the one mapping that drives live and
    // backtest alike (E15: a sweep is a pure config variation).
    let cfg = CoreConfig {
        trend_confirm_sec: 90,
        spread_arb_trend_entry_factor: Some(dec!(0.8)),
        spread_arb_entry_min_obi: Some(dec!(-0.1)),
        spread_arb_entry_max_spread_pct: Some(dec!(2.5)),
        spread_arb_entry_dip_max_pct: Some(dec!(6)),
        spread_arb_entry_bounce_min_pct: Some(dec!(0.4)),
        spread_arb_entry_bounce_window_sec: Some(7),
        ..Default::default()
    }
    .engine_config()
    .spread_arb;
    assert_eq!(cfg.trend_confirm_sec, 90);
    assert_eq!(cfg.trend_entry_factor, dec!(0.8));
    assert_eq!(cfg.entry_min_obi, dec!(-0.1));
    assert_eq!(cfg.entry_max_spread_pct, dec!(2.5));
    assert_eq!(cfg.entry_dip_max_pct, dec!(6));
    assert_eq!(cfg.entry_bounce_min_pct, dec!(0.4));
    assert_eq!(cfg.entry_bounce_window_sec, 7);
}

/// P1 #235: the day's percentage cap is a share of a BOOK, and which book the
/// kernel hands the position manager is decided by the same predicate that
/// seeded the ledger — so the label cannot drift from the number.
#[test]
fn the_daily_budget_basis_follows_the_mode() {
    assert_eq!(dry_core(dec!(100)).equity_basis(), EquityBasis::Dry);
    assert_eq!(live_core(dec!(100)).equity_basis(), EquityBasis::Live);
    // Read-only settles locally, so its money is the simulation's too.
    let read_only = Core::new(CoreConfig {
        mode: Mode::ReadOnly,
        ..Default::default()
    });
    assert_eq!(read_only.equity_basis(), EquityBasis::Dry);
}

/// A restarted core restores live orders into the OME under their ORIGINAL
/// ids; the id counter must skip past them, or the first new placements
/// re-allocate a restored order's id and the OME's dedup guard refuses it.
/// Production saw `order already registered: dry_6` right after the 0.3.0
/// upgrade restart: `restore_orders` re-adopted a live `dry_6` while the
/// fresh process's counter was still at 1, and the sixth new placement
/// walked straight into it. Same rule the position recovery already
/// applies (advance past the restored ids, position.rs).
#[test]
fn restored_live_orders_push_the_id_counter_past_their_ids() {
    let dir = std::env::temp_dir().join(format!("bk-restore-ids-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("orders.jsonl");
    let restored = crate::model::TrackedOrder {
        order_id: "dry_6".into(),
        internal_key: "k6".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        role: OrderRole::Pending,
        price: dec!(0.40),
        size: dec!(5),
        filled_size: Decimal::ZERO,
        avg_fill_price: None,
        status: OrderStatus::Live,
        round_slot: 1,
        submitted_at_ms: 1,
        updated_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        venue_order_id: None,
        escalate_at_ms: None,
        maker_timeout_ms: 0,
        account_id: crate::model::default_account_id(),
        venue: String::new(),
        round_duration_sec: 0,
    };
    std::fs::write(&log, serde_json::to_string(&restored).unwrap() + "\n").unwrap();
    let mut c = Core::new(CoreConfig {
        order_log_path: Some(log.to_string_lossy().into_owned()),
        risk: RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: Decimal::from(10_000),
        ..Default::default()
    });
    assert_eq!(c.restore_orders(), 1, "the live order is re-adopted");

    // The counter was seeded past the restored id: the FIRST placement
    // after the restart must not collide with `dry_6` — before the fix
    // ids started at dry_1 and the sixth walked into the restored one.
    let (id, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "new-1"), 0, 1)
        .unwrap();
    assert_eq!(id, "dry_7", "the counter skips the restored ids entirely");
    let (id2, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "new-2"), 0, 1)
        .unwrap();
    assert_eq!(id2, "dry_8");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn taker_fills_immediately_and_spends() {
    let mut c = dry_core(dec!(10));
    // A live FOK fills only against resting depth at or inside its limit:
    // mirror an ask the order can actually take.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (id, st) = c
        .place(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(st, OrderStatus::Filled);
    assert_eq!(c.ome().get(&id).unwrap().filled_size, dec!(5));
    // 10 - 0.4*5 = 8, minus the taker entry fee (1.8% of 2 = 0.036);
    // reservation fully consumed.
    assert_eq!(c.ledger().balance(), Decimal::new(7964, 3));
    assert_eq!(c.ledger().reserved(), dec!(0));
}

#[test]
fn taker_not_crossing_the_book_is_rejected() {
    let mut c = dry_core(dec!(10));
    // Best ask 0.50 is outside a 0.40 buy limit — a live FOK dies.
    c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 1);
    let out = c
        .place_outcome(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(out.status, OrderStatus::Rejected);
    // #180: the refusal names its cause, in the code a live FOK dies under.
    let err = out
        .rejection
        .expect("a kernel refusal must carry its reason");
    assert_eq!(err.code, CoreErrorCode::WouldCross);
    assert!(
        err.message.starts_with("taker_no_crossing_liquidity:"),
        "want the no-crossing cause, got {:?}",
        err.message
    );
    assert!(
        err.message.contains("0.50"),
        "names the best ask: {}",
        err.message
    );
    // And the same reason is the one the panel/`engine.stats` reader sees.
    let stats = c.engine_stats();
    assert_eq!(stats["lastError"]["code"], "WOULD_CROSS");
    assert_eq!(stats["lastError"]["message"], err.message);
    assert_eq!(stats["lastError"]["tsMs"], 1);
    assert_eq!(c.ledger().balance(), dec!(10));
    assert_eq!(c.ledger().reserved(), dec!(0));
    assert!(c.positions().open_positions().is_empty());
}

#[test]
fn taker_insufficient_depth_is_rejected() {
    let mut c = dry_core(dec!(10));
    // Ask 0.40 crosses, but only 3 shares rest — an all-or-nothing FOK
    // cannot take 5, so the whole order is rejected.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(3))], 1);
    let out = c
        .place_outcome(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(out.status, OrderStatus::Rejected);
    let err = out
        .rejection
        .expect("a kernel refusal must carry its reason");
    assert_eq!(err.code, CoreErrorCode::WouldCross);
    // A DIFFERENT cause from the non-crossing case, and the message says
    // which one: a quote that moved is not a book too thin for our size.
    assert!(
        err.message.starts_with("taker_insufficient_depth:"),
        "want the depth cause, got {:?}",
        err.message
    );
    assert!(
        err.message.contains("only 3 of 5 shares"),
        "names the shortfall: {}",
        err.message
    );
    let stats = c.engine_stats();
    assert_eq!(stats["lastError"]["code"], "WOULD_CROSS");
    assert_eq!(stats["lastError"]["message"], err.message);
    assert_eq!(c.ledger().balance(), dec!(10));
    assert_eq!(c.ledger().reserved(), dec!(0));
    assert!(c.positions().open_positions().is_empty());
}

#[test]
fn a_risk_rejection_reaches_the_error_slot_with_its_own_reason() {
    // The third refusal kind a caller can hit (the observability gate drives
    // all three): the per-order notional cap. It reaches the caller as a
    // structured Err — and, since #180, it also refreshes the one error slot,
    // so the panel's answer to "why did nothing get placed?" is the same
    // reason the caller got, not a stale banner.
    let mut c = dry_core(dec!(10));
    c.risk_config_mut().max_order_notional = dec!(1);
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let err = c
        .place_outcome(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
        .expect_err("2.00 notional must be refused by the 1 USD per-order cap");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(
        err.message.contains("exceeds per-order cap"),
        "names the cap that refused it: {}",
        err.message
    );
    let stats = c.engine_stats();
    assert_eq!(stats["lastError"]["code"], "RISK_REJECTED");
    assert_eq!(stats["lastError"]["message"], err.message);
    assert_eq!(stats["lastError"]["tsMs"], 1);
    // Nothing reached the OME, and the reason is NOT one of the two liquidity
    // causes — the three refusal kinds stay distinguishable.
    assert!(c.list_orders().is_empty());
    assert!(
        !err.message.contains("taker_no_crossing_liquidity")
            && !err.message.contains("taker_insufficient_depth"),
        "a risk refusal must not read like a liquidity one: {}",
        err.message
    );
}

#[test]
fn a_filled_taker_leaves_no_error_and_a_maker_leg_reports_none() {
    // The other direction: the slot must stay EMPTY on the happy path (an
    // error slot that is always populated tells an operator nothing).
    let mut c = dry_core(dec!(10));
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let out = c
        .place_outcome(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(out.status, OrderStatus::Filled);
    assert!(out.rejection.is_none());
    assert_eq!(c.engine_stats()["lastError"], serde_json::Value::Null);

    // A resting maker is not a refusal either — it may fill later.
    let mut c = dry_core(dec!(10));
    let out = c
        .place_outcome(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k2"), 0, 1)
        .unwrap();
    assert_eq!(out.status, OrderStatus::Live);
    assert!(out.rejection.is_none());
    assert_eq!(c.engine_stats()["lastError"], serde_json::Value::Null);
}

#[test]
fn maker_rests_until_book_crosses() {
    let mut c = dry_core(dec!(10));
    // BUY maker 0.40*5 = 2.0 reserved, not filled yet.
    let (id, st) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(st, OrderStatus::Live);
    assert_eq!(c.ledger().available(), dec!(8));

    // Ask 0.45 does not cross a 0.40 bid → still live.
    c.book_snapshot("tok", vec![], vec![(dec!(0.45), dec!(100))], 2);
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);

    // Ask drops to 0.40 → fills at the resting 0.40.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 3);
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Filled);
    assert_eq!(c.ledger().balance(), dec!(8));
}

#[test]
fn maker_then_taker_escalates_after_timeout() {
    let mut c = dry_core(dec!(10));
    let (id, st) = c
        .place(
            order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
            1000,
            1,
        )
        .unwrap();
    assert_eq!(st, OrderStatus::Live);
    // Book never crosses within the maker window.
    c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2);
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);
    // At timeout: maker cancelled, taker order crosses immediately — at
    // the price the book is actually offering (0.50), not the rejected
    // passive limit.
    c.tick(1002).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
    // A new escalated taker order exists and is filled at the ask.
    let filled: Vec<_> = c
        .ome()
        .all()
        .into_iter()
        .filter(|o| o.status == OrderStatus::Filled)
        .collect();
    assert_eq!(filled.len(), 1);
    assert_eq!(filled[0].filled_size, dec!(5));
    assert_eq!(filled[0].price, dec!(0.50));
    assert_eq!(filled[0].avg_fill_price, Some(dec!(0.50)));
    // 10 - 0.5*5 = 7.5, minus the taker entry fee
    // (0.125 × (0.5·0.5)² × 5 = 0.0390625 — the E17 schedule, price-dependent).
    assert_eq!(c.ledger().balance(), dec!(7.4609375));
}

/// `--entry-maker-timeout-ms -1`: the entry is a pure maker. The book DOES
/// offer a crossing ask at the deadline — `maker_then_taker_escalates_after_timeout`
/// above fills at 0.50 under the same setup — and with the clock disarmed
/// nothing happens: no cancel, no taker leg, no fill, no money moved.
#[test]
fn a_negative_entry_timeout_never_escalates() {
    let mut c = dry_core(dec!(10));
    let before = c.ledger().balance();
    let (id, st) = c
        .place(
            order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
            -1,
            1,
        )
        .unwrap();
    assert_eq!(st, OrderStatus::Live);
    assert!(c.ome().get(&id).unwrap().escalate_at_ms.is_none());
    c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2);
    // Past the 5000 ms default too, not just past the deadline the old
    // fallback would have armed: if any clock existed at all, it has fired.
    c.tick(6000).unwrap();
    let o = c.ome().get(&id).unwrap();
    assert!(
        o.escalate_at_ms.is_none(),
        "a disarmed entry must never arm a clock, not even on the maintenance pass"
    );
    assert_eq!(o.status, OrderStatus::Live);
    assert!(
        c.ome().all().iter().all(|o| o.status == OrderStatus::Live),
        "the maker is still the only order; no taker leg was placed"
    );
    assert_eq!(c.ledger().balance(), before);
}

#[test]
fn escalation_delay_is_three_valued() {
    // 0 = the configured default (unchanged for every caller that passes
    // nothing); positive = that clock; negative = never escalate.
    assert_eq!(escalation_delay_ms(5_000, 5_000), Some(5_000));
    assert_eq!(escalation_delay_ms(0, 5_000), Some(5_000));
    assert_eq!(escalation_delay_ms(-1, 5_000), None);
    assert_eq!(escalation_delay_ms(-3_600_000, 5_000), None);
}

#[test]
fn escalation_without_crossing_depth_rearms_and_keeps_maker() {
    let mut c = dry_core(dec!(10));
    let (id, st) = c
        .place(
            order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
            1000,
            1,
        )
        .unwrap();
    assert_eq!(st, OrderStatus::Live);
    // No asks rest at all at the deadline: a taker leg has nothing to
    // cross into, so the maker stays and the clock re-arms.
    c.tick(1002).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);
    // Re-armed one maker timeout later (1001 − 1 = 1000 from now).
    assert_eq!(c.ome().get(&id).unwrap().escalate_at_ms, Some(2002));
    // No escalated order was spawned on the empty book.
    assert_eq!(
        c.ome()
            .all()
            .into_iter()
            .filter(|o| o.internal_key.ends_with(":escalated"))
            .count(),
        0
    );
    // Once the book does cross, the next due tick escalates and fills
    // at the ask.
    c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2003);
    c.tick(2003).unwrap();
    let filled: Vec<_> = c
        .ome()
        .all()
        .into_iter()
        .filter(|o| o.status == OrderStatus::Filled)
        .collect();
    assert_eq!(filled.len(), 1);
    assert_eq!(filled[0].avg_fill_price, Some(dec!(0.50)));
}

/// #261: an escalated leg the risk gate would refuse must not cost the
/// resting maker its life.
///
/// Before this fix the sweep cancelled the maker first and escalated
/// second, so a book that only offers depth above `cap / size` (3.00 / 5
/// shares = 0.60 here; a live 10-share lot under a 6.00 cap gives 0.60 the
/// same way) destroyed the entry outright AND failed the whole tick. On the
/// deployed dry stack, whose books offer a single 0.99 ask, that was every
/// spread_arb entry for a day and a half — zero fills, 15 aborted ticks.
#[test]
fn unaffordable_cross_keeps_the_maker_and_does_not_fail_the_tick() {
    let mut c = dry_core(dec!(10));
    let (id, st) = c
        .place(
            order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
            1000,
            1,
        )
        .unwrap();
    assert_eq!(st, OrderStatus::Live);
    // The only ask rests at 0.99: crossing 5 shares there costs 4.95, past
    // the 3.00 per-order cap, so the gate refuses the taker leg.
    c.book_snapshot("tok", vec![], vec![(dec!(0.99), dec!(100))], 2);
    c.tick(1002)
        .expect("an unaffordable cross must not abort the maintenance pass");
    let o = c.ome().get(&id).cloned().unwrap();
    assert_eq!(o.status, OrderStatus::Live, "the maker must stay resting");
    assert_eq!(o.escalate_at_ms, Some(2002), "the clock re-arms");
    assert_eq!(
        c.ome()
            .all()
            .into_iter()
            .filter(|o| o.internal_key.ends_with(":escalated"))
            .count(),
        0,
        "no escalated leg may be placed"
    );
    // The entry intent survived: once the book is affordable again, the
    // next due tick escalates and fills as a taker.
    c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2003);
    c.tick(2003).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
    let filled: Vec<_> = c
        .ome()
        .all()
        .into_iter()
        .filter(|o| o.status == OrderStatus::Filled)
        .collect();
    assert_eq!(filled.len(), 1);
    assert_eq!(filled[0].avg_fill_price, Some(dec!(0.50)));
}

/// The second half of #261: once the gate has passed, a placement can still
/// fail (here the reservation: the maker's own 2.00 was all the cash, and
/// the escalated 5 × 0.50 = 2.50 has nothing to reserve). The maker is
/// genuinely gone by then, but ONE dead escalation must not abort the
/// maintenance pass — the remaining due orders still have their turn.
#[test]
fn an_escalation_that_dies_after_the_maker_does_not_abort_the_pass() {
    let mut c = dry_core(dec!(2));
    let (id, st) = c
        .place(
            order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
            1000,
            1,
        )
        .unwrap();
    assert_eq!(st, OrderStatus::Live);
    c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2);
    c.tick(1002)
        .expect("a failed escalation must not abort the maintenance pass");
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
    assert_eq!(
        c.ome()
            .all()
            .into_iter()
            .filter(|o| o.status == OrderStatus::Filled)
            .count(),
        0,
        "nothing may fill: the escalation was refused"
    );
}

#[test]
fn cancel_releases_reservation() {
    let mut c = dry_core(dec!(10));
    let (id, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(c.ledger().available(), dec!(8));
    c.cancel(&id, 2).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
    assert_eq!(c.ledger().available(), dec!(10));
}
