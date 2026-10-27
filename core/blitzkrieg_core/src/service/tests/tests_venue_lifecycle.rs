#![cfg(test)]

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

fn fill(order_id: &str, size: Decimal, price: Decimal, maker: Option<bool>) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some("t1".into()),
        token_id: "tok".into(),
        side: Side::Buy,
        price,
        size,
        status: FillStatus::Confirmed,
        ts_ms: 10,
        tx_hash: None,
        maker,
        account_id: crate::model::default_account_id(),
    }
}

#[test]
fn cancel_of_a_venue_bound_order_queues_a_real_venue_cancel() {
    let mut c = live_core(dec!(10));
    // E31-a: the venue has not acked yet (no id) — nothing to forward.
    let (id, st) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(st, OrderStatus::Pending);
    c.cancel(&id, 2).unwrap();
    assert!(c.take_pending_venue_cancels().is_empty());

    // The venue accepts: id bound, order live, ledger still reserved.
    let (id, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k2"), 0, 3)
        .unwrap();
    c.bind_venue(&id, "0xv1".into(), 4).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);

    c.cancel(&id, 5).unwrap();
    // A local-only retire would leave the venue resting: the id rides the
    // cancel queue for the live bridge to DELETE for real.
    assert_eq!(c.take_pending_venue_cancels(), vec!["0xv1".to_string()]);
    assert!(c.take_pending_venue_cancels().is_empty());
}

#[test]
fn confirm_live_arms_escalation_from_the_bound_maker_timeout() {
    let mut c = live_core(dec!(10));
    let (id, _) = c
        .place(
            order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
            5_000,
            1,
        )
        .unwrap();
    assert!(c.ome().get(&id).unwrap().escalate_at_ms.is_none());
    c.confirm_live(&id, 100).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().escalate_at_ms, Some(5_100));

    // Zero timeout falls back to the configured default window.
    let (id2, _) = c
        .place(
            order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k3"),
            0,
            2,
        )
        .unwrap();
    c.confirm_live(&id2, 200).unwrap();
    assert_eq!(
        c.ome().get(&id2).unwrap().escalate_at_ms,
        Some(200 + c.config.default_maker_timeout_ms)
    );

    // A negative timeout is the off switch: the venue's acceptance arms no
    // clock at all, so the leg rests as a pure maker.
    let (id3, _) = c
        .place(
            order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k5"),
            -1,
            3,
        )
        .unwrap();
    c.confirm_live(&id3, 300).unwrap();
    assert!(c.ome().get(&id3).unwrap().escalate_at_ms.is_none());
}

#[test]
fn cancel_remaining_keeps_filled_shares_and_retires_the_remainder() {
    let mut c = live_core(dec!(10));
    let (id, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    c.bind_venue(&id, "0xv1".into(), 2).unwrap();
    // Partial fill: 2 of 5.
    c.ingest_fill(fill(&id, dec!(2), dec!(0.40), None), 3)
        .unwrap();
    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.status, OrderStatus::PartiallyFilled);
    assert_eq!(o.filled_size, dec!(2));

    c.cancel_remaining(&id, 4).unwrap();
    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.status, OrderStatus::Cancelled);
    // The filled part is NOT refunded — it is real inventory now.
    assert_eq!(c.positions().open_positions().len(), 1);
    // And the venue-side delete rides the queue.
    assert_eq!(c.take_pending_venue_cancels(), vec!["0xv1".to_string()]);
}

#[test]
fn close_filled_flattens_only_the_shares_behind_the_order() {
    let mut c = dry_core(dec!(10));
    // Maker BUY 5 rests; a partial fill accrues 2 real shares.
    let (id, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    c.ingest_fill(fill(&id, dec!(2), dec!(0.40), None), 3)
        .unwrap();
    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.status, OrderStatus::PartiallyFilled);

    // Unknown orders / positions without inventory are errors, not panics.
    assert_eq!(
        c.close_filled("nope", 4).unwrap_err().code,
        CoreErrorCode::UnknownOrder
    );

    // A bid exists → the flatten SELL crosses and closes the position.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.44), dec!(100))],
        vec![(dec!(0.50), dec!(100))],
        4,
    );
    assert_eq!(c.close_filled(&id, 5).unwrap(), 1);
    assert!(c.positions().open_positions().is_empty());
}

