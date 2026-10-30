#![cfg(test)]
//! #462 验收：订单状态机与盘口时间戳修复的正向判据。helper 自带（兄弟
//! mod 不共享 `tests` 的私有夹具），变异有牙由 PR 前的手工验证完成。

use super::*;
use crate::risk::RiskConfig;
use rust_decimal_macros::dec;

/// Dry core, seed `balance`, the shipped per-order notional cap 3.
fn dry_core(balance: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: Decimal::from(10_000),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

/// Live-mode core: placements stay Pending until a venue ack.
fn live_core(balance: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        },
        dry_seed_balance: Decimal::from(10_000),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

fn order(mode: FillPolicy, price: Decimal, size: Decimal, key: &str) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode,
        price,
        size,
        internal_key: key.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

/// A CryptoMarket fed through the engine so the reconciler can look up
/// metadata for an external fill's token.
fn feed_market(c: &mut Core, up_token: &str, down_token: &str, now: i64) {
    let slot = now / 1000 / 900;
    c.engine_on_data(
        crate::engine::DataEvent::RoundMarkets {
            markets: vec![crate::model::CryptoMarket {
                asset: "BTC".into(),
                condition_id: "cond".into(),
                question_id: "q".into(),
                up_token_id: up_token.into(),
                down_token_id: down_token.into(),
                up_price: dec!(0.6),
                down_price: dec!(0.4),
                expires_at_ms: (slot + 1) * 900 * 1000,
                round_slot: slot,
                round_duration_sec: 900,
                archive_verdict: false,
                venue: String::new(),
                neg_risk: false,
                question: "?".into(),
            }],
            now_ms: now,
        },
        now,
    );
}

/// #462 fix 3, acceptance: a book whose receive time is past the exit
/// ladder's `max_book_age_sec` budget must NOT price an exit — the stop
/// stays silent instead of acting on a quote nobody is standing behind.
///
/// The position opens at the 0.40 ask and NO new book ever arrives. The
/// one book's bid (0.35, −12.5% off entry) is already past the shipped
/// 12% stop, so only the
/// freshness gate can hold the exit back — before the fix the mirrored
/// timestamp was the host's tick clock, the stale book read fresh, and
/// this tick would have sold into a bid the feed stopped carrying 3
/// hours ago.
#[test]
fn an_exit_is_not_priced_off_a_stale_book() {
    let cfg = CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        auto_exits_enabled: true,
        ..Default::default()
    };
    let mut c = Core::new(cfg);
    c.enable_engine(crate::engine::Engine::new(
        crate::engine::EngineConfig::default(),
    ));
    let now = 1_000_000_000i64;
    feed_market(&mut c, "tok", "tok-down", now);
    c.engine_on_data(
        crate::engine::DataEvent::Book {
            token_id: "tok".into(),
            // The ask prices the entry (the FOK fills at the 0.40 ask);
            // the bid is the crash the exit ladder WOULD act on if it
            // trusted this book forever: 0.35 is −12.5% off the 0.40
            // entry (past the shipped 12% stop) and only a 6.7% wick
            // (under the 8% guard), so the freshness gate is the ONLY
            // thing that can hold the exit back.
            bids: vec![(dec!(0.35), dec!(100))],
            asks: vec![(dec!(0.40), dec!(100))],
            now_ms: now,
        },
        now,
    );
    let req = crate::model::OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Taker,
        price: dec!(0.45),
        size: dec!(5),
        internal_key: "entry".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: now / 1000 / 900,
        account_id: crate::model::default_account_id(),
    };
    let (_, st) = c.place(req, 0, now).unwrap();
    assert_eq!(st, OrderStatus::Filled);

    // 90 s later, still no book: the mirrored snapshot's receive time is
    // the ONLY timestamp the ladder may judge on, and it is now past the
    // 60 s budget (the round has 13.5 min left, so the expiry gate is not
    // what holds — the freshness gate is). The exit must hold.
    c.tick(now + 90 * 1000).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "a stale book must not price an exit (#462)"
    );
    assert_eq!(
        c.positions().closed_positions().len(),
        0,
        "nothing was sold off the stale quote"
    );
}

