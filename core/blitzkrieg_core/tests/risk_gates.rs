//! P0 risk-gate acceptance tests — the ISSUE-level behaviour, not the unit.
//!
//! Four audit findings are pinned here end to end, through the same public
//! entry points the kernel uses at runtime (`Core::place`, `Core::flatten`,
//! `Core::tick`, `PositionManager::check_exits`), because the unit tests inside
//! `risk.rs` / `exit_policy.rs` / `position.rs` each cover only one layer of the
//! chain these bugs travelled through:
//!
//! * **#174** — the kill switch must freeze ENTRIES and never the way out of a
//!   position: the closing SELL has to pass the same risk gate that refuses the
//!   BUY, or a stop-limited loss silently becomes unlimited.
//! * **#177** — the protective stop must fire when the bid is gone, missing or
//!   dislocated on a real decline, and a genuine pin bar that IS withheld must
//!   leave a report behind instead of vanishing.
//! * **#173** — the daily-loss breaker must actually exist: a real day boundary,
//!   a loss that survives a restart, a cap relative to the account, and a trip
//!   the operator can see.
//! * **#202** — one order's notional must be bounded as a SHARE of the account,
//!   because a fixed lot is not a risk statement: on the live 4.8 USDC book the
//!   shipped 10-share band commits 4.00–6.00 USD per order (83%–125% of it).
//!   The cap rejects over-cap orders and still lets every close through.

use blitzkrieg_core::ipc::schema::Event;
use blitzkrieg_core::model::{
    CoreErrorCode, ExitReason, FillPolicy, Mode, OrderRequest, OrderRole, OrderStatus,
    OrderbookSnapshot, Side, SignalDirection,
};
use blitzkrieg_core::position::{
    DailyLossState, EquityBasis, OpenParams, PositionConfig, PositionManager, StopSuppressionCause,
    utc_day_index,
};
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::service::{Core, CoreConfig};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// A fixed instant (2001-09-09, 01:46:40 UTC) whose UTC day index is 11_574 —
/// far from a boundary, so the tests can pick the day they mean.
const NOW: i64 = 1_000_000_000_000;
const DAY_MS: i64 = 86_400_000;

fn temp_path(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("bk-risk-gates-{}-{name}.json", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn dry_core(positions: PositionConfig) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        dry_seed_balance: dec!(100),
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions,
        ..Default::default()
    });
    c.set_balance(dec!(100));
    c
}

/// The round slot the kernel derives from the clock: a position opened in slot N
/// expires at the END of it, so this keeps the test's expiry in the future (a
/// hard-coded slot puts it in 1970 and ForceExit preempts every other rule).
fn slot_at(now: i64) -> i64 {
    now / 1000 / 900
}

fn order(
    tok: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    key: &str,
    now: i64,
) -> OrderRequest {
    OrderRequest {
        token_id: tok.into(),
        condition_id: "cond".into(),
        side,
        mode: FillPolicy::Taker,
        price,
        size,
        internal_key: key.into(),
        strategy: "s".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: slot_at(now),
    }
}

/// Open one LONG through the ordinary taker path (dry fills are immediate).
fn open_long(c: &mut Core, tok: &str, bid: Decimal, ask: Decimal, size: Decimal, now: i64) {
    c.book_snapshot(tok, vec![(bid, dec!(1000))], vec![(ask, dec!(1000))], now);
    let (_, status) = c
        .place(
            order(tok, Side::Buy, ask, size, &format!("entry:{tok}"), now),
            0,
            now,
        )
        .expect("the entry must be accepted");
    assert_eq!(status, OrderStatus::Filled);
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "one position must have opened"
    );
}

fn drain_alerts(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Event::RiskAlert { message, .. } = ev {
            out.push(message);
        }
    }
    out
}

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

