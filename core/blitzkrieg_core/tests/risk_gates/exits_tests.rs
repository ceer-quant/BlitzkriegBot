use crate::support::*;

// ── #174: the kill switch freezes entries, never the way out ────────────────

#[test]
fn the_kill_switch_freezes_entries_but_never_the_exit() {
    let mut c = dry_core(PositionConfig::default());
    open_long(&mut c, "tok", dec!(0.39), dec!(0.40), dec!(10), NOW);
    c.kill("test: venue refusals".into());
    assert!(c.is_killed());

    // (a) A new entry is frozen, with the kill switch's own error code.
    let err = c
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + 1_000,
            ),
            0,
            NOW + 1_000,
        )
        .expect_err("a killed core must refuse new exposure");
    assert_eq!(err.code, CoreErrorCode::KillSwitchActive);

    // (b) The exemption is for CLOSING INTENTS, not for the SELL side: an
    // ordinary sell intent is still blocked, so the hatch cannot be abused to
    // hand exposure to someone else's key.
    let err = c
        .place(
            order(
                "tok",
                Side::Sell,
                dec!(0.38),
                dec!(10),
                "k-other",
                NOW + 2_000,
            ),
            0,
            NOW + 2_000,
        )
        .expect_err("a non-closing sell must stay blocked while killed");
    assert_eq!(err.code, CoreErrorCode::KillSwitchActive);

    // (c) The real way out — the operator's flatten — goes through and fills.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.38), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW + 3_000,
    );
    assert_eq!(
        c.flatten(None, NOW + 3_000).unwrap(),
        1,
        "the closing sell must be submitted"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "the closing sell must have completed, not just been accepted"
    );
    // …and it must not have lifted the freeze it was exempt from.
    assert!(c.is_killed(), "closing a position is not a resume");
}

// ── #177: no-wick / missing-book stop, plus the withheld-trigger report ─────

#[test]
fn a_pin_bar_is_withheld_and_reported_then_a_real_decline_fires_the_stop() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut c = dry_core(PositionConfig::default());
    c.set_event_sink(tx);
    open_long(&mut c, "tok", dec!(0.39), dec!(0.40), dec!(10), NOW);
    let _ = drain_alerts(&mut rx); // the round-start chatter is not the subject

    // A pin bar 5s in: the bid craters to -50% while the mid holds at entry.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.20), dec!(1000))],
        vec![(dec!(0.60), dec!(1000))],
        NOW + 5_000,
    );
    c.tick(NOW + 5_000).unwrap();
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "a pin bar must not close the position"
    );
    assert!(
        !c.list_orders()
            .iter()
            .any(|o| o.side == Side::Sell && o.status.is_live()),
        "no closing sell may be resting off a pin bar"
    );
    // …but it must be REPORTED: "should have triggered" is an event, not a
    // silent no-op.
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts
            .iter()
            .any(|m| m.contains("SUPPRESSED") && m.contains("wick guard")),
        "the withheld stop must reach the event sink: {alerts:?}"
    );
    assert_eq!(c.positions().suppressed_stop_count(), 1);
    let stats = c.engine_stats_at(NOW + 5_000);
    assert_eq!(stats["dailyLoss"]["suppressedStops"].as_u64(), Some(1));

    // 5s later the decline is real: the bid AND the mid collapse together, so
    // no wick ratio can excuse holding through it.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.05), dec!(1000))],
        vec![(dec!(0.06), dec!(1000))],
        NOW + 10_000,
    );
    c.tick(NOW + 10_000).unwrap();
    assert!(
        c.positions().open_positions().is_empty(),
        "a real decline must fire the stop"
    );
}

fn book(
    bids: Vec<(Decimal, Decimal)>,
    asks: Vec<(Decimal, Decimal)>,
    now: i64,
) -> OrderbookSnapshot {
    // Built the way the kernel builds it from the mirrored book, so the
    // placeholder prices an empty side gets are part of the test.
    OrderbookSnapshot::from_levels("tok", bids, asks, now)
}