/// #462 fix 3, acceptance (escalation side): the SAME freshness budget
/// gates the escalation chase. The maker rests; the only book it ever
/// sees is hours old by the time the clock fires; the sweep must NOT
/// authorize a chase priced off it — the maker stays and the clock
/// re-arms, exactly like the no-depth branch.
#[test]
fn an_escalation_is_not_chased_off_a_stale_book() {
    let mut c = dry_core(dec!(10));
    // #457: the expiry sweep closes an order whose round has ended, so this
    // fixture's long clock jump needs a round that outlives the jump —
    // slot for a round still open at the tick instant.
    let long_round_dur = 4 * 3600 + 60; // covers the 3h+ jump below
    let slot = (1 + 2 * 1000) / 1000 / long_round_dur;
    let outcome = c
        .place_inner(
            OrderRequest {
                round_slot: slot,
                ..order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(2), "k1")
            },
            1000,
            1,
            false,
        )
        .unwrap();
    assert_eq!(outcome.status, OrderStatus::Live);
    let id = outcome.order_id;

    // Fresh at t=1 (a 0.99 ask would authorize the chase), but this is the
    // ONLY snapshot the book ever sees.
    c.book_snapshot("tok", vec![], vec![(dec!(0.99), dec!(100))], 1);

    // The escalation clock fires hours after the book's receive time.
    c.tick(1 + 3 * 3600 * 1000 + 2).unwrap();
    let o = c.ome().get(&id).cloned().unwrap();
    assert_eq!(o.status, OrderStatus::Live, "the maker stays resting");
    assert_eq!(
        o.escalate_at_ms,
        Some(1 + 3 * 3600 * 1000 + 2 + 1000),
        "the clock re-arms (no-depth semantics)"
    );
    assert_eq!(
        c.ome()
            .all()
            .into_iter()
            .filter(|o| o.internal_key.ends_with(":escalated"))
            .count(),
        0,
        "a stale book must not authorize a chase (#462)"
    );
}

/// #462 fix 2, acceptance: a place TIMEOUT must not release the cash
/// reservation — the venue may have accepted the order. The order stays
/// Pending with its reservation held; `reject_live` (the old path) is
/// what releases.
#[test]
fn a_place_timeout_keeps_the_order_pending_with_its_reservation() {
    let mut c = live_core(dec!(10));
    let (id, st) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(st, OrderStatus::Pending);
    let reserved_before = c.ledger().reserved();

    c.note_place_timeout(&id);

    let o = c.ome().get(&id).cloned().unwrap();
    assert_eq!(
        o.status,
        OrderStatus::Pending,
        "a timeout is not a rejection — reconciliation decides (#462)"
    );
    assert_eq!(
        c.ledger().reserved(),
        reserved_before,
        "the reservation must stay held while the venue's answer is unknown"
    );
    // Contrast: the actual rejection path still releases (unchanged).
    c.reject_live(&id, 2).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Rejected);
    assert_eq!(c.ledger().reserved(), Decimal::ZERO);
}

/// P0 (v0.4.2 audit): a timed-out place must LEAVE the executor drain.
///
/// `take_pending_orders` is a live VIEW over [`Core::pending_unbound`], not a
/// consuming queue, so before this flag the bridge re-submitted the same
/// order on every 500 ms tick for the whole 30 s retirement window. Polymarket
/// signs a brand-new order per POST (no client-id dedupe), so a timed-out POST
/// that had actually filled was filled a SECOND time — real money, real
/// exposure. The flag keeps the order Pending (its reservation stays held, the
/// #462 sweep still owns it) but takes it out of the re-submit view.
///
/// Negative: on the pre-fix code `pending_unbound()` still returns the order.
#[test]
fn a_place_timeout_leaves_the_executor_drain() {
    let mut c = live_core(dec!(10));
    let (timed_out, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    let (healthy, _) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k2"), 0, 2)
        .unwrap();

    c.note_place_timeout(&timed_out);

    let drained: Vec<String> = c
        .pending_unbound()
        .into_iter()
        .map(|o| o.order_id.clone())
        .collect();
    assert!(
        !drained.contains(&timed_out),
        "a timed-out place must not be handed to the executor again"
    );
    assert!(
        drained.contains(&healthy),
        "an order whose POST is still in flight must keep draining"
    );
    assert_eq!(
        c.ome().get(&timed_out).unwrap().status,
        OrderStatus::Pending,
        "still Pending: the 30 s retirement sweep decides, not the drain"
    );
}

/// #462 fix 2, acceptance: an external BUY the venue really filled (a
/// timed-out place) is folded into the position book through the SAME
/// ledger+position path every fill takes — shares exist afterwards, the
/// cash moved, and the position carries real market metadata.
#[test]
fn an_external_buy_from_a_timed_out_place_rebuilds_the_position() {
    let cfg = CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        round_duration_sec: 900,
        ..Default::default()
    };
    let mut c = Core::new(cfg);
    c.enable_engine(crate::engine::Engine::new(
        crate::engine::EngineConfig::default(),
    ));
    c.set_balance(dec!(1000));
    let now = 1_000_000_000i64;
    feed_market(&mut c, "tok", "tok-down", now);

    let balance_before = c.ledger().balance();
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![crate::reconcile::VenueTrade {
            venue_order_id: "venue-unknown-1".into(),
            trade_id: "t-ext-1".into(),
            token_id: "tok".into(),
            side: Side::Buy,
            size: dec!(5),
            price: dec!(0.45),
            ts_ms: now + 1,
            tx_hash: None,
            maker: Some(false),
        }],
        now_ms: now + 2,
    })
    .unwrap();

    let pos = c
        .positions()
        .open_positions()
        .iter()
        .find(|p| p.token_id == "tok")
        .expect("the external BUY must rebuild a position (#462)");
    assert_eq!(pos.shares, dec!(5));
    assert_eq!(pos.condition_id, "cond");
    assert!(
        c.ledger().balance() < balance_before,
        "the cash the venue spent must leave the ledger"
    );
}