#[test]
fn three_consecutive_sweep_failures_freeze_trading() {
    let mut c = live_core(dec!(10));
    let err = crate::model::CoreError::new(
        blitzkrieg_market_api::CoreErrorCode::VenueError,
        "venue down",
    );
    c.on_reconcile_failed(err.clone(), 1);
    c.on_reconcile_failed(err.clone(), 2);
    assert!(
        !c.risk.is_killed(),
        "one or two failures are noise, not a freeze"
    );
    c.on_reconcile_failed(err, 3);
    assert!(c.risk.is_killed());
    let e = c
        .place(order(FillPolicy::Taker, dec!(0.4), dec!(1), "k1"), 0, 4)
        .unwrap_err();
    assert_eq!(e.code, CoreErrorCode::KillSwitchActive);
}

#[test]
fn a_successful_sweep_resets_the_failure_streak() {
    let mut c = live_core(dec!(10));
    let err = crate::model::CoreError::new(
        blitzkrieg_market_api::CoreErrorCode::VenueError,
        "venue down",
    );
    c.on_reconcile_failed(err.clone(), 1);
    c.on_reconcile_failed(err, 2);
    // A sweep that RUNS (empty snapshot is fine) proves the channel alive.
    let snap = crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 3,
    };
    c.reconcile(snap).unwrap();
    c.on_reconcile_failed(
        crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::VenueError,
            "venue down again",
        ),
        4,
    );
    assert!(
        !c.risk.is_killed(),
        "streak must count CONSECUTIVE failures only"
    );
}

