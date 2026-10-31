use super::*;

fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
    use rust_decimal::prelude::FromPrimitive;
    let bd = Decimal::from_f64(bid).unwrap();
    let ad = Decimal::from_f64(ask).unwrap();
    OrderbookSnapshot {
        token_id: "t".into(),
        bids: vec![(bd, dec!(100))],
        asks: vec![(ad, dec!(100))],
        bid_depth: dec!(100),
        ask_depth: dec!(100),
        obi: Decimal::ZERO,
        spread: ad - bd,
        spread_pct: Decimal::ZERO,
        best_bid: bd,
        best_ask: ad,
        mid_price: (bd + ad) / Decimal::TWO,
        timestamp: 0,
    }
}

/// The shipped default must stay the shipped default. If a change flips it,
/// this is the test that says so before a replay is needed (#203).
#[test]
fn default_fee_schedule_is_the_shipped_one() {
    let s = fee_schedule();
    assert_eq!(s.name, "legacy_quadratic");
    assert_eq!(s.rate, dec!(0.125));
    assert_eq!(s.exponent, 2);
}

/// The holder view: the protective stop comes out FLAT at the wide
/// containment width (no ramp), while a non-holder's stop keeps its
/// tightening ramp — and the shipped widths are the calibrated ones.
#[test]
fn holder_view_flattens_the_stop_at_the_wide_width() {
    let cfg = ExitConfig::default();
    // The @almach containment compromise, pinned: 50% wide vs 12% base.
    assert_eq!(cfg.holder_stop_pct, HOLDS_STOP_PCT);
    assert_eq!(cfg.stop_loss_pct, dec!(12));

    // Non-holder: the ramp tightens from 12% (far) toward the 10% floor
    // (at expiry itself — force_exit 0 makes the floor time the expiry).
    let far = effective_stop_pct(cfg.stop_loss_pct, 600, &cfg);
    let near = effective_stop_pct(cfg.stop_loss_pct, 10, &cfg);
    let floor = effective_stop_pct(cfg.stop_loss_pct, 0, &cfg);
    assert_eq!(far, dec!(12));
    assert_eq!(floor, dec!(10));
    assert!(
        near > floor && near < far,
        "mid-ramp between 10 and 12: {near}"
    );

    // Holder: flat at the wide width at EVERY time-left — the ramp may
    // not re-open the bleed through the back door.
    for t in [600i64, 300, 60, 10, 1] {
        assert_eq!(
            holder_effective_stop_pct(t, &cfg),
            dec!(50),
            "holder stop must be flat 50% at t={t}s"
        );
    }
    // The view is a pure projection: the kernel-wide config is untouched.
    assert_eq!(cfg.stop_loss_pct, dec!(12));
    assert_eq!(cfg.stop_min_pct, dec!(10));
}

/// The legacy curve as charged: `0.125*(p*(1-p))^2` per share, quoted as a
/// percentage of price (p=0.40 -> $0.0072/share, 1.8% of price).
#[test]
fn legacy_schedule_prices_as_documented() {
    assert_eq!(taker_fee_pct(dec!(0.4)), dec!(1.8));
    assert_eq!(taker_fee_pct(dec!(0.5)), dec!(1.5625));
    assert_eq!(taker_fee_pct(dec!(0)), Decimal::ZERO);
}

/// #203's decision rests on this ratio: the published crypto schedule is
/// 2.33x the legacy curve at p=0.40 and 5.30x at p=0.12. A test, not a
/// comment, because the whole "switching is a cost increase" claim is this
/// arithmetic.
#[test]
fn official_schedule_is_the_published_multiple_of_legacy() {
    let official = fee_schedule_by_name("official").expect("official schedule");
    assert_eq!(official.rate, dec!(0.07));
    assert_eq!(official.exponent, 1);
    let legacy = legacy_quadratic_schedule();
    // The ratio is taken through the ONE production spelling of the curve
    // (#234, item 2): a restated copy here would test the copy, not the fee.
    let at = |p: Decimal| official.fee_per_share(p) / legacy.fee_per_share(p);
    assert_eq!(at(dec!(0.40)).round_dp(2), dec!(2.33));
    assert_eq!(at(dec!(0.12)).round_dp(2), dec!(5.30));
}

