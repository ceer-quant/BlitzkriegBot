#![cfg(test)]
//! P-1.2: the dry matcher honours the configured [`crate::sim::FillModel`]. The
//! default is the identity (existing tests cover it); these pin down the opt-in
//! deviations and the maker-only nature of the latency/probability gates.

use super::*;
use crate::risk::RiskConfig;
use crate::sim::FillModel;
use rust_decimal_macros::dec;

fn core_with(model: FillModel) -> Core {
    Core::new(CoreConfig {
        mode: Mode::Dry,
        risk: RiskConfig {
            max_order_notional: dec!(100),
            ..Default::default()
        },
        dry_seed_balance: dec!(1000),
        auto_exits_enabled: false,
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        fill_model: model,
        ..Default::default()
    })
}

fn buy(mode: FillPolicy, price: Decimal, size: Decimal) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        condition_id: "cond".into(),
        side: Side::Buy,
        mode,
        price,
        size,
        internal_key: "k1".into(),
        strategy: "spread_arb".into(),
        asset: "BTC".into(),
        direction: "up".into(),
        round_slot: 1,
        account_id: crate::model::default_account_id(),
    }
}

#[test]
fn default_model_fills_a_crossing_maker_at_its_own_limit() {
    let mut c = core_with(FillModel::default());
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(100))],
        vec![(dec!(0.40), dec!(100))],
        1_100,
    );
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Filled);
    let pos = &c.positions().open_positions()[0];
    assert_eq!(
        pos.entry_price,
        dec!(0.40),
        "a maker fill is priced by your own quote"
    );
}

#[test]
fn taker_slippage_worsens_the_fill_price() {
    let mut c = core_with(FillModel {
        taker_slippage_ticks: 2,
        ..FillModel::default()
    });
    // Ask rests AT the buy limit: the walk prices 0.40, and the slippage
    // dial prices the impact of our own 10 shares on top of it.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(500))], 900);
    let (id, st) = c
        .place(buy(FillPolicy::Taker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    assert_eq!(st, OrderStatus::Filled);
    // 2 ticks = 0.02: a taker buy pays up rather than filling at the limit.
    assert_eq!(c.ome().get(&id).unwrap().avg_fill_price, Some(dec!(0.42)));
    assert_eq!(c.positions().open_positions()[0].entry_price, dec!(0.42));
}

#[test]
fn maker_latency_delays_the_crossing_fill() {
    let mut c = core_with(FillModel {
        maker_latency_ms: 5_000,
        ..FillModel::default()
    });
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    // Crossed 100 ms after submission: the venue could not have the order yet.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(100))],
        vec![(dec!(0.40), dec!(100))],
        1_100,
    );
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "no fill inside the latency window"
    );
    assert!(
        c.ome().get(&id).unwrap().status.is_live(),
        "the order stays live"
    );
    // Crossed again after the window: fills.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(100))],
        vec![(dec!(0.40), dec!(100))],
        6_100,
    );
    assert_eq!(
        c.positions().open_positions().len(),
        1,
        "fills once the latency has elapsed"
    );
    assert_eq!(c.positions().open_positions()[0].entry_price, dec!(0.40));
}

#[test]
fn zero_fill_probability_never_fills_on_a_crossing() {
    let mut c = core_with(FillModel {
        maker_fill_prob_bps: 0,
        ..FillModel::default()
    });
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    for t in [1_100i64, 2_000, 9_000] {
        c.book_snapshot(
            "tok",
            vec![(dec!(0.39), dec!(100))],
            vec![(dec!(0.40), dec!(100))],
            t,
        );
    }
    assert_eq!(
        c.positions().open_positions().len(),
        0,
        "a losing queue draw keeps the order unfilled"
    );
    assert!(c.ome().get(&id).unwrap().status.is_live());
    assert_eq!(c.ome().get(&id).unwrap().filled_size, Decimal::ZERO);
}

// ── #351 反向验收 · a taker with latency is a two-stage in-flight leg ───
//
// With the latency dials engaged a taker is IN FLIGHT between decision and
// venue: it must not fill at decision time, it must match the book as it
// stands at execute_at (never the decision quote), and its ledger effects
// land at report_at. Each test below is RED against the historic
// fill-at-decision behaviour it replaces.

