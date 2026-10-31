use super::*;
use rust_decimal_macros::dec;

fn req(internal: &str, size: Decimal) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode: FillPolicy::Maker,
        price: dec!(0.4),
        size,
        internal_key: internal.into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 7,
        account_id: crate::model::default_account_id(),
    }
}

fn fill(order_id: &str, trade: &str, size: Decimal, price: Decimal, status: FillStatus) -> Fill {
    Fill {
        order_id: order_id.into(),
        trade_id: Some(trade.into()),
        token_id: "tok".into(),
        side: Side::Buy,
        price,
        size,
        status,
        ts_ms: 1000,
        tx_hash: None,
        // No venue report: the order's own fill policy decides the role.
        maker: None,
        account_id: crate::model::default_account_id(),
    }
}

#[test]
fn cumulative_deltas_idempotent_and_capped() {
    let mut ome = Ome::new();
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    ome.mark_live("o1", 2).unwrap();

    // First trade 6 @ 0.40 → +6
    let d1 = ome
        .apply_fill(
            fill("o1", "t1", dec!(6), dec!(0.40), FillStatus::Confirmed),
            3,
        )
        .unwrap()
        .unwrap();
    assert_eq!(d1.delta, dec!(6));
    assert_eq!(d1.price, dec!(0.40));
    assert_eq!(ome.get("o1").unwrap().status, OrderStatus::PartiallyFilled);

    // Duplicate CONFIRMED for same trade → no delta.
    assert!(
        ome.apply_fill(
            fill("o1", "t1", dec!(6), dec!(0.40), FillStatus::Confirmed),
            4
        )
        .unwrap()
        .is_none()
    );

    // Second trade pushes cumulative to 12 but order size caps at 10 → +4.
    let d2 = ome
        .apply_fill(
            fill("o1", "t2", dec!(12), dec!(0.42), FillStatus::Confirmed),
            5,
        )
        .unwrap()
        .unwrap();
    assert_eq!(d2.delta, dec!(4));
    assert_eq!(ome.get("o1").unwrap().filled_size, dec!(10));
    assert_eq!(ome.get("o1").unwrap().status, OrderStatus::Filled);
    // Weighted avg: (0.40*6 + 0.42*4)/10 = 0.408
    assert_eq!(ome.get("o1").unwrap().avg_fill_price, Some(dec!(0.408)));
}

#[test]
fn failed_fill_rolls_back_provisional() {
    let mut ome = Ome::new();
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    // MATCHED provisional +5
    let d = ome
        .apply_fill(fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Matched), 2)
        .unwrap()
        .unwrap();
    assert_eq!(d.delta, dec!(5));
    assert_eq!(ome.get("o1").unwrap().filled_size, dec!(5));
    // FAILED same trade → -5 rollback
    let r = ome
        .apply_fill(fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Failed), 3)
        .unwrap()
        .unwrap();
    assert_eq!(r.delta, dec!(-5));
    assert_eq!(ome.get("o1").unwrap().filled_size, dec!(0));
}

