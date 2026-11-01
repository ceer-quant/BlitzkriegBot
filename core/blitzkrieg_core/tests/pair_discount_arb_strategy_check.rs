//! pair_discount_arb acceptance checks — the REAL package
//! (`user_layer/strategies_lua/pair_discount_arb`) driven through the engine
//! adapter, exactly the surface the kernel runs.
//!
//! The spec's acceptance rows, mapped:
//!   positive  — ask_UP + ask_DOWN + entry fees < max_pair_cost → ONE
//!               suggestion per leg at the v1.3 fill-path quote (each one tick
//!               UNDER its own ask — the RESTABLE price; an at-ask post-only
//!               POST is refused, #464), EQUAL declared share counts (a pair
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

/// Adapter whose `bk.params()` start from a modified default bag (the
/// manifest-defaults path, overridden the way a param registry would — the
/// market_maker check's pattern).
fn adapter_with_params(override_key: &str, override_value: &str) -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("pair_discount_arb package must load");
    let mut tunables = loaded.tunables;
    tunables.insert(override_key.to_string(), override_value.to_string());
    let a = LuaEngineAdapter::new(loaded.strategy, tunables)
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

/// 0.48 + 0.45 + fees (0.00778752 + 0.00765703) = 0.94544455 < max_pair_cost
/// (default 0.985) → BOTH legs suggested at the v1.3 fill-path quote (mode 1,
/// the default: each one tick UNDER its OWN ask — the RESTABLE price; the
/// at-ask quote is the ONE price a post-only BUY can never rest at and the
/// kernel refuses its POST, #464), EQUAL declared share counts. The DISCOUNT
/// TEST stays on the asks: the reason still names the ask-based all-in cost,
/// and a maker fill only ever prints at or below its quote.
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
    assert_eq!(
        up.price,
        dec!(0.47),
        "each leg quotes one tick UNDER its OWN ask (v1.3 mode 1: the restable \
         price — at-ask is refused #464)"
    );
    assert_eq!(down.price, dec!(0.44));
    assert_eq!(
        up.shares, down.shares,
        "the legs must match in share count — a pair merges per share-pair"
    );
    assert_eq!(up.shares, Some(dec!(600)), "min(ask depths), grid-floored");
    assert!(up.condition_id == down.condition_id && up.condition_id == COND);
    assert!(
        up.reason.contains("pair 0.9454"),
        "the discount test stays on the asks — the reason names the all-in \
         cost: {}",
        up.reason
    );
    assert!(
        up.reason.contains("post-only@ask-1t"),
        "the reason names the v1.3 fill path: {}",
        up.reason
    );
}