/// #462 fix 2, acceptance (the closer): a timed-out place the venue never
/// received must not pend forever — the sweep retires it past the budget
/// and the reservation is released. Without this judge, the timeout
/// branch's "stays pending" leaks the cash for the life of the process.
#[test]
fn a_never_acked_timed_out_place_is_retired_by_the_sweep() {
    let mut c = live_core(dec!(10));
    let (id, st) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
        .unwrap();
    assert_eq!(st, OrderStatus::Pending);
    c.note_place_timeout(&id);
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Pending);

    // Inside the budget: the sweep leaves it alone.
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 10,
    })
    .unwrap();
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Pending,
        "inside the budget the POST may still land — no verdict yet"
    );

    // Past the budget: retired, reservation released.
    c.reconcile(crate::reconcile::VenueSnapshot {
        open_order_ids: vec![],
        trades: vec![],
        now_ms: 10 + PLACE_TIMEOUT_VERDICT_MS,
    })
    .unwrap();
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Cancelled,
        "no venue evidence past the budget — the intent is retired (#462)"
    );
    assert_eq!(
        c.ledger().reserved(),
        Decimal::ZERO,
        "the reservation must be released with the intent"
    );
}

/// #462 fix 1, acceptance (kernel-side evidence): the escalation's
/// cancel-then-place is serialized through ONE queue the bridge drains
/// before it places anything new — `take_pending_venue_cancels` yields
/// the venue DELETE ahead of every POST of the same tick, because the
/// bridge's loop retires cancels FIRST (its step 1) and only then
/// submits pending orders (step 1b, skipped entirely when a cancel just
/// failed).
#[test]
fn the_escalation_cancel_is_forwarded_before_any_new_order() {
    let mut c = live_core(dec!(10));
    // A resting maker bound to a venue id, cancelled the way the
    // escalation sweep would: the DELETE rides the queue the bridge
    // drains ahead of any placement.
    let (id, _st) = c
        .place(order(FillPolicy::Maker, dec!(0.40), dec!(2), "k1"), 0, 1)
        .unwrap();
    c.bind_venue(&id, "venue-resting-1".into(), 2).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);

    c.cancel(&id, 3).unwrap();
    assert_eq!(
        c.take_pending_venue_cancels(),
        vec!["venue-resting-1".to_string()],
        "the escalation's cancel rides the queue the bridge drains before placing (#462)"
    );
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
}

// ── #457: ForceClosed — ghosts die on the clock ─────────────────────────

fn fc_core(round_duration_sec: i64) -> Core {
    Core::new(CoreConfig {
        round_duration_sec,
        ..Default::default()
    })
}

/// A resting maker whose round slot N covers [N·dur, (N+1)·dur) — the
/// same shape `place` writes for a real engine entry.
fn fc_resting_maker(c: &mut Core, slot: i64, now: i64, key: &str) -> String {
    let req = OrderRequest {
        round_slot: slot,
        ..order(FillPolicy::Maker, dec!(0.40), dec!(2), key)
    };
    let (id, _st) = c.place(req, 0, now).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);
    id
}