/// A price outside `(0, 1)` charges nothing — for EVERY schedule, in both
/// conventions (#234, item 3). The `p > 1` half is the money-path case: with
/// `exponent >= 1` the un-clamped curve prices a NEGATIVE fee there, i.e. a
/// rebate on a live fill. `p == 1` is 0 for the accidental reason that the
/// base is 0, so it is pinned here rather than left to arithmetic luck.
#[test]
fn out_of_range_prices_are_charged_nothing() {
    for s in [legacy_quadratic_schedule(), official_schedule()] {
        for p in [dec!(1), dec!(1.0000001), dec!(1.5), dec!(2), dec!(100)] {
            assert_eq!(
                s.fee_per_share(p),
                Decimal::ZERO,
                "{}: {p} is not a price in (0,1)",
                s.name
            );
            assert_eq!(
                s.fee_pct(p),
                Decimal::ZERO,
                "{}: {p} must charge nothing as a percentage either",
                s.name
            );
            assert!(
                s.fee_per_share(p) >= Decimal::ZERO,
                "{}: a fee at {p} must never be a rebate",
                s.name
            );
        }
        for p in [dec!(0), dec!(-0.5), dec!(-1)] {
            assert_eq!(s.fee_per_share(p), Decimal::ZERO, "{}: p={p}", s.name);
            assert_eq!(s.fee_pct(p), Decimal::ZERO, "{}: p={p}", s.name);
        }
    }
    // The charge path in force goes through the same clamp.
    assert_eq!(taker_fee_pct(dec!(1)), Decimal::ZERO);
    assert_eq!(taker_fee_pct(dec!(1.5)), Decimal::ZERO);
    assert_eq!(taker_fee_pct(dec!(2)), Decimal::ZERO);
}

/// Every schedule must name where its parameters come from. `0.07` is only
/// authoritative because the publication says so; a schedule with a blank
/// source is a number nobody can check (#203 acceptance).
#[test]
fn every_schedule_declares_its_source() {
    for name in FEE_SCHEDULE_NAMES {
        let s = fee_schedule_by_name(name).unwrap_or_else(|| panic!("{name} missing"));
        assert!(
            s.source.len() > 20,
            "{name}: the schedule must say who published its parameters, got {:?}",
            s.source
        );
        assert_eq!(s.name, *name, "lookup name and declared name disagree");
    }
    assert!(fee_schedule_by_name("no_such_model").is_none());
}

#[test]
fn force_exit_fires_at_deadline() {
    // The guillotine is OFF by default (calibrated: the deadline amputated
    // rounds that went on to pay), so the deadline itself is pinned here:
    // the RULE still works when an operator turns it on.
    let cfg = ExitConfig {
        force_exit_sec: 120,
        ..Default::default()
    };
    let st = ExitState::new(dec!(0.4), 0);
    let b = book(0.39, 0.41);
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: Some(&b),
        fallback_price: None,
        time_left_sec: 100,
        hold_sec: 50,
        state: &st,
        now_ms: 1000,
        cfg: &cfg,
    });
    assert_eq!(d.unwrap().reason, ExitReason::ForceExit);
}

/// The calibrated default: `force_exit_sec = 0` DISABLES the guillotine —
/// including the `time_left == 0` edge a non-positive threshold would
/// otherwise misfire on ahead of settlement.
#[test]
fn force_exit_default_is_disabled() {
    let cfg = ExitConfig::default();
    assert_eq!(cfg.force_exit_sec, 0);
    assert_eq!(cfg.min_time_left_sec, 0);
    let st = ExitState::new(dec!(0.4), 0);
    let b = book(0.39, 0.41);
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: Some(&b),
        fallback_price: None,
        time_left_sec: 0,
        hold_sec: 900,
        state: &st,
        now_ms: 1000,
        cfg: &cfg,
    });
    // At expiry with the guillotine OFF the ladder is SILENT — the position
    // belongs to settlement (dry: `drive_settlement`; live: the venue
    // answer). No dying-book rule (stop included) may fire: letting them
    // would sell a $1 winner at the residual bid.
    assert!(
        d.is_none(),
        "expiry + guillotine OFF = hold; settlement owns the position"
    );
}

