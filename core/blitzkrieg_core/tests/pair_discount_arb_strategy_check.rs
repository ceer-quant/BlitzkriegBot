//! pair_discount_arb acceptance checks — the REAL package
//! (`user_layer/strategies_lua/pair_discount_arb`) driven through the engine
//! adapter, exactly the surface the kernel runs.
//!
//! The spec's acceptance rows, mapped:
//!   positive  — ask_UP + ask_DOWN + entry fees < 0.995 → ONE suggestion per
//!               leg at its own ask, EQUAL declared share counts (a pair
//!               merges per share-pair); the armed condition then emits
//!               `reason = "merge"` collection intents on later evaluations;
//!   negative  — pair sum above the cost cap (the spec's "> 1.005 never
//!               fires" row included), just-above-cap sums, no fee schedule
//!               in force (fail-closed: the fee is a cost parameter, never
//!               assumed zero), a stale/missing leg book, sub-floor time
//!               left (entries only — an armed condition keeps collecting);
//!   boundary  — asymmetric depth sizes the pair to the SMALLER leg;
//!   seal      — every emitted exit is a "merge" collection, never a sell,
//!               and `breaks` is always empty.
//!
//! The adapter is constructed the way the service does it at load:
//! manifest declarations stamped (`declare`) and the kernel's fee schedule
//! injected (`set_fee_schedule`) — here the legacy_quadratic curve the
//! deployment line charges.

use std::path::PathBuf;

use blitzkrieg_core::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
use blitzkrieg_core::strategies::{EngineStrategy, HeldPosition, StrategyCtx, StrategyExitIntent};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_lua_runtime::FeeScheduleView;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const UP: &str = "tok-up";
const DOWN: &str = "tok-down";
const COND: &str = "cond-1";

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/pair_discount_arb")
}

/// The kernel's ONE fee schedule, as the service stamps it at load
/// (legacy_quadratic: 0.125*(p*(1-p))^2 USD/share).
fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("pair_discount_arb package must load");
    let a = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions);
    a.set_fee_schedule(FeeScheduleView {
        name: "legacy_quadratic".into(),
        rate: "0.125".into(),
        exponent: 2,
    });
    a
}

/// The same adapter WITHOUT a fee schedule — `bk.fees()` answers nil and the
/// strategy must refuse to price anything.
fn adapter_feeless() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("pair_discount_arb package must load");
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
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: NOW + 300_000,
        round_slot: NOW / 300_000,
        round_duration_sec: 300,
        archive_verdict: false,
        venue: String::new(),
        neg_risk: false,
        question: "BTC up or down".into(),
    }
}

fn leg_book(token: &str, ask: Decimal, depth: Decimal, ts: i64) -> OrderbookSnapshot {
    OrderbookSnapshot::from_levels(
        token.to_string(),
        vec![(ask - dec!(0.01), depth)],
        vec![(ask, depth)],
        ts,
    )
}

fn ctx<'a>(
    markets: &'a [CryptoMarket],
    time_left_sec: i64,
    now_ms: i64,
    fresh: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> StrategyCtx<'a> {
    StrategyCtx::new(markets, NOW / 300_000, time_left_sec, now_ms, fresh)
}