/// The lifecycle pins the three timestamps: decision 1000, match 1100,
/// report 1150 — no position and no cash movement before report_at.
#[test]
fn an_inflight_taker_does_not_fill_at_decision_time() {
    let mut c = core_with(FillModel {
        taker_latency_ms: 100,
        taker_rtt_ms: 50,
        ..FillModel::default()
    });
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(500))], 900);
    let (id, st) = c
        .place(buy(FillPolicy::Taker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    assert_eq!(
        st,
        OrderStatus::Pending,
        "in flight: the venue has not acked yet"
    );
    assert!(c.positions().open_positions().is_empty());
    assert_eq!(
        c.ledger().reserved(),
        dec!(4),
        "the FOK reservation holds while the leg is in flight"
    );

    // Before execute_at: still nothing — the leg has not reached the venue.
    c.tick(1_050).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Pending);
    assert!(c.positions().open_positions().is_empty());

    // At execute_at the venue MATCHES (the book still crosses) but the
    // report has not landed: no position, reservation still held.
    c.tick(1_100).unwrap();
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Pending,
        "matched at the venue, not yet reported locally"
    );
    assert!(c.positions().open_positions().is_empty());

    // At report_at the fill books: decision 1000, match 1100, report 1150.
    c.tick(1_150).unwrap();
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Filled);
    assert_eq!(c.positions().open_positions()[0].entry_price, dec!(0.40));
    // Taker entry: 4.00 notional + the 1.8% taker fee (0.072).
    assert_eq!(c.ledger().balance(), dec!(1000) - dec!(4) - dec!(0.072));
    assert_eq!(c.ledger().reserved(), Decimal::ZERO);
}

/// The decision book offered 500 shares at 0.40; the venue's book at
/// execute_at does not. A FOK that cannot fill on arrival dies — the
/// decision-time quote is not a fill.
#[test]
fn an_inflight_taker_matches_the_execute_at_book_not_the_decision_book() {
    let mut c = core_with(FillModel {
        taker_latency_ms: 100,
        taker_rtt_ms: 50,
        ..FillModel::default()
    });
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(500))], 900);
    let (id, _) = c
        .place(buy(FillPolicy::Taker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    // The ask is pulled before the order could arrive at the venue.
    c.book_snapshot("tok", vec![], vec![], 1_050);
    c.tick(1_100).unwrap(); // execute_at: nothing to walk → the venue kills it
    c.tick(1_150).unwrap(); // report_at: the kill reports
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::Rejected,
        "a FOK that cannot fill at execute_at dies, whatever the decision book had"
    );
    assert!(c.positions().open_positions().is_empty());
    assert_eq!(
        c.ledger().balance(),
        dec!(1000),
        "the reservation is released"
    );
    assert_eq!(c.ledger().reserved(), Decimal::ZERO);
}

/// A cancel that wins the race leaves no ghost fill: the dead leg is
/// dropped at its next drain, whatever the book does afterwards.
#[test]
fn a_cancel_that_wins_the_race_leaves_no_ghost_fill() {
    let mut c = core_with(FillModel {
        taker_latency_ms: 100,
        taker_rtt_ms: 50,
        ..FillModel::default()
    });
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(500))], 900);
    let (id, _) = c
        .place(buy(FillPolicy::Taker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    c.cancel(&id, 1_050).unwrap();
    c.tick(1_100).unwrap(); // execute_at passes — for a dead order
    c.tick(1_200).unwrap(); // report_at passes
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
    assert!(
        c.positions().open_positions().is_empty(),
        "no fill may report for an order the venue was told to forget"
    );
    assert_eq!(c.ledger().balance(), dec!(1000));
    assert_eq!(c.ledger().reserved(), Decimal::ZERO);
}

// ── #183 · a dry maker fills at most what the book offers ──────────────
//
// Before this, a crossing maker filled its WHOLE size at once, so a dry run
// could never reproduce the partial fills live hits on day one and every
// backtest read as a 100% fill rate.

/// ACCEPTANCE (#183): a maker only trades the crossing depth, and the three
/// records of that fact — order, position and ledger — agree.
#[test]
fn crossing_depth_limits_the_maker_fill_and_the_books_agree() {
    let mut c = core_with(FillModel::default());
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    // Only 4 shares rest at the crossing ask. That is the whole of what a
    // maker can trade on this book.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(4))], 1_100);

    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.filled_size, dec!(4), "depth, not order size: {o:?}");
    assert_eq!(o.status, OrderStatus::PartiallyFilled);
    assert_eq!(c.ome().remaining(&id), dec!(6), "the rest is still working");
    // What the fill cost is what the ledger moved: 4 × 0.40.
    assert_eq!(
        c.ledger().balance(),
        dec!(1000) - dec!(1.6),
        "cash follows the fill, not the order"
    );
    assert_eq!(
        c.ledger().reserved(),
        dec!(2.4),
        "0.40 × 6 unfilled shares stay committed while the order rests"
    );
    assert_eq!(c.ledger().available(), c.ledger().balance() - dec!(2.4));
    assert!(c.ledger().is_balanced());
    // The position holds the shares that actually traded.
    let pos = &c.positions().open_positions()[0];
    assert_eq!(pos.shares, dec!(4), "position = filled size");
    assert_eq!(pos.entry_price, dec!(0.40));
    assert_eq!(pos.cost_usd, dec!(1.6), "basis = cash actually spent");
    let audit = c.run_accounting_audit(1_200);
    assert!(audit.ok, "{}", audit.note);
}