/// The declared mode-0 arm (v1.2's at-ask baseline, kept as the replay
/// control) still emits AT the asks — the ONE price a post-only BUY can never
/// rest at, which the kernel refuses at POST (#464 `WouldCross`, pinned at
/// the OME level in
/// `pair_entry_maker_tests::the_at_ask_baseline_still_refuses_the_post_and_never_chases`).
/// Locked here so the under-ask default can never silently drift back onto
/// the unpostable price.
#[test]
fn the_mode_0_baseline_quotes_at_the_ask_the_kernel_refuses() {
    let mut a = adapter_with_params("entry_quote_mode", "0");
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "the v1.2 arm still emits both legs");
    let up = got.iter().find(|s| s.token_id == UP).unwrap();
    let down = got.iter().find(|s| s.token_id == DOWN).unwrap();
    assert_eq!(
        up.price,
        dec!(0.48),
        "mode 0 quotes AT its own ask — the unpostable price"
    );
    assert_eq!(down.price, dec!(0.45), "join, not chase (the v1.2 form)");
    assert!(
        up.reason.contains("post-only@ask,"),
        "mode 0's fill-path label is the plain at-ask form: {}",
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

// ── entry window (v1.2) ─────────────────────────────────────────────────────

/// `entry_window = 0.5` on the declared 900s family: a NEW pair attempt needs
/// at least 450s still to run (the boundary instant is INCLUSIVE — `>=`).
/// T-451 and T-450 fire; T-449 and T-100 refuse. The window gates ATTEMPTS
/// only — with nothing armed there is nothing to collect, but an ARMED
/// condition keeps collecting past the window (next row).
#[test]
fn the_entry_window_confines_new_attempts_to_the_front_half() {
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];

    let mut a = adapter_with_params("entry_window", "0.5");
    let got = candidates_of(&mut a, &markets, 451, NOW, &fresh);
    assert_eq!(got.len(), 2, "T-451 is inside the front half: {got:?}");

    let mut a = adapter_with_params("entry_window", "0.5");
    let got = candidates_of(&mut a, &markets, 450, NOW, &fresh);
    assert_eq!(got.len(), 2, "T-450 (the boundary) is inclusive: {got:?}");

    let mut a = adapter_with_params("entry_window", "0.5");
    let got = candidates_of(&mut a, &markets, 449, NOW, &fresh);
    assert!(got.is_empty(), "T-449 is the back half: {got:?}");

    let mut a = adapter_with_params("entry_window", "0.5");
    let got = candidates_of(&mut a, &markets, 100, NOW, &fresh);
    assert!(got.is_empty(), "T-100 in the back half: {got:?}");
    assert!(
        drain_exits(&mut a).is_empty(),
        "nothing armed, nothing collects"
    );
}

/// The window gates ATTEMPTS only: armed inside the window, the condition
/// keeps merging (and gap-fills) deep into the back half — collection runs to
/// the last tick.
#[test]
fn the_entry_window_does_not_stop_collection() {
    let mut a = adapter_with_params("entry_window", "0.5");
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 600, NOW, &fresh);
    assert_eq!(got.len(), 2, "armed inside the window");
    drain_exits(&mut a);

    // T-100 — far inside the back half — still emits the merge collection.
    let got2 = candidates_of(&mut a, &markets, 100, NOW + 1_000, &fresh);
    assert!(got2.is_empty(), "no NEW attempt in the back half: {got2:?}");
    let exits = drain_exits(&mut a);
    assert_eq!(exits.len(), 2, "collection runs past the window");
    assert!(exits.iter().all(|e| e.reason == "merge"));
}

/// `entry_window = 0` disables the window: the whole round enters — the v1.1
/// posture, byte-identical where the old tests sit (T-10, T-500, T-600…).
#[test]
fn entry_window_zero_restores_the_whole_round() {
    let mut a = adapter_with_params("entry_window", "0");
    let fresh = pair_fresh(
        Some((dec!(0.48), dec!(600))),
        Some((dec!(0.45), dec!(600))),
        NOW,
    );
    let markets = vec![market()];
    let got = candidates_of(&mut a, &markets, 100, NOW, &fresh);
    assert_eq!(
        got.len(),
        2,
        "window 0: T-100 (back half of 900) still fires"
    );
}

// ── gap-fill ────────────────────────────────────────────────────────────────

/// A partially-filled leg is topped up to its counterpart by the SAME v1.3
/// fill path: arms a pair, then re-evaluates with a holdings slice showing the
/// UP leg fully held (3.0) and the DOWN leg missing — the gap-fill branch must
/// emit ONE top-up entry on the DOWN leg one tick UNDER the CURRENT ask (the
/// restable quote; an at-ask top-up is refused at POST exactly like an at-ask
/// entry, #464), tagged with the shortfall reason. The incremental TEST stays
/// on the ask (ask + fee < max_top_up_cost); the collection merge intents
/// still fire.
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
        account_id: blitzkrieg_market_api::DEFAULT_ACCOUNT_ID.to_string(),
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
    assert_eq!(
        sig.price,
        dec!(0.44),
        "the top-up quotes the CURRENT ask − 1t (v1.3 mode 1: the restable \
         price — an at-ask top-up POST is refused #464)"
    );
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