#[test]
fn take_profit_is_maker_first() {
    let cfg = ExitConfig::default();
    let st = ExitState::new(dec!(0.4), 0);
    // Bid +150% ≥ TP 100%.
    let b = book(1.0, 1.0);
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: Some(&b),
        fallback_price: None,
        time_left_sec: 600,
        hold_sec: 20,
        state: &st,
        now_ms: 1000,
        cfg: &cfg,
    })
    .unwrap();
    assert_eq!(d.reason, ExitReason::TakeProfit);
    assert!(d.use_maker);
}

#[test]
fn default_stop_is_tight_but_an_explicit_wide_stop_does_not_fire() {
    // Default stop was tightened to 15% (see the §26 tuning note), so a -30%
    // bid now fires a StopLoss in simple mode.
    let cfg = ExitConfig::default();
    let st = ExitState::new(dec!(0.4), 0);
    let b = book(0.28, 0.30); // -30%
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: Some(&b),
        fallback_price: None,
        time_left_sec: 600,
        hold_sec: 20,
        state: &st,
        now_ms: 1000,
        cfg: &cfg,
    })
    .unwrap();
    assert_eq!(d.reason, ExitReason::StopLoss);
    assert!(!d.use_maker);

    // An explicit wide stop (the old 50%) still holds through -30%.
    let wide = ExitConfig {
        stop_loss_pct: dec!(50),
        ..Default::default()
    };
    assert!(
        decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: None,
            time_left_sec: 600,
            hold_sec: 20,
            state: &st,
            now_ms: 1000,
            cfg: &wide,
        })
        .is_none()
    );
}

#[test]
fn wick_below_mid_does_not_trigger_stop() {
    // bid 0.20 vs mid 0.40 → a 50% wick; protective stop must not fire on it.
    let cfg = ExitConfig::default();
    let st = ExitState::new(dec!(0.4), 0);
    let b = book(0.20, 0.60);
    let verdict = decide_exit_verdict(ExitTickInput {
        entry_price: dec!(0.4),
        book: Some(&b),
        fallback_price: None,
        time_left_sec: 600,
        hold_sec: 20,
        state: &st,
        now_ms: 1000,
        cfg: &cfg,
    });
    assert!(verdict.decision.is_none());
    // …but the trigger it withheld must be reported (P0 #177): a stop that
    // fired on the raw bid and was held back is NOT the same as no trigger.
    let s = verdict
        .suppressed_stop
        .expect("a withheld protective stop must be reported");
    assert_eq!(s.bid, dec!(0.20));
    assert_eq!(s.mid, dec!(0.40));
    assert_eq!(s.pnl_pct_at_bid, dec!(-50));
    assert_eq!(s.pnl_pct_at_mid, dec!(0));
}

/// A bid-less book is the situation the protective stop exists for: the
/// mid is the only price left, and it must be able to fire the stop.
#[test]
fn zero_bid_falling_mid_fires_stop() {
    let cfg = ExitConfig::default();
    let st = ExitState::new(dec!(0.4), 0);
    let mut b = book(0.30, 0.30);
    b.best_bid = Decimal::ZERO;
    b.mid_price = dec!(0.15);
    b.bids.clear();
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: Some(&b),
        fallback_price: None,
        time_left_sec: 600,
        hold_sec: 20,
        state: &st,
        now_ms: 1000,
        cfg: &cfg,
    })
    .expect("a collapsed mid with no bid must still fire the stop");
    assert_eq!(d.reason, ExitReason::StopLoss);
    assert!(!d.use_maker);
}

/// No book at all: the position's own last price (fresh) stands in.
#[test]
fn missing_book_uses_fresh_last_price_for_stop() {
    let cfg = ExitConfig::default();
    let st = ExitState::new(dec!(0.4), 0);
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: None,
        fallback_price: Some(dec!(0.15)),
        time_left_sec: 600,
        hold_sec: 20,
        state: &st,
        now_ms: 1000,
        cfg: &cfg,
    })
    .expect("a fresh last price must be able to fire the stop");
    assert_eq!(d.reason, ExitReason::StopLoss);
}