#[test]
fn unknown_fill_buffers_then_applies_after_submit() {
    let mut ome = Ome::new();
    // Fill arrives before order known → buffered, no delta.
    assert!(
        ome.apply_fill(
            fill("ghost", "t9", dec!(3), dec!(0.4), FillStatus::Confirmed),
            1
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(ome.pending_unknown_count(), 1);
    // Still unknown on drain → retained.
    assert!(ome.drain_pending(2).unwrap().is_empty());
    assert_eq!(ome.pending_unknown_count(), 1);
    // Order registered (resolution falls back to token+side) → buffered fill applies.
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 3,
        decision_at_ms: 3,
        execute_at_ms: 3,
        report_at_ms: 3,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    let drained = ome.drain_pending(4).unwrap();
    assert_eq!(drained.len(), 1);
    let d = drained[0].delta.as_ref().expect("delta projected");
    assert_eq!(d.delta, dec!(3));
    assert_eq!(ome.pending_unknown_count(), 0);
}

#[test]
fn duplicate_internal_key_rejected_while_live() {
    let mut ome = Ome::new();
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    assert!(
        ome.submit(SubmitParams {
            order_id: "o2".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 2,
            decision_at_ms: 2,
            execute_at_ms: 2,
            report_at_ms: 2,
            maker_timeout_ms: 0,
            round_duration_sec: 0,
        })
        .is_err()
    );
    ome.mark_terminal("o1", OrderStatus::Cancelled, 3).unwrap();
    // Same key reusable once terminal.
    assert!(
        ome.submit(SubmitParams {
            order_id: "o3".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 4,
            decision_at_ms: 4,
            execute_at_ms: 4,
            report_at_ms: 4,
            maker_timeout_ms: 0,
            round_duration_sec: 0,
        })
        .is_ok()
    );
}

// ── #178: restart idempotency ────────────────────────────────────────────

/// The acceptance criterion: after a restart (a fresh Ome hydrated from the
/// persisted journal), re-delivering the same trade id must NOT book a second
/// fill. Before the fix the table was memory-only, so the same trade was
/// applied twice and the position doubled.
#[test]
fn restored_applied_table_makes_redelivery_a_no_op() {
    let mut first = Ome::new();
    first
        .submit(SubmitParams {
            order_id: "o1".into(),
            request: req("k1", dec!(10)),
            submitted_at_ms: 1,
            decision_at_ms: 1,
            execute_at_ms: 1,
            report_at_ms: 1,
            maker_timeout_ms: 0,
            round_duration_sec: 0,
        })
        .unwrap();
    first.mark_live("o1", 2).unwrap();
    let d = first
        .apply_fill(
            fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Confirmed),
            3,
        )
        .unwrap()
        .expect("first fill books");
    assert_eq!(d.delta, dec!(4));

    // What the service would write to disk: the drained mutation journal.
    let journal = first.take_applied_journal();
    assert_eq_one_record(&journal, "t1", dec!(4));

    // "Restart": a brand-new OME with the order and the table restored.
    let mut second = Ome::new();
    second.restore(first.all().into_iter().cloned().collect());
    second.restore_applied(first.applied_snapshot());
    assert!(second.already_applied("t1"));

    // The venue re-delivers the same trade (its WS replay / REST sweep).
    let repeat = second
        .apply_fill(
            fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Confirmed),
            9,
        )
        .unwrap();
    assert!(repeat.is_none(), "re-delivered trade must not book again");
    assert_eq!(second.get("o1").unwrap().filled_size, dec!(4));
    assert_eq!(
        second.get("o1").unwrap().status,
        OrderStatus::PartiallyFilled
    );

    // The journal is also empty: nothing to persist for a duplicate.
    assert!(second.take_applied_journal().is_empty());
}

