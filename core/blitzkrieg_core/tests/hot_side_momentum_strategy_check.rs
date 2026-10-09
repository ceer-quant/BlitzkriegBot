//! hot_side_momentum acceptance checks — the REAL package
//! (`user_layer/strategies_lua/hot_side_momentum`) driven through the engine
//! adapter, exactly the surface the kernel runs.
//!
//! The spec's acceptance rows, mapped:
//!   positive  — round 40%-70% through + spot momentum confirming the leader
//!               + leader ask in [0.60, 0.80] → ONE suggestion for the
//!               leading side at its own ask;
//!   negative  — outside the progress window (both ends), sub-threshold
//!               momentum, momentum CONTRADICTING the leader, equal asks (no
//!               leader), leader outside the band (the spec's 0.55/0.85
//!               reverse rows included), a stale trigger, a missing leg
//!               book;
//!   boundary  — exactly +0.10% triggers (scaled-integer gate); exactly 40%
//!               and 70% through fire (inclusive window ends); asks exactly
//!               0.60 and 0.80 fire (inclusive band);
//!   latch     — one attempt per condition per round; a new round slot
//!               retires the latch;
//!   seal      — a FIRING evaluation still emits no exit and no break: the
//!               position rides to settlement and the winner redeems $1.00.
//!
//! The adapter runs FEELESS on purpose: this strategy prices no fee (the
//! kernel charges the entry fee on the fill and the replay measures net) —
//! a firing test without `bk.fees()` in force documents that the trigger
//! genuinely does not depend on it.

use std::path::PathBuf;

use blitzkrieg_core::kline::Kline;
use blitzkrieg_core::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
use blitzkrieg_core::strategies::{EngineStrategy, StrategyCtx};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_market_api::kline::KlineInterval;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const UP: &str = "tok-up";
const DOWN: &str = "tok-down";
const COND: &str = "cond-1";
const SLOT: i64 = 1234;
/// A 15s window at 1s cadence: 15 closes, the last closing here. The
/// coverage gate needs span >= window - 1s = 14000ms — 14 gaps of 1s is
/// exactly the minimum.
const TRIG_MS: i64 = NOW + 14 * 1000 + 999;

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/hot_side_momentum")
}

fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("hot_side_momentum package must load");
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
}

fn market() -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: COND.into(),
        question_id: "q".into(),
        up_token_id: UP.into(),
        down_token_id: DOWN.into(),
        up_price: dec!(0.65),
        down_price: dec!(0.30),
        expires_at_ms: NOW + 900_000,
        round_slot: SLOT,
        round_duration_sec: 900,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

fn leg_book(token: &str, ask: Decimal, ts: i64) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        token.to_string(),
        vec![(ask - dec!(0.01), dec!(600))],
        vec![(ask, dec!(600))],
        ts,
    )
}

fn ctx<'a>(
    markets: &'a [CryptoMarket],
    slot: i64,
    time_left_sec: i64,
    now_ms: i64,
    fresh: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> StrategyCtx<'a> {
    StrategyCtx::new(markets, slot, time_left_sec, now_ms, fresh)
}