/// …but the fallback is bounded by freshness: a stale last price can no
/// longer speak for the market.
#[test]
fn stale_last_price_does_not_fire_stop() {
    let cfg = ExitConfig::default();
    let mut st = ExitState::new(dec!(0.4), 0);
    st.last_quote_at_ms = 0; // written before the field existed = age unknown
    // Age the quote well past the bound.
    let aged = ExitState {
        last_quote_at_ms: 1,
        ..st
    };
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: None,
        fallback_price: Some(dec!(0.15)),
        time_left_sec: 600,
        hold_sec: 20,
        state: &aged,
        now_ms: 1 + cfg.max_last_price_age_sec * 1_000 + 1,
        cfg: &cfg,
    });
    assert!(d.is_none());
    // The same price, still fresh, does fire — the bound is the only
    // difference between the two.
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: None,
        fallback_price: Some(dec!(0.15)),
        time_left_sec: 600,
        hold_sec: 20,
        state: &aged,
        now_ms: 1 + cfg.max_last_price_age_sec * 1_000,
        cfg: &cfg,
    });
    assert_eq!(d.unwrap().reason, ExitReason::StopLoss);
}

/// A real decline that ALSO trips the wick ratio (thin book, bid hanging
/// far under the ask) is not a pin bar: the mid collapsed with it, so the
/// stop must fire rather than hide behind the wick guard.
#[test]
fn wick_ratio_with_collapsed_mid_fires_stop() {
    let cfg = ExitConfig::default();
    let st = ExitState::new(dec!(0.4), 0);
    let b = book(0.05, 0.075); // mid 0.0625: -84% from entry, wick 20%
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: Some(&b),
        fallback_price: None,
        time_left_sec: 600,
        hold_sec: 20,
        state: &st,
        now_ms: 1000,
        cfg: &cfg,
    })
    .expect("a mid that fell in sync is a real decline, not a wick");
    assert_eq!(d.reason, ExitReason::StopLoss);
}

/// A book with no levels on either side ("the book was swept") has no price
/// at all: its placeholder mid must not veto the stop, so the last known
/// price decides.
#[test]
fn an_emptied_book_falls_back_to_the_last_price() {
    let cfg = ExitConfig::default();
    let st = ExitState::new(dec!(0.4), 0);
    let mut b = book(0.0, 0.0);
    b.best_bid = Decimal::ZERO;
    b.best_ask = Decimal::ONE;
    b.mid_price = dec!(0.5); // the from_levels placeholder
    b.bids.clear();
    b.asks.clear();
    let d = decide_exit(ExitTickInput {
        entry_price: dec!(0.4),
        book: Some(&b),
        fallback_price: Some(dec!(0.15)),
        time_left_sec: 600,
        hold_sec: 20,
        state: &st,
        now_ms: 1000,
        cfg: &cfg,
    })
    .expect("an emptied book must not hide the stop behind a placeholder mid");
    assert_eq!(d.reason, ExitReason::StopLoss);
}

/// A dead book — no levels on either side, so the mid is only the
/// construction placeholder — plus a fallback quote far ABOVE entry is a
/// paper profit with no executable price behind it. The fallback may not
/// manufacture a take-profit any more than it may hide a stop.
#[test]
fn dead_book_fallback_cannot_take_profit() {
    let cfg = ExitConfig::default();
    let st = ExitState::new(dec!(0.4), 0);
    let mut b = book(0.0, 0.0);
    b.best_bid = Decimal::ZERO;
    b.best_ask = Decimal::ZERO;
    b.mid_price = Decimal::ZERO;
    b.bids.clear();
    b.asks.clear();
    assert!(
        decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: Some(dec!(1.0)), // +150% on paper
            time_left_sec: 600,
            hold_sec: 20,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        })
        .is_none()
    );
}

// ── F6: a one-sided book must never mint an executable sell price ───────

fn one_sided_book(ask: Decimal) -> OrderbookSnapshot {
    OrderbookSnapshot {
        token_id: "t".into(),
        bids: vec![],
        asks: vec![(ask, dec!(100))],
        bid_depth: Decimal::ZERO,
        ask_depth: dec!(100),
        obi: Decimal::ZERO,
        spread: Decimal::ZERO,
        spread_pct: Decimal::ZERO,
        best_bid: Decimal::ZERO,
        best_ask: ask,
        // A one-sided book carries NO mid (see strategy_logic
        // `from_sorted_levels`): `(0 + ask)/2` was the phantom price that
        // started F6.
        mid_price: Decimal::ZERO,
        timestamp: 0,
    }
}