fn candidates_of(
    adapter: &mut LuaEngineAdapter,
    markets: &[CryptoMarket],
    time_left_sec: i64,
    now_ms: i64,
    fresh: &dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> Vec<blitzkrieg_core::signal::TradeSignal> {
    adapter.find_candidates(&ctx(markets, time_left_sec, now_ms, fresh))
}

fn drain_exits(adapter: &mut LuaEngineAdapter) -> Vec<StrategyExitIntent> {
    adapter.take_exit_intents()
}

/// Both legs fresh at the given asks; a `None` ask hides that leg's book.
fn pair_fresh(
    up_ask: Option<(Decimal, Decimal)>,
    down_ask: Option<(Decimal, Decimal)>,
    ts: i64,
) -> impl Fn(&str) -> Option<OrderbookSnapshot> {
    move |t: &str| {
        let (ask, depth) = match (t, up_ask, down_ask) {
            (x, Some((a, d)), _) if x == UP => (a, d),
            (x, _, Some((a, d))) if x == DOWN => (a, d),
            _ => return None,
        };
        Some(leg_book(t, ask, depth, ts))
    }
}

// ── positive ────────────────────────────────────────────────────────────────

/// 0.48 + 0.45 + fees (0.00778752 + 0.00765703) = 0.94544455 < 0.995 → BOTH
/// legs suggested, each at its own ask, EQUAL declared share counts.
#[test]
fn a_discounted_pair_suggests_both_legs_at_their_asks() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);

    assert_eq!(got.len(), 2, "one suggestion per leg, got {got:?}");
    let up = got.iter().find(|s| s.token_id == UP).unwrap();
    let down = got.iter().find(|s| s.token_id == DOWN).unwrap();
    assert_eq!(up.direction, SignalDirection::Up);
    assert_eq!(down.direction, SignalDirection::Down);
    assert_eq!(up.price, dec!(0.48), "each leg prices at its OWN ask");
    assert_eq!(down.price, dec!(0.45));
    assert_eq!(
        up.shares, down.shares,
        "the legs must match in share count — a pair merges per share-pair"
    );
    assert_eq!(up.shares, Some(dec!(600)), "min(ask depths), grid-floored");
    assert!(up.condition_id == down.condition_id && up.condition_id == COND);
    assert!(
        up.reason.contains("pair 0.9454"),
        "reason should name the all-in cost: {}",
        up.reason
    );
}

/// Asymmetric depth: the pair is sized to the SMALLER leg's ask depth —
/// unpaired shares have no collateral claim.
#[test]
fn pair_size_floors_to_the_thinner_leg() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(250))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2);
    for s in &got {
        assert_eq!(s.shares, Some(dec!(250)), "sized to the thinner leg");
    }
}

/// After the pair attempt is armed, later evaluations emit `reason = "merge"`
/// collection intents for BOTH legs — the kernel's pre-ladder intercept that
/// pays $1.00 per complete pair.
#[test]
fn an_armed_pair_emits_merge_collection_intents() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2);
    assert!(
        drain_exits(&mut a).is_empty(),
        "arming pass collects nothing yet"
    );

    // A later evaluation — same round, entries would re-fire if the latch
    // leaked; collection fires instead.
    let got2 = candidates_of(&mut a, &markets, 500, NOW + 1_000, &fresh);
    assert!(
        got2.is_empty(),
        "one pair ATTEMPT per condition per round: {got2:?}"
    );
    let exits = drain_exits(&mut a);
    assert_eq!(exits.len(), 2, "one merge intent per leg, got {exits:?}");
    let tokens: Vec<&str> = exits.iter().map(|e| e.token_id.as_str()).collect();
    assert!(tokens.contains(&UP) && tokens.contains(&DOWN));
    assert!(
        exits.iter().all(|e| e.reason == "merge"),
        "collection is tagged merge: {exits:?}"
    );
}

// ── negative (each row must refuse) ─────────────────────────────────────────