/// A terminal status the sweep stamps (ghost vanished → force-closed) must
/// land in orders.jsonl in the same pass, or a restart restores the dead
/// order as LIVE and force-closes it all over again — the zombie revival
/// the polysimulator upgrade restart exhibited (live_7/9/11 resurrected,
/// re-force-closed 14s after boot).
#[test]
fn sweep_marked_terminal_orders_are_persisted() {
    let dir = std::env::temp_dir().join(format!("bk-recon-persist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("orders.jsonl");
    let mut c = Core::new(CoreConfig {
        order_log_path: Some(log.to_string_lossy().into_owned()),
        risk: RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: Decimal::from(10_000),
        ..Default::default()
    });
    // A live order bound to a venue id; the snapshot's open list omits it,
    // so the sweep marks it ForceClosed (ghost).
    let (id, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    c.bind_venue(&id, "venue-1".to_string(), 2).expect("bind");
    let snap = crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 3,
    };
    let report = c.reconcile(snap).unwrap();
    assert_eq!(
        report
            .actions
            .iter()
            .filter(|a| matches!(
                a,
                crate::reconcile::ReconcileAction::MarkedForceClosed { .. }
            ))
            .count(),
        1,
        "the ghost was force-closed"
    );
    // The persisted tail must say terminal NOW — not only after some other
    // code path happens to persist the order later.
    let rows: Vec<crate::model::TrackedOrder> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let last = rows
        .iter()
        .rev()
        .find(|o| o.order_id == id)
        .expect("order was persisted");
    assert_eq!(
        last.status,
        OrderStatus::ForceClosed,
        "terminal status reached orders.jsonl in the sweep pass"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn risk_and_ledger_reject_oversize_and_kill() {
    let mut c = dry_core(dec!(10));
    // notional 0.4*10 = 4 > cap 3 → rejected before reservation.
    let e = c
        .place(order(FillPolicy::Taker, dec!(0.4), dec!(10), "k1"), 0, 1)
        .unwrap_err();
    assert_eq!(e.code, CoreErrorCode::RiskRejected);

    // Balance gate: only 2 available after a 2 reservation → a 3 buy fails.
    let mut c2 = dry_core(dec!(2));
    let e2 = c2
        .place(order(FillPolicy::Taker, dec!(0.3), dec!(10), "k2"), 0, 1)
        .unwrap_err();
    assert_eq!(e2.code, CoreErrorCode::InsufficientFunds);

    let mut c3 = dry_core(dec!(10));
    c3.kill("manual".into());
    let e3 = c3
        .place(order(FillPolicy::Taker, dec!(0.4), dec!(1), "k3"), 0, 1)
        .unwrap_err();
    assert_eq!(e3.code, CoreErrorCode::KillSwitchActive);
}

#[test]
fn buy_fill_opens_position_and_exit_closes_it() {
    let mut c = dry_core(dec!(100));
    // Taker BUY 0.40 x 5 fills → position opened. A FOK taker needs
    // resting ask depth to fill.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 0);
    let (id, st) = c
        .place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(st, OrderStatus::Filled);
    assert_eq!(c.positions().open_positions().len(), 1);

    // Push a book showing a large profit → tick must emit a closing SELL.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.99), dec!(100))],
        vec![(dec!(1.0), dec!(100))],
        2,
    );
    c.tick(2).unwrap();
    // Exit SELL (taker) crosses immediately at 0.99 → position closed.
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "position should be closed"
    );
    assert_eq!(c.positions().closed_positions().len(), 1);
    let closed = &c.positions().closed_positions()[0];
    assert!(
        closed.net_pnl_usd > Decimal::ZERO,
        "expected profit, got {}",
        closed.net_pnl_usd
    );
    let _ = id;
}

#[test]
fn forced_exit_closes_at_a_loss() {
    let mut c = dry_core(dec!(100));
    // Pins the guillotine ON (mechanism test — the force-exit deadline
    // closing at a loss); the calibrated default ships it OFF.
    let mut pc = c.config.positions.clone();
    pc.exit.force_exit_sec = 120;
    c.positions.set_config(pc);
    // Buy at 0.40 (round_slot 1 → expires at the END of slot 1 = 1_800_000ms).
    // A FOK taker needs resting ask depth to fill.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 0);
    c.place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 0)
        .unwrap();
    assert_eq!(c.positions().open_positions().len(), 1);
    // Push a lower book, then tick at the force-exit horizon (100s left).
    c.book_snapshot(
        "tok",
        vec![(dec!(0.30), dec!(100))],
        vec![(dec!(0.31), dec!(100))],
        1_800_000 - 1_000,
    );
    c.tick(1_800_000 - 100).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "force exit should close the position"
    );
    let closed = &c.positions().closed_positions()[0];
    assert!(
        closed.net_pnl_usd < Decimal::ZERO,
        "expected a loss, got {}",
        closed.net_pnl_usd
    );
}

/// 退出纪律可编辑（`risk.setExit`）：写路径的**两处落点**必须同时到位 ——
/// 已开仓位的退出扫描读 `PositionManager` 自己的 config 副本（每 tick
/// 克隆），新入场绑定读 `config.positions.exit`。只写 service 侧会让退出
/// 扫描静默沿用旧纪律（「新仓位一种纪律、旧仓位另一种」的分裂）；只写
/// manager 侧会让 readout（risk.limits）撒谎。这里用已开仓位 + 收紧的
/// 止损证明扫描真的跟随新值：12% 止损按住不触发，热改 1% 后下一个
/// tick 触发。
#[test]
fn exit_set_reaches_the_exit_sweep_of_already_open_positions() {
    let mut c = dry_core(dec!(100));
    // The dynamic-stop ramp needs time_left <= 300 to reach the floor; a
    // static judgement would blend with the ramp. force_exit OFF keeps the
    // position alive for the second phase.
    let mut pc = c.config.positions.clone();
    pc.exit.force_exit_sec = 0;
    c.positions.set_config(pc);

    // Open at 0.40 (FOK taker needs resting ask depth).
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 0);
    c.place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 0)
        .unwrap();
    assert_eq!(c.positions().open_positions().len(), 1);

    // Price drops 8% (0.40 → 0.368): inside the shipped 12% stop, no exit.
    // time_left 600s keeps the dynamic ramp at its wide end; 8% stays above
    // the ramped threshold, so the position survives — the JUDGEMENT that
    // holds it is the 12 we are about to edit.
    // #462: the snapshot ts is the book's RECEIVED time now — put it 1s
    // before the tick so the freshness gate judges a book the fixture just
    // delivered, not one delivered at epoch+1ms.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.368), dec!(100))],
        vec![(dec!(0.37), dec!(100))],
        1_800_000 - 601_000,
    );
    c.tick(1_800_000 - 600_000).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "an 8% drawdown must not trigger the 12% stop"
    );

    // THE WRITE: tighten the stop to 1% through the verb's service path.
    c.apply_exit_set(
        &crate::ipc::schema::SetExitParams {
            stop_loss_pct: Some(dec!(1)),
            ..Default::default()
        },
        "test",
        0,
    )
    .unwrap();

    // Same drawdown, next tick: the sweep now judges on 1% and fires.
    c.tick(1_800_000 - 599_000).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "after the hot edit the exit sweep must judge on the NEW stop"
    );

    // The readout agrees (config.positions.exit moved too).
    let exit = &c.config.positions.exit;
    assert_eq!(exit.stop_loss_pct, dec!(1), "the readout must not lie");
}