#[test]
fn no_bid_means_no_executable_price_even_with_a_high_ask() {
    // ask 0.90, zero bids: the old code returned mid = 0.45 here.
    let b = one_sided_book(dec!(0.90));
    assert_eq!(executable_bid(Some(&b)), Decimal::ZERO);
    assert_eq!(executable_bid::<OrderbookSnapshot>(None), Decimal::ZERO);
}

#[test]
fn force_exit_does_not_fire_without_an_executable_bid() {
    // The audit's F6 scenario: maker entry @0.40, then only an ask 0.90
    // remains. Deadline pressure must not mint a 0.45 SELL out of thin air.
    let cfg = ExitConfig::default();
    let st = ExitState::new(dec!(0.4), 0);
    let b = one_sided_book(dec!(0.90));
    assert!(
        decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            // The stale last-known price is a reference, not a quote: even
            // offered as fallback it must not price a forced SELL.
            fallback_price: Some(dec!(0.45)),
            time_left_sec: 10,
            hold_sec: 800,
            state: &st,
            now_ms: 900_000,
            cfg: &cfg,
        })
        .is_none()
    );
}

#[test]
fn reference_price_prefers_bid_then_two_sided_mid_then_fallback() {
    let two_sided = book(0.40, 0.44);
    assert_eq!(reference_price(Some(&two_sided), dec!(9)), dec!(0.40));
    // One-sided book: no phantom mid — fall back to the last known price.
    let one_sided = one_sided_book(dec!(0.90));
    assert_eq!(reference_price(Some(&one_sided), dec!(9)), dec!(9));
    assert_eq!(reference_price::<OrderbookSnapshot>(None, dec!(9)), dec!(9));
}

// ── #225: a bid-less book must not leave the stop with no price ──────────

/// Build a book the way the kernel does, so the placeholder/sentinel prices
/// an empty side gets are part of what is under test.
fn shaped(bids: Vec<(Decimal, Decimal)>, asks: Vec<(Decimal, Decimal)>) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels("tok", bids, asks, 1_000)
}

/// The stop's judgement for a book, with a last known price of 0.40 taken a
/// second ago (fresh for the default 30s bound).
fn judged(book: &OrderbookSnapshot, fallback: Decimal) -> StopReference {
    stop_reference(
        Some(book),
        Some(fallback),
        &ExitState::new(dec!(0.40), 1_000),
        1_000,
        &ExitConfig::default(),
    )
    .expect("every book here leaves the stop something to judge on")
}

#[test]
fn a_bidless_book_judges_the_stop_on_its_ask_and_prices_nothing() {
    let b = shaped(vec![], vec![(dec!(0.15), dec!(1000))]);
    assert_eq!(b.mid_price, Decimal::ZERO, "F6: no bid ⇒ no mid");

    let r = judged(&b, dec!(0.40));
    assert_eq!(r.price, dec!(0.15), "the ask is the only real level left");
    assert_eq!(r.source, StopRefSource::Ask);
    assert_eq!(r.suppressed_bid, None);
    // …and the judgement still cannot become an order: the F6 gate is
    // untouched, so all it can produce is a REPORTED held stop (#179).
    assert_eq!(executable_bid(Some(&b)), Decimal::ZERO);
}

#[test]
fn an_ask_above_the_last_known_price_never_loosens_or_tightens_the_stop() {
    let b = shaped(vec![], vec![(dec!(0.90), dec!(1000))]);
    // The buy side was swept but the market did not fall: the remembered
    // price stays the judgement. `min` is what guarantees this branch can
    // only ever FIRE a stop the old code fired, never hide one.
    assert_eq!(judged(&b, dec!(0.40)).price, dec!(0.40));
    // …and an ask ABOVE a last known price that itself collapsed does not
    // raise the judgement back up.
    assert_eq!(judged(&b, dec!(0.10)).price, dec!(0.10));
}