fn candidates_of(
    adapter: &mut LuaEngineAdapter,
    markets: &[CryptoMarket],
    slot: i64,
    time_left_sec: i64,
    now_ms: i64,
    fresh: &dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> Vec<blitzkrieg_core::signal::TradeSignal> {
    adapter.find_candidates(&ctx(markets, slot, time_left_sec, now_ms, fresh))
}

/// CLOSED sec1 bars 1s apart — 14 flats then the move, so the window has
/// real coverage (span exactly 14000ms) and the move is oldest → latest.
fn feed_momentum(adapter: &mut LuaEngineAdapter, final_close: Decimal) {
    let mut closes = vec![dec!(60000); 14];
    closes.push(final_close);
    for (i, price) in closes.iter().enumerate() {
        let open = NOW + (i as i64) * 1000;
        let k = Kline {
            symbol: "BTC".into(),
            interval: KlineInterval::Sec1,
            open_time_ms: open,
            close_time_ms: open + 999,
            open: *price,
            high: *price,
            low: *price,
            close: *price,
            volume: dec!(1),
            trade_count: 1,
            is_closed: true,
        };
        adapter.on_kline(&k);
    }
}

/// Exactly +0.10% inside the 15s window (the threshold's inclusive boundary).
fn pump() -> Decimal {
    dec!(60060)
}
/// Exactly −0.10%.
fn dump() -> Decimal {
    dec!(59940)
}

/// Both legs' books: UP at `up_ask`, DOWN at `down_ask`, fresh at TRIG_MS.
fn pair_fresh(up_ask: Decimal, down_ask: Decimal) -> impl Fn(&str) -> Option<OrderbookSnapshot> {
    move |t: &str| {
        if t == UP {
            Some(leg_book(t, up_ask, TRIG_MS))
        } else if t == DOWN {
            Some(leg_book(t, down_ask, TRIG_MS))
        } else {
            None
        }
    }
}

// ── positive ────────────────────────────────────────────────────────────────

/// Mid-window (50% through), spot +0.10% confirms the UP leader at 0.65 →
/// ONE suggestion, the UP side, at its own ask.
#[test]
fn confirmed_up_leader_is_bought_at_its_ask() {
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let fresh = pair_fresh(dec!(0.65), dec!(0.30));
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);

    assert_eq!(got.len(), 1, "one suggestion, got {got:?}");
    assert_eq!(got[0].token_id, UP);
    assert_eq!(got[0].direction, SignalDirection::Up);
    assert_eq!(got[0].price, dec!(0.65), "entry is the leader's ask");
    assert_eq!(got[0].shares, None, "sizing belongs to the kernel");
    assert!(
        got[0].reason.contains("hot UP") && got[0].reason.contains("spot +0.10%"),
        "reason should name side and move: {}",
        got[0].reason
    );
}

/// Spot −0.10% confirms the DOWN leader at 0.65 → the DOWN side.
#[test]
fn confirmed_down_leader_is_bought_at_its_ask() {
    let mut a = adapter();
    feed_momentum(&mut a, dump());
    let fresh = pair_fresh(dec!(0.30), dec!(0.65));
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);

    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].token_id, DOWN);
    assert_eq!(got[0].direction, SignalDirection::Down);
    assert_eq!(got[0].price, dec!(0.65));
}

// ── progress window (exact integer boundaries, inclusive) ───────────────────

/// Exactly 40% through (time_left 540 of 900) fires; one second earlier is
/// outside the window and refuses.
#[test]
fn progress_window_starts_inclusively_at_forty_percent() {
    let markets = vec![market()];
    let fresh = pair_fresh(dec!(0.65), dec!(0.30));

    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let got = candidates_of(&mut a, &markets, SLOT, 540, TRIG_MS + 500, &fresh);
    assert_eq!(got.len(), 1, "exactly 40% through must fire");

    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let got = candidates_of(&mut a, &markets, SLOT, 541, TRIG_MS + 500, &fresh);
    assert!(got.is_empty(), "before 40% must refuse: {got:?}");
}

/// Exactly 70% through (time_left 270) fires; one second later refuses.
#[test]
fn progress_window_ends_inclusively_at_seventy_percent() {
    let markets = vec![market()];
    let fresh = pair_fresh(dec!(0.65), dec!(0.30));

    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let got = candidates_of(&mut a, &markets, SLOT, 270, TRIG_MS + 500, &fresh);
    assert_eq!(got.len(), 1, "exactly 70% through must fire");

    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let got = candidates_of(&mut a, &markets, SLOT, 269, TRIG_MS + 500, &fresh);
    assert!(got.is_empty(), "past 70% must refuse: {got:?}");
}

// ── negative (each row must refuse) ─────────────────────────────────────────

/// +0.0983% is under the 0.10% threshold → no trigger → no trade.
#[test]
fn refuses_a_sub_threshold_move() {
    let mut a = adapter();
    feed_momentum(&mut a, dec!(60059));
    let fresh = pair_fresh(dec!(0.65), dec!(0.30));
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);
    assert!(got.is_empty(), "sub-threshold must refuse: {got:?}");
}

/// The spec's contradiction row: spot rises but the DOWN side leads — a
/// move against the leader is not a trade.
#[test]
fn refuses_momentum_that_contradicts_the_leader() {
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    // DOWN leads (0.65) while spot pumps.
    let fresh = pair_fresh(dec!(0.30), dec!(0.65));
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);
    assert!(
        got.is_empty(),
        "contradicting momentum must refuse: {got:?}"
    );
}