/// Open one LONG directly in the position manager (no Core), so a test can
/// hand it any book it likes — including none at all.
fn position(
    pm: &mut PositionManager,
    tok: &str,
    entry: Decimal,
    shares: Decimal,
    now: i64,
) -> String {
    let pos = pm.open(
        OpenParams {
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: SignalDirection::Up,
            token_id: tok.into(),
            condition_id: "cond".into(),
            entry_price: entry,
            expires_at_ms: now + 900_000,
            was_maker: false,
            target_exit_price: None,
        },
        now,
    );
    pm.apply_entry_fill(&pos.id, shares, entry, Decimal::ZERO, OrderRole::Taker);
    pos.id
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
        &|_| Some(book(vec![], vec![(dec!(0.15), dec!(1000))], NOW + 5_000)),
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
            Some(book(
                vec![(dec!(0.05), dec!(1000))],
                vec![(dec!(0.075), dec!(1000))],
                NOW + 5_000,
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
        &|_| Some(book(vec![], vec![(dec!(0.15), dec!(1000))], NOW + 5_000)),
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
        &|_| Some(book(vec![], vec![(dec!(0.90), dec!(1000))], NOW + 5_000)),
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
        &|_| Some(book(vec![], vec![(dec!(0.90), dec!(1000))], NOW + 5_000)),
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
        &|_| Some(book(vec![(dec!(0.15), dec!(1000))], vec![], NOW + 5_000)),
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
    let reqs = pm.check_exits(&|_| Some(book(vec![], vec![], NOW + 5_000)), NOW + 5_000);
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
    let reqs = pm.check_exits(&|_| Some(book(vec![], vec![], NOW + 4_000)), NOW + 4_000);
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
            Some(book(
                vec![(dec!(0.10), dec!(1000))],
                vec![(dec!(0.12), dec!(1000))],
                NOW + 5_000,
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
            Some(book(
                vec![(dec!(0.39), dec!(1000))],
                vec![(dec!(0.41), dec!(1000))],
                NOW + 5_000,
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
            Some(book(
                vec![(dec!(0.20), dec!(1000))],
                vec![(dec!(0.60), dec!(1000))],
                NOW + 5_000,
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

// ── #202: one order is bounded as a share of the account ────────────────────

/// A dry core on a chosen account size with the equity-relative per-order cap
/// armed (`pct`) or off (`0`). The absolute cap is deliberately wide so only the
/// relative one can bite.
fn account_core(balance: Decimal, pct: Decimal) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            max_order_notional_pct: pct,
            ..Default::default()
        },
        dry_seed_balance: balance,
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions: PositionConfig::default(),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

#[test]
fn the_relative_cap_bounds_any_order_but_never_the_way_out() {
    // The live account: 4.8 USDC, 20% per order = 0.96 USD.
    let mut c = account_core(dec!(4.8), dec!(20));
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW,
    );

    // The measured live ticket — 10 shares at 0.40 = 4.00 USD, 83% of this
    // account — is refused, and the refusal names the cap that bit. The old
    // 6.00 absolute bound waved exactly this order through.
    let err = c
        .place(
            order("tok", Side::Buy, dec!(0.40), dec!(10), "entry:tok", NOW),
            0,
            NOW,
        )
        .expect_err("an over-cap entry must be rejected, not truncated");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("equity cap"), "{}", err.message);
    assert!(
        c.positions().open_positions().is_empty(),
        "a refused entry leaves no position and no half-filled remnant"
    );

    // The largest whole ticket inside 0.96 is 2 shares (0.80): it fills, so the
    // escape hatch below has a real position to reduce.
    let (_, status) = c
        .place(
            order("tok", Side::Buy, dec!(0.40), dec!(2), "entry:tok", NOW),
            0,
            NOW,
        )
        .expect("the in-cap entry must be accepted");
    assert_eq!(status, OrderStatus::Filled);
    assert_eq!(c.positions().open_positions().len(), 1);

    // The way out: 2 shares at a 0.59 bid is 1.18 USD — over the cap — and the
    // close must pass anyway, because a cap that traps a position is #174
    // reopened through another door.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.59), dec!(1000))],
        vec![(dec!(0.60), dec!(1000))],
        NOW + 5_000,
    );
    assert_eq!(
        c.flatten(None, NOW + 5_000).unwrap(),
        1,
        "the closing sell must be submitted"
    );
    assert!(
        c.positions().open_positions().is_empty(),
        "the close must have completed in full"
    );
}

#[test]
fn the_panel_states_one_orders_worst_case_and_the_cap_that_bounds_it() {
    // Armed: 20% of a 4.8 book = 0.96 USD, and that is the binding bound.
    let c = account_core(dec!(4.8), dec!(20));
    let stats = c.engine_stats_at(NOW);
    let sizing = &stats["sizing"];
    assert_eq!(sizing["equityUsd"].as_f64(), Some(4.8));
    assert_eq!(sizing["shareBandUsd"].as_f64(), Some(10.0));
    assert_eq!(sizing["maxOrderNotionalPct"].as_f64(), Some(20.0));
    assert_eq!(sizing["equityCapUsd"].as_f64(), Some(0.96));
    assert_eq!(sizing["worstCaseOrderUsd"].as_f64(), Some(0.96));
    let pct = sizing["worstCasePctOfEquity"].as_f64().unwrap();
    assert!(pct <= 20.0 + 1e-9, "worst case must fit the cap: {pct}%");
    assert_eq!(sizing["closesExemptFromEquityCap"].as_bool(), Some(true));

    // Unconfigured (the shipped default): the same account has NO bound as a
    // share of itself — 10 shares × 1.00 = 10.00 USD, 208% of the book. That is
    // the hole #202 is about, and the panel now states it instead of leaving it
    // to be re-derived from the flags.
    let c = account_core(dec!(4.8), Decimal::ZERO);
    let sizing = &c.engine_stats_at(NOW)["sizing"];
    assert_eq!(sizing["worstCaseOrderUsd"].as_f64(), Some(10.0));
    let pct = sizing["worstCasePctOfEquity"].as_f64().unwrap();
    assert!((pct - 208.333_333).abs() < 0.001, "{pct}%");
    assert_eq!(sizing["equityCapUsd"], serde_json::Value::Null);

    // The same knobs on a 480 USD account: the PERCENTAGE is the same, the money
    // is not — which is why an absolute bound cannot say this on every account.
    let c = account_core(dec!(480), dec!(20));
    let sizing = &c.engine_stats_at(NOW)["sizing"];
    assert_eq!(sizing["equityCapUsd"].as_f64(), Some(96.0));
    assert_eq!(sizing["worstCaseOrderUsd"].as_f64(), Some(10.0));
}

// ── #173: the daily-loss breaker ────────────────────────────────────────────

/// The budget config the position-level tests use: every cooldown off, so the
/// only thing `can_open` can complain about is the daily cap.
fn budget_config(abs: Decimal, pct: Decimal, path: Option<String>) -> PositionConfig {
    PositionConfig {
        max_positions: 8,
        max_daily_loss_usd: abs,
        max_daily_loss_equity_pct: pct,
        daily_pnl_path: path,
        stop_loss_cooldown_sec: 0,
        exit_cooldown_sec: 0,
        asset_cooldown_sec: 0,
        loss_cooldown_sec: 0,
        ..Default::default()
    }
}

/// Open `shares` at `entry` and close them at `exit`; returns the realized net
/// PnL the daily budget just absorbed.
fn realize(
    pm: &mut PositionManager,
    tok: &str,
    entry: Decimal,
    shares: Decimal,
    exit: Decimal,
    now: i64,
) -> Decimal {
    let id = position(pm, tok, entry, shares, now);
    pm.close(&id, exit, ExitReason::StopLoss, false, now + 1_000)
        .expect("the position is open")
        .net_pnl_usd
}

#[test]
fn the_cap_is_a_share_of_the_days_opening_equity_and_resets_only_across_days() {
    let mut pm = PositionManager::new(budget_config(Decimal::ZERO, dec!(20), None));
    let roll = pm
        .roll_daily(NOW, dec!(100), EquityBasis::Dry)
        .expect("the first tick stamps the day");
    assert_eq!(roll.day_index, utc_day_index(NOW));
    assert_eq!(roll.limit_usd, dec!(20), "20% of the $100 opening equity");

    // A 15 USD loss is inside the budget: entries stay open.
    let net = realize(
        &mut pm,
        "tok",
        dec!(0.40),
        dec!(50),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(net < dec!(-10), "expected a real loss, got {net}");
    assert!(!pm.daily_loss_tripped());
    assert!(pm.can_open(None, None, NOW + 2_000).is_ok());

    // A second one crosses it, and the freeze is immediate.
    realize(
        &mut pm,
        "tok2",
        dec!(0.40),
        dec!(50),
        dec!(0.10),
        NOW + 3_000,
    );
    assert!(pm.daily_loss_tripped(), "the cap must latch");
    let err = pm
        .can_open(None, None, NOW + 4_000)
        .expect_err("a tripped cap must refuse new entries");
    assert!(err.contains("Daily loss limit"), "{err}");

    // Same day: a later tick does not clear it.
    assert!(
        pm.roll_daily(NOW + 60_000, dec!(70), EquityBasis::Dry)
            .is_none()
    );
    assert!(pm.daily_loss_tripped());

    // A real UTC day boundary does — and re-bases the cap on the new equity.
    let roll = pm
        .roll_daily(NOW + DAY_MS, dec!(70), EquityBasis::Dry)
        .expect("crossing midnight UTC must roll the budget");
    assert_eq!(roll.previous_day_index, Some(utc_day_index(NOW)));
    assert_eq!(roll.day_index, utc_day_index(NOW + DAY_MS));
    assert!(roll.previous_tripped, "the closed day's state is reported");
    assert!(roll.previous_realized_pnl_usd < dec!(-20));
    assert_eq!(roll.limit_usd, dec!(14), "20% of the new $70 equity");
    assert_eq!(pm.daily_pnl(), Decimal::ZERO);
    assert!(!pm.daily_loss_tripped(), "the new day starts clean");
    assert!(pm.can_open(None, None, NOW + DAY_MS + 1_000).is_ok());
}

#[test]
fn the_absolute_cap_still_wins_when_it_is_the_tighter_one() {
    // Absolute $5 against a 50% relative cap on $100 → $5.
    let mut pm = PositionManager::new(budget_config(dec!(5), dec!(50), None));
    pm.roll_daily(NOW, dec!(100), EquityBasis::Dry);
    assert_eq!(pm.effective_daily_loss_limit(), dec!(5));

    // …and a generous absolute cap never raises the relative one.
    let mut pm = PositionManager::new(budget_config(dec!(500), dec!(10), None));
    pm.roll_daily(NOW, dec!(100), EquityBasis::Dry);
    assert_eq!(pm.effective_daily_loss_limit(), dec!(10));
}

#[test]
fn a_restart_keeps_the_days_loss_and_the_next_day_still_clears_it() {
    let path = temp_path("daily-budget");
    let cfg = budget_config(
        dec!(1),
        Decimal::ZERO,
        Some(path.to_string_lossy().into_owned()),
    );

    let mut first = PositionManager::new(cfg.clone());
    first.roll_daily(NOW, dec!(100), EquityBasis::Dry);
    let net = realize(
        &mut first,
        "tok",
        dec!(0.40),
        dec!(50),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(net < dec!(-1), "expected a loss past the cap, got {net}");
    assert!(first.daily_loss_tripped());
    assert!(
        path.exists(),
        "the day's budget must be on disk, not only in memory"
    );
    drop(first);

    // A fresh process (same file) must not be able to launder today's loss.
    let mut second = PositionManager::new(cfg);
    assert_eq!(second.daily_state().day_index, Some(utc_day_index(NOW)));
    assert!(
        second.daily_loss_tripped(),
        "the restored budget is already spent"
    );
    let err = second
        .can_open(None, None, NOW + 2_000)
        .expect_err("a restored loss must gate the very first entry");
    assert!(err.contains("Daily loss limit"), "{err}");

    // The boundary still lifts it, restored budget and all.
    let roll = second
        .roll_daily(NOW + DAY_MS, dec!(85), EquityBasis::Dry)
        .expect("the next UTC day resets it");
    assert!(roll.previous_tripped);
    assert!(!second.daily_loss_tripped());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_trip_reaches_the_panel_and_the_audit_trail() {
    let path = temp_path("trip-alert");
    let positions = budget_config(
        dec!(1),
        Decimal::ZERO,
        Some(path.to_string_lossy().into_owned()),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut c = dry_core(positions.clone());
    c.set_event_sink(tx);
    open_long(&mut c, "tok", dec!(0.39), dec!(0.40), dec!(50), NOW);
    let _ = drain_alerts(&mut rx);

    // Crash the book: the stop exits at ~0.05 and realizes a loss past $1.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.05), dec!(1000))],
        vec![(dec!(0.06), dec!(1000))],
        NOW + 5_000,
    );
    c.tick(NOW + 5_000).unwrap();

    assert!(c.positions().daily_pnl() < dec!(-1));
    assert!(c.positions().daily_loss_tripped());
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts
            .iter()
            .any(|m| m.contains("DAILY LOSS LIMIT REACHED")),
        "the trip must be visible: {alerts:?}"
    );
    let stats = c.engine_stats_at(NOW + 5_000);
    assert_eq!(stats["dailyLoss"]["tripped"].as_bool(), Some(true));
    // The panel serializes decimals as strings (crate::decimal), so compare
    // the rendered value rather than its JSON variant.
    let realized = stats["dailyLoss"]["realizedPnlUsd"].to_string();
    assert!(
        realized.trim_matches('"').starts_with('-'),
        "the panel must carry the loss: {realized}"
    );
    assert_eq!(
        stats["dailyLoss"]["dayIndex"].as_i64(),
        Some(utc_day_index(NOW))
    );

    // A new entry is refused for the rest of the day…
    let err = c
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + 6_000,
            ),
            0,
            NOW + 6_000,
        )
        .expect_err("the day's cap is spent");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("Daily loss limit"), "{}", err.message);

    // …and a restart does not give the day a second budget.
    let mut restarted = dry_core(positions);
    assert!(restarted.positions().daily_loss_tripped());
    let err = restarted
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + 60_000,
            ),
            0,
            NOW + 60_000,
        )
        .expect_err("a restart must not launder today's loss");
    assert!(err.message.contains("Daily loss limit"), "{}", err.message);

    // Crossing the boundary re-opens entries, and the roll is announced.
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    restarted.set_event_sink(tx2);
    restarted.tick(NOW + DAY_MS).unwrap();
    assert!(!restarted.positions().daily_loss_tripped());
    let alerts = drain_alerts(&mut rx2);
    assert!(
        alerts.iter().any(|m| m.contains("daily loss budget")),
        "the roll must be in the audit trail: {alerts:?}"
    );
    restarted.book_snapshot(
        "tok2",
        vec![(dec!(0.39), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW + DAY_MS + 1_000,
    );
    let (_, status) = restarted
        .place(
            order(
                "tok2",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tok2",
                NOW + DAY_MS + 1_000,
            ),
            0,
            NOW + DAY_MS + 1_000,
        )
        .expect("the new day's entries are open again");
    assert_eq!(status, OrderStatus::Filled);
    let _ = std::fs::remove_file(&path);
}

// ── #235: the day's base belongs to a BOOK, and a dry→live switch is a
//          different one ────────────────────────────────────────────────────

/// A live-mode core on `balance`: the money is the VENUE's here, so it is seeded
/// through the host path (`set_balance`) rather than `dry_seed_balance` — which
/// is also what makes the day's basis "live" (P1 #235).
fn live_core(positions: PositionConfig, balance: Decimal, position_log: &std::path::Path) -> Core {
    let mut c = Core::new(CoreConfig {
        mode: Mode::Live,
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions,
        position_log_path: Some(position_log.to_string_lossy().into_owned()),
        ..Default::default()
    });
    c.set_balance(balance);
    c
}

/// A dry core on a SEEDED book, with the open-position log wired so a test can
/// carry a position across a restart the way the kernel does.
fn dry_core_at(positions: PositionConfig, seed: Decimal, position_log: &std::path::Path) -> Core {
    let mut c = Core::new(CoreConfig {
        risk: RiskConfig {
            max_order_notional: dec!(1000),
            ..Default::default()
        },
        dry_seed_balance: seed,
        round_duration_sec: 900,
        auto_exits_enabled: true,
        positions,
        position_log_path: Some(position_log.to_string_lossy().into_owned()),
        ..Default::default()
    });
    c.set_balance(seed);
    c
}

/// Everything `f` logged on this thread, as text. Thread-local, so tests
/// running in parallel do not capture each other's lines.
#[derive(Clone, Default)]
struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn with_logs<R>(f: impl FnOnce() -> R) -> (R, String) {
    let sink = LogSink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let out = f();
    drop(guard);
    let text = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    (out, text)
}

/// Read back what the day's budget persisted.
fn persisted_daily(path: &std::path::Path) -> DailyLossState {
    let text = std::fs::read_to_string(path).expect("the day's budget must be on disk");
    serde_json::from_str(&text).expect("the persisted budget must parse")
}

/// The issue's headline: `--seed-balance 10000` in dry, then live on a $50 book
/// within the same UTC day. The old code kept the dry base, so 20% of it was a
/// $2 000 cap on a $50 account — a breaker that cannot trip.
#[test]
fn a_same_day_dry_to_live_restart_rebases_the_cap_on_the_live_book() {
    let path = temp_path("basis-switch");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    // The seeded dry run stamps the day against the simulated book.
    let mut dry = PositionManager::new(cfg.clone());
    let roll = dry
        .roll_daily(NOW, dec!(10_000), EquityBasis::Dry)
        .expect("the first tick stamps the day");
    assert_eq!(roll.limit_usd, dec!(2_000), "20% of the simulated $10 000");
    drop(dry);
    let stamped = persisted_daily(&path);
    assert_eq!(stamped.opening_equity_usd, dec!(10_000));
    assert_eq!(
        stamped.equity_basis,
        Some(EquityBasis::Dry),
        "the base must record which book it came from"
    );

    // Same UTC day, the operator switches to live on a $50 book.
    let mut live = PositionManager::new(cfg);
    let (rolled, logs) = with_logs(|| live.roll_daily(NOW + 60_000, dec!(50), EquityBasis::Live));
    assert!(
        rolled.is_none(),
        "a re-anchor is not a day boundary — nothing was closed"
    );
    assert_eq!(
        live.daily_state().opening_equity_usd,
        dec!(50),
        "the day's base now belongs to the live book"
    );
    assert_eq!(live.daily_state().equity_basis, Some(EquityBasis::Live));
    assert_eq!(
        live.effective_daily_loss_limit(),
        dec!(10),
        "20% of the live $50 — NOT the $2 000 the dry seed implied"
    );

    // The switch is announced, with both numbers and both books.
    assert!(logs.contains("WARN"), "it must be a warning: {logs}");
    assert!(
        logs.contains("opening equity 10000 (dry) -> 50 (live)"),
        "the warning must carry old/new value and old/new basis: {logs}"
    );

    // …and persisted, so the next restart starts from the live book.
    let rebased = persisted_daily(&path);
    assert_eq!(rebased.opening_equity_usd, dec!(50));
    assert_eq!(rebased.equity_basis, Some(EquityBasis::Live));
    let _ = std::fs::remove_file(&path);
}

/// Write the day's state file as a kernel wrote it: `basis = None` produces the
/// PRE-#235 shape, where `openingEquityUsd` is in the file and the book it was
/// measured from is absent.
///
/// That is the shape of the issue's own evidence — the dry run stamped
/// `openingEquityUsd: 10000` and nothing said which account that was — so the
/// compatibility path is exercised by building a real [`DailyLossState`] and
/// dropping the field, rather than by hand-writing JSON that could drift from
/// what the kernel actually persists.
fn write_daily(
    path: &std::path::Path,
    day: i64,
    opening_equity: Decimal,
    basis: Option<EquityBasis>,
) {
    let mut value = serde_json::to_value(DailyLossState {
        day_index: Some(day),
        opening_equity_usd: opening_equity,
        equity_basis: basis,
        opened_at_ms: NOW,
        ..Default::default()
    })
    .expect("the day's state must serialize");
    if basis.is_none() {
        let dropped = value
            .as_object_mut()
            .expect("DailyLossState is a JSON object")
            .remove("equityBasis");
        assert!(
            dropped.is_some(),
            "the fixture must actually drop the basis field"
        );
    }
    std::fs::write(
        path,
        serde_json::to_string(&value).expect("serialize the day's state"),
    )
    .expect("write the day's state file");
}

/// The file the issue's evidence came from: a state file written BEFORE the fix
/// records no basis at all, so its $10 000 base cannot be attributed to any book.
/// An unattributable base is not a licence to keep the old cap — it must be
/// re-anchored onto the live book in front of it, and 20% of that $50 book (not
/// 20% of the $10 000 the file happened to carry) is the day's cap.
#[test]
fn a_legacy_state_file_with_no_basis_rebases_onto_the_live_book() {
    let path = temp_path("legacy-basis-live");
    write_daily(&path, utc_day_index(NOW), dec!(10_000), None);
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    // What the file carries in, and why it is the fail-open the issue reports:
    // 20% of the unattributable base is a $2 000 cap on a $50 account.
    let stale = persisted_daily(&path);
    assert_eq!(stale.day_index, Some(utc_day_index(NOW)));
    assert_eq!(stale.opening_equity_usd, dec!(10_000));
    assert_eq!(stale.equity_basis, None, "the fixture must not name a book");
    assert_eq!(stale.opening_equity_usd * dec!(20) / dec!(100), dec!(2_000));

    // Same UTC day, a live $50 book.
    let mut live = PositionManager::new(cfg);
    let (rolled, logs) = with_logs(|| live.roll_daily(NOW + 60_000, dec!(50), EquityBasis::Live));
    assert!(
        rolled.is_none(),
        "same UTC day: no day boundary was crossed"
    );
    assert_eq!(
        live.effective_daily_loss_limit(),
        dec!(10),
        "50 x 20% — NOT the $2 000 the unattributable base implied"
    );
    assert_eq!(live.daily_state().opening_equity_usd, dec!(50));
    assert_eq!(live.daily_state().equity_basis, Some(EquityBasis::Live));
    assert!(
        logs.contains("opening equity 10000 (unrecorded) -> 50 (live)"),
        "an unrecorded basis must be named as such: {logs}"
    );
    let rebased = persisted_daily(&path);
    assert_eq!(rebased.opening_equity_usd, dec!(50));
    assert_eq!(rebased.equity_basis, Some(EquityBasis::Live));
    let _ = std::fs::remove_file(&path);
}

/// Where the missing LABEL is the only thing that can act: the book moves by less
/// than the 50% deviation fallback, so a file that names the live book is an
/// ordinary restart and a file that names nothing is re-anchored — on the same
/// $200 → $150 move.
///
/// Without this pair the rule is untested: the headline case drops 99.5%, which
/// the deviation fallback catches whether or not an unrecorded basis counts as a
/// change.
#[test]
fn a_legacy_state_file_rebases_where_a_recorded_one_would_not() {
    let day = utc_day_index(NOW);
    let labeled = temp_path("legacy-basis-control");
    write_daily(&labeled, day, dec!(200), Some(EquityBasis::Live));
    let unlabeled = temp_path("legacy-basis-discriminating");
    write_daily(&unlabeled, day, dec!(200), None);

    // Control: the file says the base came from the live book, and the live book
    // is 25% smaller — inside the deviation band — so nothing moves.
    let mut control = PositionManager::new(budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(labeled.to_string_lossy().into_owned()),
    ));
    control.roll_daily(NOW + 60_000, dec!(150), EquityBasis::Live);
    assert_eq!(
        control.daily_state().opening_equity_usd,
        dec!(200),
        "a restarted live book keeps the day's base"
    );
    assert_eq!(control.effective_daily_loss_limit(), dec!(40));

    // The same numbers with no basis recorded: it re-anchors onto the live book.
    let mut legacy = PositionManager::new(budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(unlabeled.to_string_lossy().into_owned()),
    ));
    legacy.roll_daily(NOW + 60_000, dec!(150), EquityBasis::Live);
    assert_eq!(
        legacy.daily_state().opening_equity_usd,
        dec!(150),
        "an unattributable base re-anchors even inside the deviation band"
    );
    assert_eq!(
        legacy.effective_daily_loss_limit(),
        dec!(30),
        "20% of the $150 book, not 20% of the $200 the file carried"
    );
    assert_eq!(legacy.daily_state().equity_basis, Some(EquityBasis::Live));

    let _ = std::fs::remove_file(&labeled);
    let _ = std::fs::remove_file(&unlabeled);
}

/// The other direction of the same file: a base that UNDERSTATES the book is not
/// re-anchored upward, because re-anchoring may only ever tighten. A same-day
/// switch must never hand the day a bigger budget than it already had, so the
/// smaller base (and its smaller cap) stays.
#[test]
fn a_legacy_state_file_never_loosens_the_cap_when_the_book_is_bigger() {
    let path = temp_path("legacy-basis-grow");
    write_daily(&path, utc_day_index(NOW), dec!(50), None);
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    let mut live = PositionManager::new(cfg);
    let (rolled, logs) = with_logs(|| live.roll_daily(NOW + 60_000, dec!(200), EquityBasis::Live));
    assert!(
        rolled.is_none(),
        "same UTC day: no day boundary was crossed"
    );
    assert_eq!(
        live.daily_state().opening_equity_usd,
        dec!(50),
        "the day keeps the tighter base it already had"
    );
    assert_eq!(
        live.effective_daily_loss_limit(),
        dec!(10),
        "…so the cap stays $10, not the $40 the bigger book would allow"
    );
    assert!(
        !logs.contains("re-anchored"),
        "a tightening move that does not happen must not be announced: {logs}"
    );
    let _ = std::fs::remove_file(&path);
}

/// The boundary the fix must not break: the SAME book at (roughly) the same size
/// is an ordinary restart. It must not move the day's base, must not disturb how
/// much of the day's budget is already spent, and must not warn.
#[test]
fn a_same_basis_restart_neither_rebases_the_day_nor_warns() {
    let path = temp_path("basis-same");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    let mut first = PositionManager::new(cfg.clone());
    first.roll_daily(NOW, dec!(10_000), EquityBasis::Dry);
    let realized = realize(
        &mut first,
        "tok",
        dec!(0.40),
        dec!(100),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(realized < dec!(-10), "expected a real loss, got {realized}");
    drop(first);

    // Same day, same book, 5% smaller (fees and a losing trade): a restart.
    let mut second = PositionManager::new(cfg);
    let (_, logs) = with_logs(|| {
        assert!(
            second
                .roll_daily(NOW + 60_000, dec!(9_500), EquityBasis::Dry)
                .is_none()
        );
    });
    assert_eq!(
        second.daily_state().opening_equity_usd,
        dec!(10_000),
        "the day's base is the day's"
    );
    assert_eq!(second.effective_daily_loss_limit(), dec!(2_000));
    assert_eq!(
        second.daily_pnl(),
        realized,
        "a restart must not disturb the day's realized loss"
    );
    assert!(
        logs.trim().is_empty(),
        "a same-basis restart must stay quiet: {logs}"
    );
    let _ = std::fs::remove_file(&path);
}

/// The basis branch ON ITS OWN: the two books happen to be the same size, so
/// only the recorded basis can tell that the day's base came from the other one.
/// A mutation that disables the basis check leaves this test the only one red —
/// the scale-move tests would still pass on the deviation fallback.
#[test]
fn a_same_day_basis_change_rebases_even_when_the_books_are_the_same_size() {
    let path = temp_path("basis-same-size");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    let mut dry = PositionManager::new(cfg.clone());
    dry.roll_daily(NOW, dec!(10_000), EquityBasis::Dry);
    drop(dry);

    // Live on a $9 000 book: 10% smaller, so nothing but the basis moved.
    let mut live = PositionManager::new(cfg);
    let (_, logs) = with_logs(|| {
        live.roll_daily(NOW + 60_000, dec!(9_000), EquityBasis::Live);
    });
    assert_eq!(live.daily_state().opening_equity_usd, dec!(9_000));
    assert_eq!(live.daily_state().equity_basis, Some(EquityBasis::Live));
    assert_eq!(
        live.effective_daily_loss_limit(),
        dec!(1_800),
        "20% of the live $9 000 — the dry seed's number is not this book's"
    );
    assert!(
        logs.contains("opening equity 10000 (dry) -> 9000 (live)"),
        "{logs}"
    );
    let _ = std::fs::remove_file(&path);
}

/// The fallback the basis alone cannot cover: the label is unchanged but the
/// ACCOUNT is not (a re-seeded dry run, a deposit, a withdrawal).
#[test]
fn a_same_basis_but_rescaled_book_rebases_the_day_too() {
    let path = temp_path("basis-rescale");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    let mut first = PositionManager::new(cfg.clone());
    first.roll_daily(NOW, dec!(10_000), EquityBasis::Dry);
    drop(first);

    // Same label, a hundredth of the book: `--seed-balance 50`.
    let mut second = PositionManager::new(cfg);
    let (_, logs) = with_logs(|| {
        second.roll_daily(NOW + 60_000, dec!(50), EquityBasis::Dry);
    });
    assert_eq!(
        second.effective_daily_loss_limit(),
        dec!(10),
        "the cap must follow the book that is actually trading"
    );
    assert!(
        logs.contains("opening equity 10000 (dry) -> 50 (dry)"),
        "{logs}"
    );
    let _ = std::fs::remove_file(&path);
}

/// The re-anchor may only ever TIGHTEN. A bigger book keeps the day's smaller
/// base: handing the day a budget it never had is the fail-open direction.
#[test]
fn a_rebase_never_loosens_the_days_cap() {
    let path = temp_path("basis-grow");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    // A dry $100 day, then a live $50 000 book on the same UTC day.
    let mut first = PositionManager::new(cfg.clone());
    first.roll_daily(NOW, dec!(100), EquityBasis::Dry);
    drop(first);

    let mut second = PositionManager::new(cfg);
    second.roll_daily(NOW + 60_000, dec!(50_000), EquityBasis::Live);
    assert_eq!(
        second.daily_state().opening_equity_usd,
        dec!(100),
        "the day keeps the tighter base it already had"
    );
    assert_eq!(second.effective_daily_loss_limit(), dec!(20));
    let _ = std::fs::remove_file(&path);
}

/// Suggestion 3: a re-anchor that lands UNDER the day's realized loss must
/// freeze the day on the spot — the tightened cap is already spent, and the day
/// must never read as healthy again.
#[test]
fn a_rebase_below_the_days_realized_loss_trips_it_immediately() {
    let path = temp_path("rebase-trip");
    let cfg = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(path.to_string_lossy().into_owned()),
    );

    // Dry, $10 000: a −$30 loss is deep inside the $2 000 cap.
    let mut dry = PositionManager::new(cfg.clone());
    dry.roll_daily(NOW, dec!(10_000), EquityBasis::Dry);
    let realized = realize(
        &mut dry,
        "tok",
        dec!(0.40),
        dec!(100),
        dec!(0.10),
        NOW + 1_000,
    );
    assert!(realized <= dec!(-30), "expected ≈ −$30, got {realized}");
    assert!(realized > dec!(-2_000), "…but inside the seeded cap");
    assert!(!dry.daily_loss_tripped());
    assert!(dry.can_open(None, None, NOW + 2_000).is_ok());
    drop(dry);

    // Live, $50, same UTC day: the cap is now $10 and the day is already past it.
    let mut live = PositionManager::new(cfg.clone());
    let (_, logs) = with_logs(|| {
        live.roll_daily(NOW + 60_000, dec!(50), EquityBasis::Live);
    });
    assert_eq!(live.effective_daily_loss_limit(), dec!(10));
    assert!(
        live.daily_loss_tripped(),
        "a spent, tightened cap must freeze the day instead of healing it"
    );
    assert!(
        live.daily_state().tripped,
        "…and the trip must be LATCHED on the spot, not left to be re-derived"
    );
    assert_eq!(
        live.daily_state().tripped_limit_usd,
        dec!(10),
        "the frozen cap is the live one"
    );
    assert!(
        logs.contains("already spent"),
        "the freeze must be part of the same warning: {logs}"
    );
    let err = live
        .can_open(None, None, NOW + 61_000)
        .expect_err("entries must be refused");
    assert!(err.contains("Daily loss limit"), "{err}");

    // …and it is persisted: a further restart cannot launder it either.
    drop(live);
    let again = PositionManager::new(cfg);
    assert!(again.daily_loss_tripped());
    assert_eq!(again.effective_daily_loss_limit(), dec!(10));
    let _ = std::fs::remove_file(&path);
}

/// The same thing end to end, through the kernel's own entry points: a seeded
/// dry run leaves the day stamped at $10 000 AND a position on the book; the
/// live process restores the position, re-anchors the day onto its $50, trips
/// it, refuses the next entry — and still lets the open position out (#174's
/// guarantee, re-checked on this new trip path).
#[test]
fn a_dry_to_live_rebase_trips_the_day_and_still_lets_the_position_out() {
    let day_path = temp_path("rebase-trip-core-day");
    let log_path = temp_path("rebase-trip-core-positions");
    let positions = budget_config(
        Decimal::ZERO,
        dec!(20),
        Some(day_path.to_string_lossy().into_owned()),
    );

    // (1) The seeded dry run (`--seed-balance 10000`): the day is stamped
    //     against $10 000, a loss lands well inside that cap, and one position
    //     is left open.
    let mut dry = dry_core_at(positions.clone(), dec!(10_000), &log_path);
    dry.tick(NOW).unwrap();
    assert_eq!(
        dry.positions().daily_state().opening_equity_usd,
        dec!(10_000)
    );
    open_long(
        &mut dry,
        "tokA",
        dec!(0.39),
        dec!(0.40),
        dec!(100),
        NOW + 1_000,
    );
    dry.book_snapshot(
        "tokA",
        vec![(dec!(0.05), dec!(1000))],
        vec![(dec!(0.06), dec!(1000))],
        NOW + 2_000,
    );
    dry.tick(NOW + 2_000).unwrap();
    let realized = dry.positions().daily_pnl();
    assert!(
        realized <= dec!(-30) && realized > dec!(-2_000),
        "a loss inside the seeded cap, past the live one: {realized}"
    );
    assert!(!dry.positions().daily_loss_tripped());
    open_long(
        &mut dry,
        "tokB",
        dec!(0.39),
        dec!(0.40),
        dec!(10),
        NOW + 3_000,
    );
    assert_eq!(dry.positions().open_positions().len(), 1);
    drop(dry);

    // (2) Same UTC day, live on a $50 book: the open position comes back with
    //     the process (crash recovery); the day comes back carrying the dry
    //     base, which is the window the issue is about.
    let mut c = live_core(positions, dec!(50), &log_path);
    assert_eq!(
        c.restore_positions(),
        1,
        "the open position survives the restart"
    );
    assert_eq!(c.ledger().balance(), dec!(50), "the live book is $50");
    assert_eq!(
        c.positions().effective_daily_loss_limit(),
        dec!(2_000),
        "before the first tick the day still carries the dry base"
    );

    // (3) The first tick re-anchors the day onto the live book, and the day's
    //     realized loss is already past the $10 cap that book allows.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.set_event_sink(tx);
    c.tick(NOW + 60_000).unwrap();
    assert_eq!(c.positions().effective_daily_loss_limit(), dec!(10));
    assert_eq!(c.positions().daily_state().opening_equity_usd, dec!(50));
    assert_eq!(
        c.positions().daily_state().equity_basis,
        Some(EquityBasis::Live)
    );
    assert!(
        c.positions().daily_loss_tripped(),
        "the day must be frozen, not quietly healed"
    );
    let alerts = drain_alerts(&mut rx);
    assert!(
        alerts
            .iter()
            .any(|m| m.contains("DAILY LOSS LIMIT REACHED")),
        "the freeze must reach the operator: {alerts:?}"
    );
    // The panel states the fact too: a $10 cap measured against a LIVE book.
    let stats = c.engine_stats_at(NOW + 60_000);
    assert_eq!(stats["dailyLoss"]["equityBasis"].as_str(), Some("live"));
    assert_eq!(
        stats["dailyLoss"]["openingEquityUsd"]
            .to_string()
            .trim_matches('"'),
        "50"
    );
    assert_eq!(
        stats["dailyLoss"]["limitUsd"].to_string().trim_matches('"'),
        "10"
    );

    // (4) A new entry is refused…
    let err = c
        .place(
            order(
                "tokC",
                Side::Buy,
                dec!(0.40),
                dec!(10),
                "entry:tokC",
                NOW + 61_000,
            ),
            0,
            NOW + 61_000,
        )
        .expect_err("a spent, tightened cap must refuse entries");
    assert_eq!(err.code, CoreErrorCode::RiskRejected);
    assert!(err.message.contains("Daily loss limit"), "{}", err.message);

    // (5) …and the position that was already open still gets out.
    c.book_snapshot(
        "tokB",
        vec![(dec!(0.38), dec!(1000))],
        vec![(dec!(0.40), dec!(1000))],
        NOW + 62_000,
    );
    assert_eq!(
        c.flatten(None, NOW + 62_000).unwrap(),
        1,
        "the closing sell must be submitted while the day is tripped"
    );
    let _ = std::fs::remove_file(&day_path);
    let _ = std::fs::remove_file(&log_path);
}