#[test]
fn a_book_with_no_levels_at_all_carries_no_price_information() {
    let b = shaped(vec![], vec![]);
    // The empty ask side's `1` is an entry-side sentinel (pre-existing API,
    // see `strategy_logic::model`), not a dollar of demand — reading it as a
    // quote would judge every stop against a price nobody offered.
    assert_eq!(b.best_ask, Decimal::ONE);
    assert_eq!(b.mid_price, Decimal::ZERO);

    let r = judged(&b, dec!(0.40));
    assert_eq!(r.price, dec!(0.40), "only the last known price is left");
    assert_eq!(r.source, StopRefSource::LastKnown);
}

#[test]
fn a_mid_without_a_bid_is_never_the_judgement() {
    // A hand-built or legacy snapshot can still carry the phantom
    // `(0 + ask)/2` mid that F6 killed. `from_levels` no longer produces it,
    // so the branch that read it is gone — and it must not come back
    // through a snapshot that was not built by the kernel.
    let mut b = shaped(vec![], vec![]);
    b.mid_price = dec!(0.15);

    let r = judged(&b, dec!(0.40));
    assert_eq!(r.source, StopRefSource::LastKnown);
    assert_eq!(r.price, dec!(0.40));
}

#[test]
fn a_stale_last_price_leaves_the_live_ask_as_the_only_witness() {
    // The remembered price is past its freshness bound, so it drops out —
    // but the ask is a live level in front of us, and there is no reason to
    // fall silent while one exists (#225).
    let b = OrderbookSnapshot::from_levels("tok", vec![], vec![(dec!(0.15), dec!(1000))], 32_000);
    let r = stop_reference(
        Some(&b),
        Some(dec!(0.40)),
        &ExitState::new(dec!(0.40), 1_000), // last quote 31s ago
        32_000,
        &ExitConfig::default(),
    )
    .expect("a live ask is a price");
    assert_eq!(r.price, dec!(0.15));
    assert_eq!(r.source, StopRefSource::Ask);

    // With no ask either there is nothing at all, and the stop is silent.
    let dark = OrderbookSnapshot::from_levels("tok", vec![], vec![], 32_000);
    assert!(
        stop_reference(
            Some(&dark),
            Some(dec!(0.40)),
            &ExitState::new(dec!(0.40), 1_000),
            32_000,
            &ExitConfig::default(),
        )
        .is_none(),
        "no bid, no ask and a stale memory is the one case with no witness"
    );
}

// ── F8: fee schedules ────────────────────────────────────────────────────
//
// These assert on schedule VALUES, never on an installed one: the active
// schedule is set-once per process, so a test that swapped it would poison
// every other test in the binary. `FeeSchedule` is a plain `Copy` value and
// needs no installation to be measured.

#[test]
fn legacy_schedule_fee_is_unchanged() {
    let legacy = legacy_quadratic_schedule();
    // p=0.50: 0.125 * (0.25)^2 = 0.0078125 USD/share → /0.50 = 1.5625%.
    assert_eq!(legacy.fee_pct(dec!(0.50)), dec!(1.5625));
    // The process default is this curve, so the charge path agrees.
    assert_eq!(taker_fee_pct(dec!(0.50)), legacy.fee_pct(dec!(0.50)));
    assert_eq!(legacy.fee_pct(dec!(0)), Decimal::ZERO);
    assert_eq!(legacy.fee_pct(dec!(-1)), Decimal::ZERO);
    // At and above a full dollar nothing is charged (#234, item 3): `p == 1`
    // for the base's sake, `p > 1` because squaring must not be what hides a
    // rebate.
    assert_eq!(legacy.fee_pct(dec!(1)), Decimal::ZERO);
    assert_eq!(legacy.fee_pct(dec!(1.5)), Decimal::ZERO);
    assert_eq!(legacy.fee_per_share(dec!(1.5)), Decimal::ZERO);
}