/// The spec's reverse row: a pair summing ABOVE the money (0.56 + 0.50 = 1.06
/// + fees ≈ 1.076) is not a discount — no entries, no arming, no collection.
#[test]
fn refuses_a_pair_summing_above_the_money() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.56), dec!(600))),
        Some((dec!(0.50), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert!(got.is_empty(), "sum > 1.005 must never fire: {got:?}");
    assert!(drain_exits(&mut a).is_empty(), "nothing was armed");
}

/// Just above the cap: 0.50 + 0.48 + fees = 0.99560002 > 0.995 — a discount
/// thinner than the cap is still refused (fees are part of the decision).
#[test]
fn refuses_a_pair_just_above_the_cost_cap() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.50), dec!(600))),
        Some((dec!(0.48), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert!(got.is_empty(), "0.9956 > 0.995 must refuse: {got:?}");
}

/// Fail-closed on fees: no schedule in force (`bk.fees()` nil) → NO entries.
/// The fee is a cost parameter of the same order as the edge; guessing zero
/// would fabricate profit.
#[test]
fn refuses_to_price_without_a_fee_schedule() {
    let mut a = adapter_feeless();
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert!(got.is_empty(), "no schedule → no pricing: {got:?}");
    assert!(drain_exits(&mut a).is_empty());
}

/// A stale or missing leg book is an outage, not a discount: no entries.
#[test]
fn refuses_a_stale_or_missing_leg_book() {
    let mut a = adapter();
    // UP fresh, DOWN absent.
    let fresh = pair_fresh(Some((dec!(0.48), dec!(600))), None, NOW);
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert!(got.is_empty(), "one-sided book → no pair: {got:?}");

    // DOWN fresh, UP absent.
    let fresh = pair_fresh(None, Some((dec!(0.45), dec!(600))), NOW);
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert!(got.is_empty(), "one-sided book → no pair: {got:?}");
}

/// Below the entry time floor no NEW pair is attempted — but an ALREADY
/// armed condition keeps collecting to the last tick (the collection must
/// run; only entries are gated).
#[test]
fn entry_floor_gates_entries_but_not_collection() {
    let mut a = adapter();
    let markets = vec![market()];
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    // Arm inside the window first.
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2);
    drain_exits(&mut a);

    // Past the floor: no new entries, collection still fires.
    let got2 = candidates_of(&mut a, &markets, 10, NOW + 1_000, &fresh);
    assert!(got2.is_empty(), "no entries below the time floor: {got2:?}");
    let exits = drain_exits(&mut a);
    assert_eq!(exits.len(), 2, "collection runs to the last tick");
    assert!(exits.iter().all(|e| e.reason == "merge"));
}

// ── seal ────────────────────────────────────────────────────────────────────

/// Even a FIRING evaluation expresses no sell and no break: every exit the
/// strategy ever emits is a "merge" collection tag, and `breaks` stays empty
/// (the kernel owns the ladder; the strategy owns the collection request).
#[test]
fn the_seal_no_sells_no_breaks_even_while_firing() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let _ = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    let exits = drain_exits(&mut a);
    assert!(
        exits.iter().all(|e| e.reason == "merge"),
        "exits are collections, never sells: {exits:?}"
    );
    assert!(
        a.take_breaks().is_empty(),
        "no resting order exists to break"
    );
}

// ── gap-fill ────────────────────────────────────────────────────────────────

/// A partially-filled leg is topped up to its counterpart at the CURRENT ask:
/// arms a pair, then re-evaluates with a holdings slice showing the UP leg
/// fully held (3.0) and the DOWN leg missing — the gap-fill branch must emit
/// ONE top-up entry on the DOWN leg at its own ask (ask + fee < $1), tagged
/// with the shortfall reason. The collection merge intents still fire.
#[test]
fn a_partially_filled_leg_is_topped_up_to_its_counterpart() {
    let mut a = adapter();
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    // Pass 1: arm the pair (holdings empty).
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "arming pass emits both legs");
    // Pass 2: the UP leg filled, the DOWN leg did not.
    let held = vec![HeldPosition {
        strategy: "pair_discount_arb".into(),
        condition_id: COND.into(),
        token_id: UP.into(),
        direction: "up".into(),
        shares: "3.0".into(),
        entry_price: "0.48".into(),
        round_slot: NOW / 300_000,
    }];
    let ctx2 =
        StrategyCtx::new(&markets, NOW / 300_000, 500, NOW + 1_000, &fresh).with_positions(&held);
    let got2 = a.find_candidates(&ctx2);
    assert_eq!(got2.len(), 1, "one top-up for the short leg, got {got2:?}");
    let sig = &got2[0];
    assert_eq!(sig.token_id, DOWN, "the SHORT leg is topped up");
    assert_eq!(sig.price, dec!(0.45), "top-up prices the CURRENT ask");
    assert_eq!(
        sig.shares,
        Some(dec!(3.00)),
        "top-up covers the full shortfall"
    );
    assert!(
        sig.reason.contains("gap-fill"),
        "reason names the gap: {}",
        sig.reason
    );
    // Collection still fires for the complete pairs already held.
    let exits = drain_exits(&mut a);
    assert_eq!(exits.len(), 2, "merge intents per leg, got {exits:?}");
}