#[test]
fn entry_is_blocked_by_daily_loss_limit() {
    let mut c = dry_core(dec!(100));
    // Shrink the daily loss cap so one losing trade trips it. Pins the
    // guillotine ON: this test needs the expiring position CLOSED at a loss
    // to move `daily_pnl`, and the calibrated default ships the guillotine
    // OFF (expiry belongs to settlement).
    let mut pc = c.config.positions.clone();
    pc.max_daily_loss_usd = dec!(1);
    pc.exit.force_exit_sec = 120;
    pc.asset_cooldown_sec = 0;
    pc.loss_cooldown_sec = 0;
    pc.stop_loss_cooldown_sec = 0;
    pc.exit_cooldown_sec = 0;
    c.positions.set_config(pc);
    // A FOK taker entry needs resting ask depth to fill.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 0);
    c.place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 0)
        .unwrap();
    c.book_snapshot(
        "tok",
        vec![(dec!(0.20), dec!(100))],
        vec![(dec!(0.21), dec!(100))],
        1_800_000 - 1_000,
    );
    c.tick(1_800_000 - 100).unwrap();
    assert!(c.positions().daily_pnl_for("default") < dec!(-1));
    let e = c
        .place(
            order(FillPolicy::Taker, dec!(0.40), dec!(5), "k2"),
            0,
            900_000,
        )
        .unwrap_err();
    assert_eq!(e.code, CoreErrorCode::RiskRejected);
    assert!(
        e.message.to_lowercase().contains("daily"),
        "got {}",
        e.message
    );
}