#[test]
fn official_schedule_fee_matches_official_parameters() {
    // Official: fee_usd = shares * 0.07 * p * (1-p). 100 shares @0.50 →
    // 100 * 0.07 * 0.25 = $1.75. As a percentage of the fill price:
    // 0.07 * 0.5 * 100 = 3.5%, and 3.5% * 0.50 * 100 shares = $1.75. The
    // per-share and percentage conventions must agree at every price.
    let official = official_schedule();
    for p in [dec!(0.50), dec!(0.40), dec!(0.62), dec!(0.95)] {
        let per_share_fee = dec!(0.07) * p * (Decimal::ONE - p);
        let pct = official.fee_pct(p);
        assert_eq!(
            (pct / Decimal::ONE_HUNDRED) * p,
            per_share_fee,
            "pct convention must equal the official per-share fee at p={p}"
        );
    }
    // p=0.50: 0.07 * 0.25 = 0.0175 USD/share → /0.50 = 3.5% of price.
    assert_eq!(official.fee_pct(dec!(0.50)), dec!(3.5));
    // 100 shares @0.50 → exactly $1.75.
    let fee_usd = (official.fee_pct(dec!(0.50)) / Decimal::ONE_HUNDRED) * dec!(0.50) * dec!(100);
    assert_eq!(fee_usd, dec!(1.75));
    // At p=0.50 the published curve is 2.24x the legacy one — the cost jump
    // #203 measured.
    let legacy = legacy_quadratic_schedule();
    assert_eq!(
        (official.fee_pct(dec!(0.50)) / legacy.fee_pct(dec!(0.50))).round_dp(2),
        dec!(2.24)
    );
    // Edge prices charge nothing. `p > 1` is the one that matters here: this
    // schedule's exponent is 1, so without the clamp `p*(1-p)` is negative
    // and the fee becomes a REBATE — money moving the wrong way (#234,
    // item 3). `p == 1` is 0 for the base's sake.
    assert_eq!(official.fee_pct(dec!(0)), Decimal::ZERO);
    assert_eq!(official.fee_pct(dec!(1)), Decimal::ZERO);
    assert_eq!(official.fee_pct(dec!(1.5)), Decimal::ZERO);
    assert!(
        official.fee_per_share(dec!(1.5)) >= Decimal::ZERO,
        "a fee above $1 of price must never be negative"
    );
}

#[test]
fn crypto_fee_flips_the_marginal_round_trip_sign() {
    // Audit F8: 100 shares taker 0.50 in → 0.52 out. Legacy ≈ +0.44 net;
    // official crypto fee ≈ −1.497 net. The sign must flip.
    let round_trip = |s: &FeeSchedule| -> Decimal {
        let entry_fee = (s.fee_pct(dec!(0.50)) / Decimal::ONE_HUNDRED) * dec!(0.50) * dec!(100);
        let exit_fee = (s.fee_pct(dec!(0.52)) / Decimal::ONE_HUNDRED) * dec!(0.52) * dec!(100);
        (dec!(0.52) - dec!(0.50)) * dec!(100) - entry_fee - exit_fee
    };
    let legacy = round_trip(&legacy_quadratic_schedule());
    let crypto = round_trip(&official_schedule());
    assert!(
        legacy > Decimal::ZERO,
        "legacy round trip was profitable: {legacy}"
    );
    assert!(
        crypto < Decimal::ZERO,
        "crypto round trip must lose: {crypto}"
    );
}

// ── #267: a bid-less book must not put the profit side out of work ───────