/// #457 matrix: every path that ends an order lands in a DISTINCT terminal
/// status, so the audit trail can tell the three "who closed it" stories
/// apart. ForceClosed is terminal, not live, and is NOT Cancelled.
#[test]
fn force_closed_is_terminal_and_distinct_from_cancelled() {
    let statuses = [
        OrderStatus::Filled,
        OrderStatus::Cancelled,
        OrderStatus::Rejected,
        OrderStatus::Failed,
        OrderStatus::ForceClosed,
    ];
    for s in statuses {
        assert!(s.is_terminal(), "{s:?} must be terminal");
        assert!(!s.is_live(), "{s:?} must not be live");
    }
    for s in [
        OrderStatus::Pending,
        OrderStatus::Live,
        OrderStatus::PartiallyFilled,
    ] {
        assert!(!s.is_terminal(), "{s:?} must not be terminal");
    }
    assert_ne!(OrderStatus::ForceClosed, OrderStatus::Cancelled);
}

/// #457, expiry sweep: a resting order whose round has ended is
/// force-closed by the tick — reservation released, terminal status
/// ForceClosed, no honest receipt ever needed.
#[test]
fn the_expiry_sweep_force_closes_an_order_on_an_expired_round() {
    let mut c = fc_core(900);
    // Slot 1 covers [900s, 1800s). Place at 901s (inside), sweep at
    // 1_800_000ms (past the end).
    let id = fc_resting_maker(&mut c, 1, 901_000, "k1");
    c.tick(1_700_000).unwrap();
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Live,
        "inside the round the order stays"
    );
    c.tick(1_800_000).unwrap();
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::ForceClosed,
        "the kernel closed an order nobody else closed (#457)"
    );
    assert_eq!(
        c.ledger().reserved(),
        Decimal::ZERO,
        "the buy reservation must be released with the ghost"
    );
    // Idempotent: a second sweep over the terminal order does nothing.
    c.tick(1_900_000).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::ForceClosed);
}

/// #457: the manual verb refuses a STILL-LIVE round — dodging a cancel
/// through force-close is exactly what the state machine must not allow.
#[test]
fn force_close_refuses_a_still_live_round() {
    let mut c = fc_core(900);
    let id = fc_resting_maker(&mut c, 1, 901_000, "k1");
    let err = c
        .force_close_order(&id, 1_000_000, "manual test")
        .unwrap_err();
    assert_eq!(err.code, CoreErrorCode::InvalidOrderState);
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Live,
        "the refusal leaves the order untouched"
    );
    // After the round ends, the same verb succeeds.
    c.force_close_order(&id, 1_800_000, "manual test").unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::ForceClosed);
}

/// #457: a round_slot 0 order carries no expiry — the sweep must leave it
/// alone (0 means "no round" on every manual/test path that stamps it).
#[test]
fn the_expiry_sweep_ignores_slot_zero_orders() {
    let mut c = fc_core(900);
    let id = fc_resting_maker(&mut c, 0, 1000, "k1");
    c.tick(10_000_000).unwrap();
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Live,
        "slot 0 has no derivable expiry — only the sweep's clock rules, never the sweep"
    );
}

/// #457: the live venue's resting order still gets its DELETE queued, so
/// the real venue converges with the book even after the kernel gives up.
#[test]
fn force_close_queues_the_venue_cancel_for_a_bound_order() {
    let mut c = live_core(dec!(10));
    let req = OrderRequest {
        round_slot: 1,
        ..order(FillPolicy::Maker, dec!(0.40), dec!(2), "k1")
    };
    let (id, _st) = c.place(req, 0, 1_000).unwrap();
    c.bind_venue(&id, "venue-ghost".into(), 2).unwrap();
    // Force-close past the round end.
    c.force_close_order(&id, 1_800_000, "round expired")
        .unwrap();
    assert_eq!(
        c.take_pending_venue_cancels(),
        vec!["venue-ghost".to_string()],
        "the kernel still asks the venue to retire its copy — evidence-gathering (#457)"
    );
}

/// #457: the sweep never runs under the replay scheduler — the replayed
/// order set is a tradesSha256 input and replayed archives carry no
/// live-clock ghosts.
#[test]
fn the_expiry_sweep_is_a_no_op_under_replay_scheduling() {
    let mut c = Core::new(CoreConfig {
        round_duration_sec: 900,
        replay_scheduling: true,
        ..Default::default()
    });
    let id = fc_resting_maker(&mut c, 1, 901_000, "k1");
    c.tick(10_000_000).unwrap();
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Live,
        "replay owns every transition through its own flows (#457)"
    );
}