/// KI-10 / D-18 option A end-to-end: three losing closes in ONE strategy must
/// freeze only that strategy. The old global breaker vetoed every other leg's
/// entries (TREND_FOLLOW_HOLDOUT_REPORT §3.2: `ordersRejected=655`).
#[test]
fn a_losing_streak_freezes_only_its_own_strategy() {
    let mut c = dry_core(dec!(100));
    let mut pc = c.config.positions.clone();
    pc.max_positions = 4;
    pc.max_daily_loss_usd = dec!(1_000); // isolate the consecutive-loss breaker
    // Pins the guillotine ON: each leg's loss must CLOSE at expiry to feed
    // the consecutive-loss breaker; the calibrated default ships it OFF
    // (expiry belongs to settlement).
    pc.exit.force_exit_sec = 120;
    pc.stop_loss_cooldown_sec = 0;
    pc.exit_cooldown_sec = 0;
    pc.asset_cooldown_sec = 0;
    pc.loss_cooldown_sec = 0;
    c.positions.set_config(pc);

    let buy = |strategy: &str, asset: &str, token: &str| OrderRequest {
        token_id: token.into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.40),
        size: dec!(5),
        internal_key: format!("{strategy}:{asset}"),
        strategy: strategy.into(),
        asset: asset.into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    };
    let lose_once = |c: &mut Core, asset: &str, token: &str| {
        // The entry is a FOK taker: it needs resting ask depth to fill.
        c.book_snapshot(token, vec![], vec![(dec!(0.40), dec!(100))], 0);
        c.place(buy("dog", asset, token), 0, 0).unwrap();
        c.book_snapshot(
            token,
            vec![(dec!(0.30), dec!(100))],
            vec![(dec!(0.31), dec!(100))],
            1_800_000 - 1_000,
        );
        c.tick(1_800_000 - 100).unwrap();
    };
    // Three distinct assets, so per-asset cooldowns cannot mask the breaker.
    for asset in ["A1", "A2", "A3"] {
        lose_once(&mut c, asset, &format!("tok-{asset}"));
    }
    assert_eq!(c.breaker.consecutive_losses("dog"), 3);
    assert!(
        c.breaker.is_halted("dog", 1_800_000),
        "the losing strategy's own breaker must trip"
    );
    assert!(
        !c.breaker.is_halted("trend_follow", 1_800_000),
        "an unrelated strategy must NOT be frozen by another leg's streak"
    );

    // The frozen leg is refused, and the refusal names it (the panel's
    // `risk:breaker` attribution matches on "breaker active until").
    let e = c
        .place(buy("dog", "A4", "tok-A4"), 0, 1_800_000)
        .unwrap_err();
    assert_eq!(e.code, CoreErrorCode::RiskRejected);
    assert!(
        e.message.contains("breaker active until") && e.message.contains("dog"),
        "got {}",
        e.message
    );
    // A DIFFERENT strategy enters normally through the same core.
    c.place(buy("trend_follow", "B1", "tok-B1"), 0, 1_800_000)
        .unwrap();
}

/// #461 Fix 2 reverse acceptance: the escalation chase may not push the
/// pair's PER-SHARE cost past the $1.00 a share-pair redeems. A 0.60/share
/// DOWN leg plus a 0.99 chase prices the pair at 1.59 — the merge would
/// lock in a loss — so the guard refuses the chase, the maker stays
/// resting, and the clock re-arms instead of destroying the entry. Every
/// OTHER gate passes the chase (2 × 0.99 = 1.98 ≤ the 3.00 notional cap,
/// equity is ample), so the pair-cost cap is the only thing that can have
/// refused it.
#[test]
fn an_escalation_chase_that_would_price_the_pair_over_a_dollar_is_refused() {
    let mut c = dry_core(dec!(10));
    // The counterpart leg: 2 DOWN shares at 0.60 → basis 0.60/share.
    c.book_snapshot("tok-down", vec![], vec![(dec!(0.60), dec!(100))], 1);
    let down = OrderRequest {
        token_id: "tok-down".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.60),
        size: dec!(2),
        internal_key: "k-down".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "down".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    };
    let (_id, st) = c.place(down, 0, 1).unwrap();
    assert_eq!(st, OrderStatus::Filled);
    let after_counterpart = c.ledger().balance();

    // The maker leg prices the pair at exactly 0.60 + 0.40 = 1.00 — the
    // boundary PASSES (a pair that redeems a dollar may cost the dollar).
    let outcome = c
        .place_inner(
            order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(2), "k-up"),
            1000,
            1,
            false,
        )
        .unwrap();
    assert_eq!(outcome.status, OrderStatus::Live);
    let id = outcome.order_id;

    // The only ask rests at 0.99: a naive escalation crosses it and
    // prices the pair at 1.59.
    c.book_snapshot("tok", vec![], vec![(dec!(0.99), dec!(100))], 2);
    c.tick(1002).unwrap();
    let o = c.ome().get(&id).cloned().unwrap();
    assert_eq!(o.status, OrderStatus::Live, "the maker stays resting");
    assert_eq!(o.escalate_at_ms, Some(2002), "the clock re-arms");
    assert!(
        c.ome()
            .all()
            .iter()
            .all(|o| !o.internal_key.ends_with(":escalated")),
        "no escalated leg may exist"
    );
    // Only the counterpart fill moved money; the chase bought nothing.
    assert_eq!(c.ledger().balance(), after_counterpart);
    assert_eq!(c.positions().open_positions().len(), 1);
}