/// ACCEPTANCE (#183): the remainder of a partially filled maker is a real,
/// cancellable order — the entry that live leaves half-open must be
/// closable in dry too.
#[test]
fn a_partially_filled_maker_keeps_its_remainder_cancellable() {
    let mut c = core_with(FillModel::default());
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(4))], 1_100);

    c.cancel(&id, 1_200).unwrap();

    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.status, OrderStatus::Cancelled);
    assert_eq!(o.filled_size, dec!(4), "cancelling never unwinds a fill");
    assert_eq!(c.ledger().reserved(), Decimal::ZERO, "2.4 released");
    assert_eq!(
        c.ledger().available(),
        c.ledger().balance(),
        "stranded cash is unusable cash"
    );
    assert!(c.ledger().is_balanced());
    // The filled half stays a position worth exactly what was paid.
    let pos = &c.positions().open_positions()[0];
    assert_eq!(pos.shares, dec!(4));
    assert_eq!(pos.cost_usd, dec!(1.6));
    assert_eq!(c.ledger().balance(), dec!(1000) - dec!(1.6));
    let audit = c.run_accounting_audit(1_300);
    assert!(audit.ok, "{}", audit.note);
}

/// ACCEPTANCE (#183): successive books keep filling the order up to each
/// book's depth until it is whole — a partial fill is not a terminal state.
#[test]
fn partial_maker_fills_accumulate_until_the_order_is_whole() {
    let mut c = core_with(FillModel::default());
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();

    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(4))], 1_100);
    assert_eq!(c.ome().get(&id).unwrap().filled_size, dec!(4));
    // A shallower book a moment later adds only what it offers.
    c.book_snapshot("tok", vec![], vec![(dec!(0.39), dec!(3))], 1_200);
    assert_eq!(c.ome().get(&id).unwrap().filled_size, dec!(7));
    assert_eq!(
        c.ome().get(&id).unwrap().status,
        OrderStatus::PartiallyFilled
    );
    // The third book covers the remaining 3: the order ends whole.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(10))], 1_300);

    let o = c.ome().get(&id).unwrap();
    assert_eq!(o.filled_size, dec!(10));
    assert_eq!(o.status, OrderStatus::Filled);
    assert_eq!(c.ome().remaining(&id), Decimal::ZERO);
    assert_eq!(c.ledger().reserved(), Decimal::ZERO);
    assert_eq!(c.ledger().balance(), dec!(1000) - dec!(4));
    // Every chunk was a maker fill at the resting limit, so the average is
    // the quote itself and no taker fee was ever charged.
    assert_eq!(o.avg_fill_price, Some(dec!(0.40)));
    assert_eq!(c.positions().open_positions()[0].shares, dec!(10));
    let audit = c.run_accounting_audit(1_400);
    assert!(audit.ok, "{}", audit.note);
}

/// Two chunks inside the SAME millisecond are two executions, not one
/// revised report: each carries its own trade identity, so the second books
/// on top of the first instead of being swallowed as a downward revision.
#[test]
fn two_partial_chunks_in_one_millisecond_both_book() {
    let mut c = core_with(FillModel::default());
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(3))], 1_100);
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(4))], 1_100);

    assert_eq!(
        c.ome().get(&id).unwrap().filled_size,
        dec!(7),
        "3 + 4 in the same millisecond: the second chunk is a second trade"
    );
    assert_eq!(c.ledger().balance(), dec!(1000) - dec!(2.8));
    assert!(c.ledger().is_balanced());
}

/// A crossing book whose levels are empty offers nothing to take: the order
/// stays whole and resting rather than filling against zero volume.
#[test]
fn a_crossing_book_with_no_volume_leaves_the_maker_resting() {
    let mut c = core_with(FillModel::default());
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), Decimal::ZERO)], 1_100);

    let o = c.ome().get(&id).unwrap();
    assert!(o.status.is_live(), "no volume traded: {o:?}");
    assert_eq!(o.filled_size, Decimal::ZERO);
    assert!(c.positions().open_positions().is_empty());
    assert_eq!(c.ledger().reserved(), dec!(4), "still committed");
}