/// The exact tick the issue names (#267, acceptance 1): the buy side has been
/// swept (`bid 0 / ask 0.90`), the position has run to a high above the
/// trailing arming threshold, and its last known price is inside the
/// judgement window. Gating every profit rule on `live` made this a `hold`:
/// the stop still fired through its fallback while the profit side went
/// dark, so the ladder kept its loss half and lost its profit half.
#[test]
fn a_bidless_book_judges_the_profit_side_on_a_fresh_last_known_price() {
    // Trailing is explicitly ON here: this test pins the #267 MECHANISM
    // (a bid-less book still JUDGES the profit side), which is orthogonal
    // to the calibrated default of banking nothing via trailing sells.
    let cfg = ExitConfig {
        trailing_enabled: true,
        ..Default::default()
    };
    let now = 1_000_000;
    let entry = dec!(0.40);
    let b = one_sided_book(dec!(0.90));
    assert_eq!(
        executable_bid(Some(&b)),
        Decimal::ZERO,
        "F6: a one-sided book is still not priceable, so the decision below \
         cannot be coming from a bid"
    );
    // `high_pnl_pct` is a RUNNING high: 50% is what the position reached
    // before the buy side emptied, 0.48 (+20%) is where its own last known
    // price now sits. Both are `check_exits`' own inputs.
    let st = ExitState {
        high_pnl_pct: dec!(50),
        last_quote_at_ms: now - 5_000,
        ..ExitState::new(entry, now)
    };

    fn tick<'a>(
        b: &'a OrderbookSnapshot,
        state: &'a ExitState,
        cfg: &'a ExitConfig,
        now_ms: i64,
    ) -> ExitTickInput<'a> {
        ExitTickInput {
            entry_price: dec!(0.40),
            book: Some(b),
            fallback_price: Some(dec!(0.48)),
            time_left_sec: 600,
            hold_sec: 60,
            state,
            now_ms,
            cfg,
        }
    }

    let verdict = decide_exit_verdict(tick(&b, &st, &cfg, now));
    assert_eq!(
        verdict.decision.map(|d| d.reason),
        Some(ExitReason::TrailingStop),
        "the profit side must judge on the remembered price, not go dark"
    );
    assert!(
        verdict.suppressed_stop.is_none(),
        "nothing was withheld: the stop never fired here"
    );
    assert!(
        verdict.stop_reference.is_none(),
        "a profit-side exit carries no stop reference: the report is about \
         the bid, and the two sides judge independently (#267)"
    );

    // The freshness bound is what governs, not the book's emptiness: the
    // same tick with the memory past its window has nothing honest left to
    // judge a profit on, and holds.
    let aged = ExitState {
        last_quote_at_ms: now - (cfg.max_last_price_age_sec + 1) * 1_000,
        ..st.clone()
    };
    assert!(
        decide_exit_verdict(tick(&b, &aged, &cfg, now))
            .decision
            .is_none(),
        "a remembered price past its budget is not a judgement"
    );
}

// ── #268: one quote-trust budget, two windows ───────────────────────────

/// #268, item 3: the judgement window (30 s) and the pricing window (60 s)
/// are one concept at two budgets, so the band where a stop could still act
/// while the profit side was already blind is stated rather than implied.
#[test]
fn the_two_windows_are_one_quote_trust_budget_at_two_depths() {
    let cfg = ExitConfig::default();
    assert_eq!(cfg.max_last_price_age_sec, QUOTE_TRUST_SEC);
    assert_eq!(cfg.max_book_age_sec, BOOK_TRUST_SEC);
    assert_eq!(
        BOOK_TRUST_SEC,
        QUOTE_TRUST_SEC * 2,
        "a book carries real levels a remembered number has nobody behind, \
         so it may be read for twice the budget — derived, never restated"
    );
}

/// #268, item 1: `timestamp <= 0` is "this record carries no clock", which
/// is a different statement from "age zero" and takes the other branch. The
/// choice — an unknown age counts as FRESH — is asserted here so nobody has
/// to infer it from an `||`.
#[test]
fn an_unknown_book_age_is_fresh_by_choice_and_a_far_expired_one_is_not() {
    let cfg = ExitConfig::default();
    let now = 1_000_000;

    let unknown = one_sided_book(dec!(0.90)); // timestamp 0
    assert_eq!(
        book_age_ms(&unknown, now),
        None,
        "no clock on the record is not age zero"
    );
    assert!(
        book_is_fresh(&unknown, now, &cfg),
        "an unknown age counts as fresh ON PURPOSE: refusing to price every \
         clockless snapshot would make 'no timestamp' mean 'can never exit'"
    );

    let expired = OrderbookSnapshot::from_levels(
        "tok",
        vec![],
        vec![(dec!(0.90), dec!(100))],
        now - 10 * 60_000,
    );
    assert_eq!(book_age_ms(&expired, now), Some(600_000));
    assert!(
        !book_is_fresh(&expired, now, &cfg),
        "ten minutes old is not a quote standing here now"
    );

    // The budget is the only difference between the two verdicts: the same
    // book exactly one budget old is still inside it.
    let inside = OrderbookSnapshot::from_levels(
        "tok",
        vec![],
        vec![(dec!(0.90), dec!(100))],
        now - cfg.max_book_age_sec * 1_000,
    );
    assert!(book_is_fresh(&inside, now, &cfg));
}