/// #461 Fix 3 reverse acceptance: a RESTING BUY order is cash promised —
/// the exposure cap must see it before, not after, it becomes a position.
/// With `max_total_exposure_usd` armed at 3.00, a resting 2.00 bid plus a
/// fresh 2.00 entry is 4.00: the second entry is refused while the bid
/// rests, and admitted once the bid is gone.
#[test]
fn a_resting_bid_counts_toward_the_exposure_cap() {
    let mut c = dry_core(dec!(10));
    c.risk_config_mut().systemic.global.max_total_exposure_usd =
        crate::risk::limits::Bound::new(dec!(3), crate::risk::limits::LimitSource::Toml);

    // A 0.40 × 5 = 2.00 bid rests (no ask to cross it).
    let (id, st) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(st, OrderStatus::Live);

    // A fresh 2.00 entry on top would commit 4.00 — past the cap. Before
    // the fix the resting bid was invisible to the aggregates and this
    // entry sailed through.
    let e = c
        .place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k2"), 0, 2)
        .unwrap_err();
    assert_eq!(e.code, CoreErrorCode::RiskRejected);
    assert!(
        e.message.contains("max_total_exposure_usd"),
        "got {}",
        e.message
    );

    // The refusal was the cap, not broken plumbing: once the bid is gone
    // the same-sized entry is admitted.
    c.cancel(&id, 3).unwrap();
    let (_id2, st) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k3"), 0, 4)
        .unwrap();
    assert_eq!(st, OrderStatus::Live);
}

/// #461 Fix 4 reverse acceptance: `orders.place` has no bypass around the
/// systemic nine. With `max_total_position` armed at 1 and one position
/// open, a manual entry on an asset with NO position (so the one-per-asset
/// gate has nothing to say) is still refused — by the systemic gate, in
/// its own vocabulary.
#[test]
fn a_manual_entry_over_an_armed_systemic_limit_is_refused() {
    let mut c = dry_core(dec!(10));
    c.risk_config_mut().systemic.global.max_total_position =
        crate::risk::limits::Bound::new(dec!(1), crate::risk::limits::LimitSource::Toml);

    // One open position (taker fill at 0.40).
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, st) = c
        .place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(st, OrderStatus::Filled);

    // A manual entry on ETH: no ETH position exists, so the ONLY gate
    // that can refuse it is the systemic one.
    let mut eth = order(FillPolicy::Taker, dec!(0.40), dec!(5), "k2");
    eth.asset = "ETH".into();
    eth.token_id = "tok-eth".into();
    c.book_snapshot("tok-eth", vec![], vec![(dec!(0.40), dec!(100))], 2);
    let e = c.place(eth, 0, 2).unwrap_err();
    assert_eq!(e.code, CoreErrorCode::RiskRejected);
    assert!(
        e.message.contains("max_total_position"),
        "got {}",
        e.message
    );
    assert_eq!(c.positions().open_positions().len(), 1);
}

/// #461 Fix 4, the Reduce arm at the submission gate: an armed
/// `max_position_size` SHRINKS a manual entry in place, exactly as the
/// pipeline shrinks a strategy entry — the order the venue sees is the
/// approved size, not the requested one.
#[test]
fn an_armed_size_cap_shrinks_a_manual_entry_at_the_submission_gate() {
    let mut c = dry_core(dec!(10));
    c.risk_config_mut().systemic.account.max_position_size =
        crate::risk::limits::Bound::new(dec!(5), crate::risk::limits::LimitSource::Toml);

    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
    let (_id, st) = c
        .place(order(FillPolicy::Taker, dec!(0.40), dec!(7), "k1"), 0, 1)
        .unwrap();
    assert_eq!(st, OrderStatus::Filled);
    let placed = &c.ome().all()[0];
    assert_eq!(placed.size, dec!(5), "the cap shrunk 7 shares to 5");
    assert_eq!(placed.filled_size, dec!(5));
    assert_eq!(c.positions().open_positions()[0].shares, dec!(5));
}