#[test]
fn the_stop_survives_a_dead_quote_a_missing_book_and_a_collapsed_mid() {
    // (a) No bid at all (the book was swept). A one-sided book now has no mid
    // either — `from_levels` zeroes it, because `(0 + ask)/2` is an arithmetic
    // artifact, not a level anyone will lift. So there is no buyer to sell to
    // AND no trustworthy mid to judge the stop on.
    //
    // Two things must both hold, and F6's answer to the first must not swallow
    // the second:
    //   • no SELL — minting one out of the phantom mid is what made the old
    //     code "book profit no buyer was offering";
    //   • no SILENCE — the ask is still a real level, and a stop that judged on
    //     it and was breached must be reported (#225; the fuller matrix lives
    //     in `a_one_sided_book_still_judges_the_protective_stop`).
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(vec![], vec![(dec!(0.15), dec!(1000))], NOW + 5_000),
            ))
        },
        NOW + 5_000,
    );
    assert!(
        reqs.is_empty(),
        "a bid-less book must never mint a SELL out of the mid or the ask"
    );
    let events = pm.drain_suppressed_stops();
    assert_eq!(
        events.len(),
        1,
        "and it must not be silent either: the held stop is an event"
    );
    assert_eq!(events[0].cause, StopSuppressionCause::NoExecutableQuote);
    assert_eq!(events[0].mid, dec!(0.15), "judged on the ask side");

    // (b) No book at all, but a FRESH last valid price: the stop's judgement
    // stands — the position is underwater and the rule says get out — but a
    // price is not a bid, so the judgement still cannot become an order. No
    // order, AND no silence: the held exit is reported.
    let mut pm = PositionManager::new(PositionConfig::default());
    let id = position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    pm.tick(
        &id,
        Some(&book(
            vec![(dec!(0.15), dec!(1000))],
            vec![(dec!(0.16), dec!(1000))],
            NOW + 3_000,
        )),
        NOW + 3_000,
    );
    let reqs = pm.check_exits(&|_| None, NOW + 4_000);
    assert!(
        reqs.is_empty(),
        "a missing book must not mint a SELL either"
    );
    let events = pm.drain_suppressed_stops();
    assert_eq!(
        events.len(),
        1,
        "the held stop must be reported, not swallowed"
    );
    assert_eq!(events[0].cause, StopSuppressionCause::NoExecutableQuote);
    assert_eq!(events[0].bid, Decimal::ZERO, "there was no bid to report");
    assert_eq!(
        events[0].mid,
        dec!(0.15),
        "the last known price is what the stop judged on"
    );
    assert_eq!(events[0].pnl_pct_at_mid, dec!(-62.5));
    assert!(events[0].message().contains("no executable bid"));
    assert_eq!(
        pm.suppressed_stop_count(),
        1,
        "the panel counter moves for a held stop too"
    );
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "a drain must be a drain"
    );

    // (b') …but the same price, aged past the freshness bound, must not: a
    // stale quote cannot speak for the market.
    let mut pm = PositionManager::new(PositionConfig::default());
    let id = position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    pm.tick(
        &id,
        Some(&book(
            vec![(dec!(0.15), dec!(1000))],
            vec![(dec!(0.16), dec!(1000))],
            NOW + 3_000,
        )),
        NOW + 3_000,
    );
    let stale_at = NOW + 3_000 + 31_000;
    assert!(
        pm.check_exits(&|_| None, stale_at).is_empty(),
        "a last price older than the bound must not fire the stop"
    );

    // (c) A real decline that ALSO trips the wick ratio: the bid hangs at 0.05
    // against a 0.075 ask (20% wick) but the MID collapsed with it, so the
    // guard has nothing to excuse and the stop fires.
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(
                    vec![(dec!(0.05), dec!(1000))],
                    vec![(dec!(0.075), dec!(1000))],
                    NOW + 5_000,
                ),
            ))
        },
        NOW + 5_000,
    );
    assert_eq!(reqs.len(), 1, "a collapsed mid outranks the wick guard");
    assert_eq!(reqs[0].reason, ExitReason::StopLoss);
}

// ── #225: a one-sided book must not silence the protective stop ─────────────