/// The queue-share dial scales a maker fill below the depth as well, so a
/// backtest can price being behind the queue. Identity is its ceiling.
#[test]
fn depth_share_dial_scales_the_maker_fill() {
    let mut c = core_with(FillModel {
        maker_depth_share_bps: 5_000,
        ..FillModel::default()
    });
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    // Deep book: the dial, not the depth, is the binding constraint.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1_100);

    let filled = c.ome().get(&id).unwrap().filled_size;
    assert!(
        filled > Decimal::ZERO && filled <= dec!(5),
        "a 50% queue share of a 10-share order: {filled}"
    );
    assert_eq!(c.positions().open_positions()[0].shares, filled);
    assert_eq!(c.ledger().balance(), dec!(1000) - filled * dec!(0.40));
    assert!(c.ledger().is_balanced());
    let audit = c.run_accounting_audit(1_200);
    assert!(audit.ok, "{}", audit.note);
}

// ── #464: DRY post-only 对齐 LIVE —— 提交瞬间穿价的 maker 单被拒 ────────

/// #464 acceptance: a maker (post-only) BUY submitted with the ask ALREADY
/// at or inside its limit is refused, exactly as the venue refuses the
/// same POST live (`WouldCross`). Before this, dry filled it at its own
/// limit with a zero maker fee — maker economics for an order live would
/// only have executed by paying a taker fee, a systematically optimistic
/// bias. A MakerThenTaker entry then escalates to taker on the same book
/// through `reject_live_result`'s existing WouldCross branch.
#[test]
fn a_post_only_maker_that_crosses_at_submission_is_refused_like_live() {
    let mut c = core_with(FillModel::default());
    // The ask (0.40) is AT the buy limit — the venue's post-only check
    // crosses on `<=`, and so does `Book::crosses`.
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 900);
    let (id, status) = c
        .place(
            buy(FillPolicy::MakerThenTaker, dec!(0.40), dec!(10)),
            60_000,
            1_000,
        )
        .unwrap();
    // The passive leg itself is refused…
    assert_eq!(status, OrderStatus::Rejected, "the POST is refused");
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Rejected);
    // …and the MakerThenTaker contract takes over on the SAME tick: the
    // escalation places a taker leg and fills it at the book's price —
    // LIVE's own WouldCross reaction, reached through the same
    // `reject_live_result` branch.
    assert_eq!(c.positions().open_positions().len(), 1);
    let pos = &c.positions().open_positions()[0];
    assert_eq!(pos.entry_price, dec!(0.40));
    assert!(
        pos.flows.entry_fee_usd > Decimal::ZERO,
        "the escalation pays the taker fee (dry: {})",
        pos.flows.entry_fee_usd
    );
    // The refused leg's BUY reservation was released, not stranded: the
    // books carry only the escalated leg's commitment.
    assert!(c.ledger().is_balanced());
}

/// A plain `Maker` order has no escalation contract: the same crossing
/// POST is refused, nothing fills, no position exists — the live parity
/// for pure passive orders.
#[test]
fn a_refused_pure_maker_crossing_places_nothing() {
    let mut c = core_with(FillModel::default());
    c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 900);
    let (_, status) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    assert_eq!(status, OrderStatus::Rejected);
    assert!(c.positions().open_positions().is_empty());
    assert_eq!(c.ledger().balance(), dec!(1000), "no cash committed");
    assert_eq!(
        c.last_error.as_ref().map(|e| e.code),
        Some(CoreErrorCode::WouldCross)
    );
}

/// Reverse acceptance: a maker that does NOT cross at submission keeps
/// the historic path untouched — it rests and fills at its own limit when
/// a later tick crosses it (the honest maker fill the old tests pin).
#[test]
fn a_maker_that_rests_first_keeps_its_honest_fill() {
    let mut c = core_with(FillModel::default());
    // No ask at the limit when the order posts.
    c.book_snapshot("tok", vec![], vec![(dec!(0.41), dec!(100))], 900);
    let (id, _) = c
        .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
        .unwrap();
    assert!(
        c.ome().get(&id).unwrap().status.is_live(),
        "rested, not refused"
    );
    // The ask comes down INTO the limit on a later tick: maker fill at
    // the resting limit, zero fee.
    c.book_snapshot(
        "tok",
        vec![(dec!(0.39), dec!(100))],
        vec![(dec!(0.40), dec!(100))],
        1_500,
    );
    assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Filled);
    let pos = &c.positions().open_positions()[0];
    assert_eq!(pos.entry_price, dec!(0.40));
    assert_eq!(pos.flows.entry_fee_usd, Decimal::ZERO);
}