/// Equal asks name no leader.
#[test]
fn refuses_equal_asks() {
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let fresh = pair_fresh(dec!(0.60), dec!(0.60));
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);
    assert!(got.is_empty(), "no leader, no trade: {got:?}");
}

/// The spec's reverse rows: a leader at 0.55 or 0.85 is outside the band —
/// no trigger.
#[test]
fn refuses_a_leader_outside_the_price_band() {
    let markets = vec![market()];
    // 0.55 leader (below 0.60).
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let fresh = pair_fresh(dec!(0.55), dec!(0.40));
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);
    assert!(got.is_empty(), "0.55 leader must refuse: {got:?}");

    // 0.85 leader (above 0.80).
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let fresh = pair_fresh(dec!(0.85), dec!(0.10));
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);
    assert!(got.is_empty(), "0.85 leader must refuse: {got:?}");
}

/// A stale trigger describes a market that already repriced: past
/// `mom_max_age_ms` (2000) nothing fires.
#[test]
fn refuses_a_stale_trigger() {
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let fresh = pair_fresh(dec!(0.65), dec!(0.30));
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 2500, &fresh);
    assert!(got.is_empty(), "stale trigger must refuse: {got:?}");
}

/// A missing leg book is an outage, not a signal.
#[test]
fn refuses_a_missing_leg_book() {
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let markets = vec![market()];
    let up_only = |t: &str| (t == UP).then(|| leg_book(t, dec!(0.65), TRIG_MS));
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &up_only);
    assert!(got.is_empty(), "one-sided book must refuse: {got:?}");
}

// ── band boundaries (inclusive) ─────────────────────────────────────────────

/// Asks exactly 0.60 and 0.80 are inside the band.
#[test]
fn band_edges_are_inclusive() {
    let markets = vec![market()];
    for leader_ask in [dec!(0.60), dec!(0.80)] {
        let mut a = adapter();
        feed_momentum(&mut a, pump());
        let follower = Decimal::ONE - leader_ask - dec!(0.05);
        let fresh = pair_fresh(leader_ask, follower);
        let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);
        assert_eq!(
            got.len(),
            1,
            "leader at {leader_ask} is the inclusive band edge: {got:?}"
        );
        assert_eq!(got[0].price, leader_ask);
    }
}

// ── latch ───────────────────────────────────────────────────────────────────

/// One attempt per condition per round: a second evaluation in the same
/// round emits nothing new.
#[test]
fn fires_once_per_condition_per_round() {
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let fresh = pair_fresh(dec!(0.65), dec!(0.30));
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);
    assert_eq!(got.len(), 1);
    let got2 = candidates_of(&mut a, &markets, SLOT, 440, TRIG_MS + 600, &fresh);
    assert!(
        got2.is_empty(),
        "the latch must hold within a round: {got2:?}"
    );
}

/// A new round slot retires the latch: the same setup fires again next round.
#[test]
fn a_new_round_retires_the_latch() {
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let fresh = pair_fresh(dec!(0.65), dec!(0.30));
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);
    assert_eq!(got.len(), 1);
    let got2 = candidates_of(&mut a, &markets, SLOT + 1, 450, TRIG_MS + 500, &fresh);
    assert_eq!(got2.len(), 1, "next round is a fresh decision: {got2:?}");
}

// ── seal ────────────────────────────────────────────────────────────────────

/// Even a FIRING evaluation emits no exit and no break: hold-to-settlement
/// is the whole exit plan — the winner redeems $1.00 at resolution.
#[test]
fn the_seal_no_exits_no_breaks_even_while_firing() {
    let mut a = adapter();
    feed_momentum(&mut a, pump());
    let fresh = pair_fresh(dec!(0.65), dec!(0.30));
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, SLOT, 450, TRIG_MS + 500, &fresh);
    assert_eq!(got.len(), 1, "precondition: the strategy fired");
    assert!(
        a.take_exit_intents().is_empty(),
        "hold-to-settlement never exits"
    );
    assert!(
        a.take_breaks().is_empty(),
        "no resting order exists to break"
    );
}