/// #225: a book with no bid has no mid either (F6 zeroes it — `(0 + ask)/2` is
/// arithmetic, not a level), so `stop_reference` fell straight through to a
/// remembered price. That price is the ENTRY until a two-sided book updates it,
/// which made a swept, deeply underwater position look exactly like a quiet
/// market: no order, no alert, no trace. The operator could not tell "nothing is
/// happening" from "I wanted to stop out and nobody would buy".
///
/// A bid-less book still carries a REAL level — the ask. It cannot price a SELL
/// (that stays `executable_bid`'s job, and it stays zero here), but it is
/// evidence about where the market is, and the protective stop is the one rule
/// that must keep judging when the quote is gone (#177). So: judge on it, place
/// nothing, and REPORT the withheld exit.
///
/// The three shapes a book can take for a LONG opened at 0.40 with a 12% stop:
#[test]
fn a_one_sided_book_still_judges_the_protective_stop() {
    // (a) ASKS ONLY — the buy side was swept and the ask collapsed with it.
    // No order can be priced (there is no buyer at any price we can name), but
    // the stop must still be JUDGED, on the one real level left, and the held
    // exit reported instead of vanishing.
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(vec![], vec![(dec!(0.15), dec!(1000))], NOW + 5_000),
            ))
        },
        NOW + 5_000,
    );
    assert!(
        reqs.is_empty(),
        "a bid-less book must never mint a SELL — not even for a breached stop"
    );
    let events = pm.drain_suppressed_stops();
    assert_eq!(
        events.len(),
        1,
        "the stop judged on a real level and was breached: holding it MUST be reported"
    );
    assert_eq!(events[0].cause, StopSuppressionCause::NoExecutableQuote);
    assert_eq!(
        events[0].bid,
        Decimal::ZERO,
        "there was no bid to price an order against"
    );
    assert_eq!(
        events[0].mid,
        dec!(0.15),
        "the report must carry the ask-side price the stop actually judged on, \
         not the stale entry-level reference"
    );
    assert_eq!(events[0].pnl_pct_at_mid, dec!(-62.5));
    assert_eq!(events[0].stop_pct, dec!(12));
    assert!(events[0].message().contains("no executable bid"));
    assert_eq!(
        pm.suppressed_stop_count(),
        1,
        "the panel counter must move too"
    );
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "a drain must be a drain"
    );

    // (b) ASKS ONLY, but the ask never collapsed: one-sided is not the same as
    // crashed. The ask is ABOVE the last known price, so the conservative
    // `min(last_known, best_ask)` must judge on the last known price and stay
    // quiet — a swept buy side is not by itself evidence of a decline.
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(vec![], vec![(dec!(0.90), dec!(1000))], NOW + 5_000),
            ))
        },
        NOW + 5_000,
    );
    assert!(reqs.is_empty(), "no bid ⇒ nothing to place");
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "a one-sided book whose ask is above the last known price reports nothing"
    );

    // (b') ASKS ONLY with a breached last known price: the ask branch may not
    // RESCUE a stop the position's own price already breached. A high ask only
    // tells us nobody is selling cheap; it says nothing about what the position
    // is worth, so it must never raise the judgement above the last known
    // price. This is the property that makes the fix strictly non-regressive:
    // the ask can add judgement, never remove it.
    let mut pm = PositionManager::new(PositionConfig::default());
    let id = position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    pm.tick(
        &id,
        Some(&book(
            vec![(dec!(0.30), dec!(1000))],
            vec![(dec!(0.31), dec!(1000))],
            NOW + 3_000,
        )),
        NOW + 3_000,
    );
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(vec![], vec![(dec!(0.90), dec!(1000))], NOW + 5_000),
            ))
        },
        NOW + 5_000,
    );
    assert!(reqs.is_empty(), "no bid ⇒ nothing to place");
    let events = pm.drain_suppressed_stops();
    assert_eq!(
        events.len(),
        1,
        "the ask is not a rescue: a breached last known price still reports"
    );
    assert_eq!(events[0].mid, dec!(0.30));
    assert_eq!(events[0].pnl_pct_at_mid, dec!(-25));

    // (c) BIDS ONLY — the sell side emptied, but a buyer is standing. This is
    // the control: the stop must still become a real closing SELL, unchanged by
    // any of the above. No asks ⇒ no mid either, so the bid is the only price
    // and the stop judges on it directly.
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(vec![(dec!(0.15), dec!(1000))], vec![], NOW + 5_000),
            ))
        },
        NOW + 5_000,
    );
    assert_eq!(reqs.len(), 1, "a live bid must still close a breached stop");
    assert_eq!(reqs[0].reason, ExitReason::StopLoss);
    assert_eq!(reqs[0].exit_price, dec!(0.15));
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "an exit that became an order is not a suppressed one"
    );

    // (d) NEITHER SIDE — a book with no levels at all carries no price
    // information: its mid is the builder's placeholder, not a quote. Nothing
    // has moved since entry, so there is nothing to judge and nothing to
    // report. Silence here is correct, and it is the point: it is only silence
    // when there is genuinely nothing to say.
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(vec![], vec![], NOW + 5_000),
            ))
        },
        NOW + 5_000,
    );
    assert!(reqs.is_empty(), "an empty book must not mint a SELL");
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "an unchanged last known price is a quiet market, not a held stop"
    );

    // (d') NEITHER SIDE, after a collapse: the same empty book, but the last
    // known price fell to 0.15 before the book went dark. That is a breach with
    // nothing but a recollection to judge on — still no order possible, still
    // reported. This is the pre-#225 path and it must survive the change.
    let mut pm = PositionManager::new(PositionConfig::default());
    let id = position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    pm.tick(
        &id,
        Some(&book(
            vec![(dec!(0.15), dec!(1000))],
            vec![(dec!(0.16), dec!(1000))],
            NOW + 3_000,
        )),
        NOW + 3_000,
    );
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(vec![], vec![], NOW + 4_000),
            ))
        },
        NOW + 4_000,
    );
    assert!(reqs.is_empty(), "an empty book must not mint a SELL");
    let events = pm.drain_suppressed_stops();
    assert_eq!(events.len(), 1, "the held stop must be reported");
    assert_eq!(events[0].mid, dec!(0.15));
    assert_eq!(events[0].pnl_pct_at_mid, dec!(-62.5));

    // (e) TWO-SIDED — the ordinary path, unchanged: a collapsed but healthy
    // book still fires a real closing SELL.
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(
                    vec![(dec!(0.10), dec!(1000))],
                    vec![(dec!(0.12), dec!(1000))],
                    NOW + 5_000,
                ),
            ))
        },
        NOW + 5_000,
    );
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].reason, ExitReason::StopLoss);
    assert_eq!(reqs[0].exit_price, dec!(0.10));
    assert!(pm.drain_suppressed_stops().is_empty());

    // …and a two-sided book that has NOT breached the stop stays quiet.
    let mut pm = PositionManager::new(PositionConfig::default());
    position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(
                    vec![(dec!(0.39), dec!(1000))],
                    vec![(dec!(0.41), dec!(1000))],
                    NOW + 5_000,
                ),
            ))
        },
        NOW + 5_000,
    );
    assert!(reqs.is_empty());
    assert!(pm.drain_suppressed_stops().is_empty());
}