/// A late fill for a trade whose order is gone must not be re-booked onto an
/// unrelated live order that happens to share its token+side. `resolve` has
/// that token+side fallback, so without the pre-resolution duplicate check the
/// shares land on the wrong order.
#[test]
fn duplicate_trade_for_an_untracked_order_is_not_rebooked_elsewhere() {
    let mut ome = Ome::new();
    // The old order lives, gets a fill, then is dropped (terminal orders are
    // not restored after a restart).
    ome.submit(SubmitParams {
        order_id: "old".into(),
        request: req("k-old", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    ome.mark_live("old", 1).unwrap();
    ome.apply_fill(
        fill("old", "t7", dec!(4), dec!(0.4), FillStatus::Confirmed),
        2,
    )
    .unwrap();
    let table = ome.applied_snapshot();

    let mut restarted = Ome::new();
    restarted.restore_applied(table);
    // A fresh live order on the same token+side.
    restarted
        .submit(SubmitParams {
            order_id: "new".into(),
            request: req("k-new", dec!(10)),
            submitted_at_ms: 5,
            decision_at_ms: 5,
            execute_at_ms: 5,
            report_at_ms: 5,
            maker_timeout_ms: 0,
            round_duration_sec: 0,
        })
        .unwrap();
    restarted.mark_live("new", 5).unwrap();
    // The venue re-delivers t7 (addressed to the old venue id).
    let out = restarted
        .apply_fill(
            fill("old", "t7", dec!(4), dec!(0.4), FillStatus::Confirmed),
            6,
        )
        .unwrap();
    assert!(out.is_none());
    assert_eq!(
        restarted.get("new").unwrap().filled_size,
        Decimal::ZERO,
        "the duplicate must not be booked onto the new order"
    );
    assert_eq!(restarted.pending_unknown_count(), 0);
}

// ── #178: terminal absorption ────────────────────────────────────────────

/// The acceptance criterion: a Cancelled order that receives a late partial
/// fill keeps the terminal status, accrues the size correctly, and reports the
/// late fill. Before the fix the status was recomputed unconditionally, so the
/// order "came back to life" as PartiallyFilled.
#[test]
fn cancelled_order_absorbs_a_late_fill_and_stays_terminal() {
    let mut ome = Ome::new();
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    ome.mark_live("o1", 1).unwrap();
    ome.apply_fill(
        fill("o1", "t1", dec!(2), dec!(0.4), FillStatus::Confirmed),
        2,
    )
    .unwrap();
    ome.mark_terminal("o1", OrderStatus::Cancelled, 3).unwrap();

    let out = ome
        .apply_fill_report(
            fill("o1", "t2", dec!(3), dec!(0.4), FillStatus::Confirmed),
            4,
        )
        .unwrap();
    let d = out.delta.expect("the late fill still moves size");
    assert_eq!(d.delta, dec!(3));
    assert_eq!(d.cumulative, dec!(5));
    let late = out.late_fill.expect("a late fill is reported");
    assert_eq!(late.absorbed_by, Some(OrderStatus::Cancelled));
    assert_eq!(late.trade_id, "t2");
    assert_eq!(late.size, dec!(3));
    assert!(!late.overfill);
    let o = ome.get("o1").unwrap();
    assert_eq!(o.status, OrderStatus::Cancelled, "still terminal");
    assert_eq!(o.filled_size, dec!(5), "size is still accounted for");
    // And the size is still idempotent: a replay of t2 books nothing.
    assert!(
        ome.apply_fill(
            fill("o1", "t2", dec!(3), dec!(0.4), FillStatus::Confirmed),
            5
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(ome.get("o1").unwrap().filled_size, dec!(5));
}

/// A terminal order's venue id stays in `known_venue_ids`: a fill that
/// lands on the venue AFTER the core order went terminal (force-close,
/// ghost-close, cancel) must still be forwarded by the plugin's
/// FILLED-orders back-attribution channel — `apply_fill_report` absorbs
/// it as a late fill. Live-only ids here made those fills invisible.
#[test]
fn known_venue_ids_include_terminal_orders() {
    let mut ome = Ome::new();
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    ome.mark_live("o1", 1).unwrap();
    ome.bind_venue("o1", "venue-1".into(), 2).unwrap();
    assert!(ome.known_venue_ids().contains("venue-1"));

    ome.mark_terminal("o1", OrderStatus::Cancelled, 3).unwrap();
    assert!(
        ome.known_venue_ids().contains("venue-1"),
        "terminal venue id must stay known for late-fill attribution"
    );
}

/// A venue that reports more than the order could hold is capped today (that
/// part is correct) but was silently swallowed. It must surface as a LateFill
/// with `overfill` set — the operator needs to know the venue thinks more
/// shares exist than we asked for.
#[test]
fn over_reported_fill_is_capped_and_surfaced() {
    let mut ome = Ome::new();
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    ome.mark_live("o1", 1).unwrap();
    let out = ome
        .apply_fill_report(
            fill("o1", "t1", dec!(12), dec!(0.4), FillStatus::Confirmed),
            2,
        )
        .unwrap();
    let d = out.delta.unwrap();
    assert_eq!(d.delta, dec!(10), "capped at the order size");
    let late = out.late_fill.expect("the over-report is surfaced");
    assert!(late.overfill);
    assert_eq!(late.reported_cumulative, dec!(12));
    assert_eq!(late.cumulative, dec!(10));
    assert_eq!(late.order_size, dec!(10));
    assert_eq!(late.absorbed_by, None, "the order was still live");
    assert_eq!(ome.get("o1").unwrap().status, OrderStatus::Filled);
    // A further over-report on the now-Filled order: no size left to move,
    // still surfaced (and absorbed rather than resurrected).
    let again = ome
        .apply_fill_report(
            fill("o1", "t2", dec!(1), dec!(0.4), FillStatus::Confirmed),
            3,
        )
        .unwrap();
    assert!(again.delta.is_none());
    let late = again.late_fill.expect("still surfaced");
    assert!(late.overfill);
    assert_eq!(late.absorbed_by, Some(OrderStatus::Filled));
    assert_eq!(ome.get("o1").unwrap().status, OrderStatus::Filled);
}

/// A rollback (FAILED) on a terminal order must not be booked as a fresh
/// partial fill either: the cash/shares reverse, the status stays terminal.
#[test]
fn cancelled_order_absorbs_a_late_rollback() {
    let mut ome = Ome::new();
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    ome.mark_live("o1", 1).unwrap();
    ome.apply_fill(fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Matched), 2)
        .unwrap();
    ome.mark_terminal("o1", OrderStatus::Rejected, 3).unwrap();
    let out = ome
        .apply_fill_report(fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Failed), 4)
        .unwrap();
    assert_eq!(out.delta.unwrap().delta, dec!(-4));
    assert_eq!(ome.get("o1").unwrap().filled_size, Decimal::ZERO);
    assert_eq!(ome.get("o1").unwrap().status, OrderStatus::Rejected);
    assert_eq!(
        out.late_fill.unwrap().absorbed_by,
        Some(OrderStatus::Rejected)
    );
}

// ── #178: explicit LRU eviction ──────────────────────────────────────────

/// The acceptance criterion: after 5000+ evictions the EARLIEST ACTIVE trade
/// id is still idempotent. The old eviction used `HashMap::keys().next()` —
/// an arbitrary key — so it could drop the live order's entry and let its fill
/// be booked twice.
#[test]
fn eviction_is_lru_over_terminal_orders_and_never_drops_a_live_entry() {
    let mut ome = Ome::new();
    // One long-lived live order whose trade key is the OLDEST entry in the
    // table: exactly the entry the arbitrary-key eviction used to eat.
    ome.submit(SubmitParams {
        order_id: "live".into(),
        request: req("k-live", dec!(1000)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    ome.mark_live("live", 1).unwrap();
    ome.apply_fill(
        fill("live", "t-live", dec!(1), dec!(0.4), FillStatus::Confirmed),
        2,
    )
    .unwrap();

    // 5001 more trades, each on its own short-lived order that ends terminal.
    for i in 0..5001 {
        let id = format!("o{i}");
        ome.submit(SubmitParams {
            order_id: id.clone(),
            request: req(&format!("k{i}"), dec!(1)),
            submitted_at_ms: 10,
            decision_at_ms: 10,
            execute_at_ms: 10,
            report_at_ms: 10,
            maker_timeout_ms: 0,
            round_duration_sec: 0,
        })
        .unwrap();
        ome.mark_live(&id, 10).unwrap();
        ome.apply_fill(
            fill(
                &id,
                &format!("t{i}"),
                dec!(1),
                dec!(0.4),
                FillStatus::Confirmed,
            ),
            10,
        )
        .unwrap();
        ome.mark_terminal(&id, OrderStatus::Filled, 10).unwrap();
    }

    assert!(ome.applied_len() <= APPLIED_CAP + 1);
    assert!(
        ome.already_applied("t-live"),
        "the live order's key must survive eviction"
    );
    // And it really is idempotent: a replay books nothing.
    assert!(
        ome.apply_fill(
            fill("live", "t-live", dec!(1), dec!(0.4), FillStatus::Confirmed),
            11
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(ome.get("live").unwrap().filled_size, dec!(1));
    // The oldest terminal-order keys are the ones that went.
    assert!(!ome.already_applied("t0"));
}

/// A rollback to zero keeps the key (with size 0) so the trade can still be
/// re-applied if the venue later confirms it — the table must not treat a
/// zeroed key as "never seen".
#[test]
fn rolled_back_key_is_retained_but_zeroed() {
    let mut ome = Ome::new();
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    ome.mark_live("o1", 1).unwrap();
    ome.apply_fill(fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Matched), 2)
        .unwrap();
    ome.apply_fill(fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Failed), 3)
        .unwrap();
    assert!(!ome.already_applied("t1"));
    // A later CONFIRMED for the same id is a fresh application, not a no-op.
    let again = ome
        .apply_fill(
            fill("o1", "t1", dec!(5), dec!(0.4), FillStatus::Confirmed),
            4,
        )
        .unwrap();
    assert_eq!(again.unwrap().delta, dec!(5));
    assert_eq!(ome.get("o1").unwrap().filled_size, dec!(5));
}

/// `size` is total, `filled_size` cumulative: the remaining size a cancel
/// retires is their difference.
#[test]
fn remaining_size_is_size_minus_filled() {
    let mut ome = Ome::new();
    ome.submit(SubmitParams {
        order_id: "o1".into(),
        request: req("k1", dec!(10)),
        submitted_at_ms: 1,
        decision_at_ms: 1,
        execute_at_ms: 1,
        report_at_ms: 1,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
    ome.mark_live("o1", 1).unwrap();
    assert_eq!(ome.remaining("o1"), dec!(10));
    ome.apply_fill(
        fill("o1", "t1", dec!(4), dec!(0.4), FillStatus::Confirmed),
        2,
    )
    .unwrap();
    assert_eq!(ome.remaining("o1"), dec!(6));
    assert_eq!(ome.get("o1").unwrap().size, dec!(10), "size is unchanged");
    assert_eq!(ome.remaining("ghost"), Decimal::ZERO);
}

/// A restored journal is folded in file order: later records win, tombstones
/// remove, and the restored table evicts in the same order the writer would.
#[test]
fn journal_folds_later_records_and_tombstones() {
    let mut ome = Ome::new();
    ome.restore_applied(vec![
        AppliedFillRecord {
            trade_key: "t1".into(),
            order_id: "o1".into(),
            applied: dec!(2),
            price: dec!(0.4),
            seq: 1,
            evicted: false,
        },
        // The venue later confirmed t1 at a larger size: later wins.
        AppliedFillRecord {
            trade_key: "t1".into(),
            order_id: "o1".into(),
            applied: dec!(5),
            price: dec!(0.4),
            seq: 2,
            evicted: false,
        },
        AppliedFillRecord {
            trade_key: "t2".into(),
            order_id: "o2".into(),
            applied: dec!(1),
            price: dec!(0.5),
            seq: 3,
            evicted: false,
        },
        // ...and t2 was evicted afterwards.
        AppliedFillRecord {
            trade_key: "t2".into(),
            order_id: "o2".into(),
            applied: dec!(1),
            price: dec!(0.5),
            seq: 3,
            evicted: true,
        },
    ]);
    assert!(ome.already_applied("t1"));
    assert!(!ome.already_applied("t2"));
    assert_eq!(ome.applied_len(), 1);
    let snap = ome.applied_snapshot();
    assert_eq!(snap[0].trade_key, "t1");
    assert_eq!(snap[0].applied, dec!(5));
    assert!(snap[0].seq > 0, "restored entries get explicit sequences");
}

fn assert_eq_one_record(journal: &[AppliedFillRecord], key: &str, applied: Decimal) {
    assert_eq!(journal.len(), 1, "one mutation journalled per fill");
    assert_eq!(journal[0].trade_key, key);
    assert_eq!(journal[0].applied, applied);
    assert!(!journal[0].evicted);
    assert!(journal[0].seq > 0);
}

#[test]
fn query_views_stay_submitted_ordered_without_resorting() {
    let mut ome = Ome::new();
    // Insert out of submission order — the views must not care.
    for (id, ts) in [("o2", 2i64), ("o1", 1), ("o3", 3)] {
        ome.submit(SubmitParams {
            order_id: id.into(),
            request: req(&format!("k-{id}"), dec!(10)),
            submitted_at_ms: ts,
            decision_at_ms: ts,
            execute_at_ms: ts,
            report_at_ms: ts,
            maker_timeout_ms: 0,
            round_duration_sec: 0,
        })
        .unwrap();
    }
    let ids: Vec<_> = ome.all().iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(
        ids,
        ["o1", "o2", "o3"],
        "all() orders by submitted_at_ms, not insertion"
    );

    // A fully filled order leaves the live views with no re-sorting.
    ome.mark_live("o2", 3).unwrap();
    ome.apply_fill(
        fill("o2", "t1", dec!(10), dec!(0.40), FillStatus::Confirmed),
        5,
    )
    .unwrap()
    .unwrap();
    assert_eq!(ome.get("o2").unwrap().status, OrderStatus::Filled);
    let live: Vec<_> = ome
        .live_orders()
        .iter()
        .map(|o| o.order_id.as_str())
        .collect();
    assert_eq!(live, ["o1", "o3"]);
    assert!(
        ome.live_for("tok", Side::Buy)
            .iter()
            .all(|o| o.order_id != "o2")
    );

    // Restore is idempotent per order id: a re-restore whose persisted
    // timestamp changed must re-key the index, never duplicate the order.
    let snapshot: Vec<TrackedOrder> = ome.all().into_iter().cloned().collect();
    let mut fresh = Ome::new();
    fresh.restore(snapshot.clone());
    let mut again = snapshot;
    again[0].submitted_at_ms += 1_000; // o1's persisted timestamp moved
    fresh.restore(again);
    let ids: Vec<_> = fresh.all().iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(
        ids,
        ["o2", "o3", "o1"],
        "re-restore re-keys, no duplicates, new order wins"
    );
}

fn sell_req(internal: &str, size: Decimal, token: &str) -> OrderRequest {
    OrderRequest {
        side: Side::Sell,
        token_id: token.into(),
        ..req(internal, size)
    }
}

fn sell_fill(
    order_id: &str,
    trade: &str,
    size: Decimal,
    price: Decimal,
    status: FillStatus,
) -> Fill {
    Fill {
        side: Side::Sell,
        ..fill(order_id, trade, size, price, status)
    }
}

fn submit_at(ome: &mut Ome, id: &str, request: OrderRequest, ts: i64) {
    ome.submit(SubmitParams {
        order_id: id.into(),
        request,
        submitted_at_ms: ts,
        decision_at_ms: ts,
        execute_at_ms: ts,
        report_at_ms: ts,
        maker_timeout_ms: 0,
        round_duration_sec: 0,
    })
    .unwrap();
}

#[test]
fn live_index_stays_in_sync_through_transitions() {
    let mut ome = Ome::new();
    submit_at(&mut ome, "o1", req("k1", dec!(10)), 1);
    submit_at(&mut ome, "o2", sell_req("k2", dec!(10), "tok"), 2);
    // Both are born Pending — and Pending IS live — so both live views
    // carry them from birth.
    let live: Vec<_> = ome
        .live_orders()
        .iter()
        .map(|o| o.order_id.as_str())
        .collect();
    assert_eq!(live, ["o1", "o2"], "submit inserts into the live index");

    // A fill that completes an order retires it from the live views, and
    // all() still carries the full history table.
    ome.mark_live("o1", 3).unwrap();
    ome.apply_fill(
        fill("o1", "t1", dec!(10), dec!(0.40), FillStatus::Confirmed),
        4,
    )
    .unwrap()
    .unwrap();
    assert_eq!(ome.get("o1").unwrap().status, OrderStatus::Filled);
    assert!(ome.live_orders().iter().all(|o| o.order_id != "o1"));
    assert_eq!(ome.all().len(), 2);

    // An explicit terminal transition retires too (mark_terminal is the
    // cancel/reject/fail road in).
    ome.mark_terminal("o2", OrderStatus::Cancelled, 5).unwrap();
    assert!(
        ome.live_orders().is_empty(),
        "both retired: {:?}",
        ome.live_orders()
    );

    // A fresh submit is live again without any transition.
    submit_at(&mut ome, "o3", req("k3", dec!(10)), 6);
    let live: Vec<_> = ome
        .live_orders()
        .iter()
        .map(|o| o.order_id.as_str())
        .collect();
    assert_eq!(live, ["o3"]);
    // The restore path rebuilds BOTH indexes, live membership included.
    let snapshot: Vec<TrackedOrder> = ome.all().into_iter().cloned().collect();
    let mut fresh = Ome::new();
    fresh.restore(snapshot);
    let live: Vec<_> = fresh
        .live_orders()
        .iter()
        .map(|o| o.order_id.as_str())
        .collect();
    assert_eq!(live, ["o3"], "restore rebuilds the live index");
    assert_eq!(fresh.all().len(), 3);
}

#[test]
fn live_for_scopes_to_token_and_token_index_rebuilds_on_restore() {
    let mut ome = Ome::new();
    submit_at(&mut ome, "b1", req("kb1", dec!(10)), 1);
    submit_at(&mut ome, "s1", sell_req("ks1", dec!(10), "tok"), 2);
    let mut other = req("ko1", dec!(10));
    other.token_id = "tok2".into();
    submit_at(&mut ome, "o2", other, 3);

    fn ids(v: Vec<&TrackedOrder>) -> Vec<&str> {
        v.iter().map(|o| o.order_id.as_str()).collect()
    }
    assert_eq!(ids(ome.live_for("tok", Side::Buy)), ["b1"]);
    assert_eq!(ids(ome.live_for("tok", Side::Sell)), ["s1"]);
    assert_eq!(ids(ome.live_for("tok2", Side::Buy)), ["o2"]);
    assert!(ome.live_for("tok3", Side::Buy).is_empty());

    // Reverse acceptance: an order entering a terminal state must leave
    // its token bucket — a dead order must never answer live_for.
    ome.mark_live("b1", 4).unwrap();
    ome.apply_fill(
        fill("b1", "tb1", dec!(10), dec!(0.40), FillStatus::Confirmed),
        5,
    )
    .unwrap()
    .unwrap();
    assert!(
        ome.live_for("tok", Side::Buy).is_empty(),
        "a filled buy leaves the token index"
    );
    ome.mark_terminal("s1", OrderStatus::Cancelled, 6).unwrap();
    assert!(
        ome.live_for("tok", Side::Sell).is_empty(),
        "a cancelled sell leaves the token index"
    );
    assert_eq!(
        ids(ome.live_for("tok2", Side::Buy)),
        ["o2"],
        "other tokens' buckets untouched"
    );

    // Restore rebuilds the token index from the snapshot...
    let snapshot: Vec<TrackedOrder> = ome.all().into_iter().cloned().collect();
    let mut fresh = Ome::new();
    fresh.restore(snapshot.clone());
    assert_eq!(ids(fresh.live_for("tok2", Side::Buy)), ["o2"]);
    // ...and a re-restore whose persisted timestamp moved must re-key the
    // bucket (index_remove drops the stale key) instead of double-yielding
    // the order. again[2] is o2 — the one order still live here.
    let mut again = snapshot;
    again[2].submitted_at_ms += 1_000;
    fresh.restore(again);
    assert_eq!(
        ids(fresh.live_for("tok2", Side::Buy)),
        ["o2"],
        "re-restore re-keys the token bucket, no duplicates"
    );
}

#[test]
fn filled_sell_completion_index_answers_the_residual_backstop() {
    let mut ome = Ome::new();
    // A sell on `tok` completes at now_ms=100: the backstop's "did a sell
    // on this token complete at/after entry?" is answered from the index.
    submit_at(&mut ome, "s1", sell_req("ks1", dec!(10), "tok"), 10);
    ome.mark_live("s1", 11).unwrap();
    ome.apply_fill(
        sell_fill("s1", "ts1", dec!(10), dec!(0.60), FillStatus::Confirmed),
        100,
    )
    .unwrap()
    .unwrap();
    assert!(
        ome.has_filled_sell_since("tok", 100),
        "completed AT 100 counts"
    );
    assert!(
        ome.has_filled_sell_since("tok", 55),
        "entry before the fill counts"
    );
    assert!(
        !ome.has_filled_sell_since("tok", 101),
        "strictly-after entry does not"
    );
    assert!(!ome.has_filled_sell_since("never-traded", 0));

    // A completed BUY on the same token must NOT register as a sell
    // completion (the backstop only looks for sells).
    submit_at(&mut ome, "b1", req("kb1", dec!(10)), 110);
    ome.mark_live("b1", 111).unwrap();
    ome.apply_fill(
        fill("b1", "tb1", dec!(10), dec!(0.40), FillStatus::Confirmed),
        200,
    )
    .unwrap()
    .unwrap();
    assert!(
        !ome.has_filled_sell_since("tok", 150),
        "a buy completion does not pollute the sell index"
    );

    // A CANCELLED sell registers nothing: only Filled sells complete.
    submit_at(&mut ome, "s2", sell_req("ks2", dec!(10), "tok3"), 120);
    ome.mark_live("s2", 121).unwrap();
    ome.mark_terminal("s2", OrderStatus::Cancelled, 130)
        .unwrap();
    assert!(!ome.has_filled_sell_since("tok3", 0));

    // Restore rebuilds the completion index from the snapshot so a
    // position whose sell completed before a restart still gets the
    // E31-b residual re-close after it.
    let snapshot: Vec<TrackedOrder> = ome.all().into_iter().cloned().collect();
    let mut fresh = Ome::new();
    fresh.restore(snapshot);
    assert!(
        fresh.has_filled_sell_since("tok", 55),
        "rebuilt from restore"
    );
    assert!(!fresh.has_filled_sell_since("tok", 101));
}