/// #225, at the level the operator actually sees: the core's exit tick and the
/// event sink. "No order" was never the bug — "no order AND no trace" was.
#[test]
fn a_swept_book_reaches_the_panel_as_a_held_stop() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut c = dry_core(PositionConfig::default());
    c.set_event_sink(tx);
    open_long(&mut c, "tok", dec!(0.39), dec!(0.40), dec!(10), NOW);
    let _ = drain_alerts(&mut rx); // the round-start chatter is not the subject

    // The buy side is swept and the ask collapses with it: nothing to sell into,
    // and a position that is now worth a fraction of its entry.
    c.book_snapshot("tok", vec![], vec![(dec!(0.15), dec!(1000))], NOW + 5_000);
    c.tick(NOW + 5_000).unwrap();

    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "the position is held: there is no buyer to sell to"
    );
    assert!(
        !c.list_orders()
            .iter()
            .any(|o| o.side == Side::Sell && o.status.is_live()),
        "no closing sell may rest against a book with no bid"
    );
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts.iter().any(|m| m.contains("no executable bid")),
        "the held stop must reach the event sink, not just memory: {alerts:?}"
    );
    let stats = c.engine_stats_at(NOW + 5_000);
    assert_eq!(
        stats["dailyLoss"]["suppressedStops"].as_u64(),
        Some(1),
        "the panel counter must show it too"
    );
}

#[test]
fn a_genuine_pin_bar_is_withheld_and_leaves_a_report() {
    let mut pm = PositionManager::new(PositionConfig::default());
    let id = position(&mut pm, "tok", dec!(0.40), dec!(10), NOW);
    let reqs = pm.check_exits(
        &|_| {
            Some(blitzkrieg_core::exit_policy::BookView::from_snapshot(
                &book(
                    vec![(dec!(0.20), dec!(1000))],
                    vec![(dec!(0.60), dec!(1000))],
                    NOW + 5_000,
                ),
            ))
        },
        NOW + 5_000,
    );
    assert!(reqs.is_empty(), "a pin bar must not close the position");

    let events = pm.drain_suppressed_stops();
    assert_eq!(
        events.len(),
        1,
        "the withheld trigger must be reported once"
    );
    assert_eq!(events[0].position_id, id);
    assert_eq!(events[0].bid, dec!(0.20));
    assert_eq!(events[0].mid, dec!(0.40));
    assert_eq!(events[0].pnl_pct_at_bid, dec!(-50));
    assert_eq!(events[0].pnl_pct_at_mid, Decimal::ZERO);
    assert_eq!(events[0].stop_pct, dec!(12));
    assert!(events[0].message().contains("SUPPRESSED"));
    assert_eq!(pm.suppressed_stop_count(), 1, "the panel counter moves too");
    assert!(
        pm.drain_suppressed_stops().is_empty(),
        "a drain must be a drain"
    );
}
