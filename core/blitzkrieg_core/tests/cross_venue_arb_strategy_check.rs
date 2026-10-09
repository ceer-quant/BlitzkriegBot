//! cross_venue_arb acceptance checks — the REAL package
//! (`user_layer/strategies_lua/cross_venue_arb`) driven through the engine
//! adapter, exactly the surface the kernel runs. Issue #427's blueprint
//! acceptance, mapped:
//!
//!   pairing   — only a `bk.unified_events()` mapping with status "paired"
//!               and ZERO discrepancies is priced; a discrepancy-marked or
//!               single-leg event is a PseudoHedge and never fires;
//!   positive  — all-in pair cost ≤ 1 − spread_open_pct → BOTH legs
//!               suggested, each at its own ask, EQUAL declared share counts,
//!               capacity = min(two legs' ask depths) (issue #427: "容量 =
//!               两平台各自深度的最小值");
//!   cross-leg — the UP leg comes from the listing with the cheaper UP ask
//!               (venue is data: cheapest listing wins the leg), the DOWN leg
//!               from the other — the two legs are NOT from the same listing;
//!   fees      — BOTH legs' taker fees are charged inside the decision; no
//!               fee schedule in force → no suggestions at all (fail closed);
//!   negative  — pair cost inside the spread, depth below the floor, one
//!               leg's book stale → refuse;
//!   reverse   — a hypothetical adapter that ignored one leg's depth would
//!               size past the thinner leg: asserted here by construction
//!               (the declared shares are ALWAYS ≤ both legs' depths — if
//!               the package ever emitted a size above either leg's depth
//!               this row goes red);
//!   release   — a held pair whose all-in cost falls back within
//!               spread_close_pct emits exit intents for both legs
//!               ("回落 < Y% 平仓"), and exits are { token, reason } only
//!               (the seal: no reserved keys, `breaks` always empty).
//!
//! The adapter is constructed the way the service does it at load:
//! gate declarations stamped and the kernel's fee schedule injected — here
//! the legacy_quadratic curve the deployment line charges.

use std::path::PathBuf;

use blitzkrieg_core::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
use blitzkrieg_core::strategies::{EngineStrategy, StrategyCtx, StrategyExitIntent};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_lua_runtime::FeeScheduleView;
use blitzkrieg_market_api::unified::{
    DiscrepancyKind, EventStatus, ListingStatus, PlatformListing, SettlementSource, UnifiedEvent,
    Venue,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const SLOT: i64 = NOW / 300_000;

// Two listings of the SAME bet: the polymarket row quotes the cheaper UP
// ask, the kalshi row the cheaper DOWN ask. The pair therefore legs UP@pm
// and DOWN@kx — deliberately NOT one venue's listing, so a package that
// traded one venue's own UP+DOWN pair cannot pass the cross-leg row.
const PM: &str = "pm-cond";
const KX: &str = "kx-cond";
const PM_UP: &str = "pm-up";
const PM_DOWN: &str = "pm-down";
const KX_UP: &str = "kx-up";
const KX_DOWN: &str = "kx-down";

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb")
}

fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("cross_venue_arb package must load");
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
    let loaded = load_lua_package(&package_dir()).expect("cross_venue_arb package must load");
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
}

fn listing(venue: Venue, cond: &str, up: &str, down: &str) -> PlatformListing {
    PlatformListing {
        venue,
        condition_id: cond.into(),
        up_token_id: up.into(),
        down_token_id: down.into(),
        asset: Some("bitcoin".into()),
        title: "Bitcoin Up or Down".into(),
        settlement_rules: "Binance 1m candle".into(),
        settlement_source: SettlementSource::Chainlink,
        expires_at_ms: NOW + 300_000,
        status: ListingStatus::Active,
    }
}

/// The confirmed mapping, defaulting to the PM/KX token ids above.
fn paired_event() -> UnifiedEvent {
    let mut e = UnifiedEvent {
        id: "evt-1".into(),
        title: "Bitcoin Up or Down".into(),
        settlement_rules: "Binance 1m candle".into(),
        settlement_source: SettlementSource::Chainlink,
        listings: Default::default(),
        discrepancies: Vec::new(),
        status: EventStatus::Paired,
    };
    e.listings.insert(
        Venue::Polymarket,
        listing(Venue::Polymarket, PM, PM_UP, PM_DOWN),
    );
    e.listings
        .insert(Venue::Kalshi, listing(Venue::Kalshi, KX, KX_UP, KX_DOWN));
    e
}

fn market(token_up: &str, token_down: &str, cond: &str, venue: &str) -> CryptoMarket {
    CryptoMarket {
        asset: "BTC".into(),
        condition_id: cond.into(),
        question_id: "q".into(),
        up_token_id: token_up.into(),
        down_token_id: token_down.into(),
        up_price: dec!(0.48),
        down_price: dec!(0.45),
        expires_at_ms: NOW + 300_000,
        round_slot: SLOT,
        round_duration_sec: 300,
        archive_verdict: false,
        venue: venue.to_string(),
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
    events: &'a [UnifiedEvent],
    time_left_sec: i64,
    now_ms: i64,
    fresh: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> StrategyCtx<'a> {
    StrategyCtx::new(markets, SLOT, time_left_sec, now_ms, fresh).with_unified_events(events)
}

fn candidates_of(
    adapter: &mut LuaEngineAdapter,
    ctx: &StrategyCtx<'_>,
) -> Vec<blitzkrieg_core::signal::TradeSignal> {
    adapter.find_candidates(ctx)
}

fn drain_exits(adapter: &mut LuaEngineAdapter) -> Vec<StrategyExitIntent> {
    adapter.take_exit_intents()
}

/// The pair quotes: UP ask/depth per listing and DOWN ask/depth per listing.
/// `pm = (up_ask, up_depth, down_ask, down_depth)`, same for `kx`.
fn pair_books(
    pm: (Decimal, Decimal, Decimal, Decimal),
    kx: (Decimal, Decimal, Decimal, Decimal),
    ts: i64,
) -> impl Fn(&str) -> Option<OrderbookSnapshot> {
    move |t: &str| match t {
        x if x == PM_UP => Some(leg_book(t, pm.0, pm.1, ts)),
        x if x == PM_DOWN => Some(leg_book(t, pm.2, pm.3, ts)),
        x if x == KX_UP => Some(leg_book(t, kx.0, kx.1, ts)),
        x if x == KX_DOWN => Some(leg_book(t, kx.2, kx.3, ts)),
        _ => None,
    }
}

// ── positive ────────────────────────────────────────────────────────────────

/// pm: 0.44 up / 0.50 down; kx: 0.48 up / 0.42 down.
/// Cross-venue pair = UP@pm 0.44 + DOWN@kx 0.42 = 0.86 + both fees
/// (legacy_quadratic 0.125·(p(1−p))²: 0.00765703 + 0.00765703 ≈ 0.0153)
/// ≈ 0.8753 ≤ 0.98 → fires; one-vs-one listing pair would cost
/// 0.94 / 0.90 + fees and must NOT be the suggestion.
#[test]
fn a_discounted_cross_venue_pair_suggests_both_legs() {
    let mut a = adapter();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let events = vec![paired_event()];
    let got = candidates_of(&mut a, &ctx(&markets, &events, 600, NOW, &fresh));

    assert_eq!(got.len(), 2, "one suggestion per leg, got {got:?}");
    let up = got.iter().find(|s| s.token_id == PM_UP).unwrap();
    let down = got.iter().find(|s| s.token_id == KX_DOWN).unwrap();
    assert_eq!(up.direction, SignalDirection::Up);
    assert_eq!(down.direction, SignalDirection::Down);
    assert_eq!(up.price, dec!(0.44), "each leg prices at its OWN ask");
    assert_eq!(down.price, dec!(0.42));
    assert_eq!(up.shares, down.shares, "the legs must match in share count");
    assert_eq!(
        up.shares,
        Some(dec!(250)),
        "capacity = min(the two legs' ask depths): kx down depth 250 < pm up depth 600"
    );
    assert!(up.reason.contains("cap=min(depth)"), "{}", up.reason);
}

/// The deeper leg never inflates the size: declared shares stay ≤ BOTH
/// legs' visible depths. This is the reverse-acceptance row: a package (or
/// adapter) that ignored one venue's depth cap would size past the thinner
/// leg and this assertion goes red.
#[test]
fn capacity_never_exceeds_either_legs_depth() {
    let mut a = adapter();
    // pm up depth 5000, kx down depth 40 — capacity must floor to 40.
    let fresh = pair_books(
        (dec!(0.44), dec!(5000), dec!(0.50), dec!(5000)),
        (dec!(0.48), dec!(9000), dec!(0.42), dec!(40)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let events = vec![paired_event()];
    let got = candidates_of(&mut a, &ctx(&markets, &events, 600, NOW, &fresh));
    assert_eq!(got.len(), 2);
    for s in &got {
        let shares = s.shares.expect("declared shares");
        assert!(
            shares <= dec!(40),
            "capacity must be min(two depths) = 40, got {shares}"
        );
    }
}

// ── pairing discipline ──────────────────────────────────────────────────────

/// A settlement discrepancy mark (PseudoHedge per #425→#426) never trades.
#[test]
fn refuses_an_event_with_discrepancies() {
    let mut a = adapter();
    let mut ev = paired_event();
    ev.discrepancies = vec![DiscrepancyKind::SettlementSource];
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let got = candidates_of(&mut a, &ctx(&markets, &[ev], 600, NOW, &fresh));
    assert!(got.is_empty(), "discrepancy-marked event must not fire");
    assert!(drain_exits(&mut a).is_empty());
}

/// A degraded (single-leg) mapping never trades.
#[test]
fn refuses_a_single_leg_mapping() {
    let mut a = adapter();
    let mut ev = paired_event();
    ev.status = EventStatus::SingleLegAvailable;
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let got = candidates_of(&mut a, &ctx(&markets, &[ev], 600, NOW, &fresh));
    assert!(got.is_empty(), "single-leg mapping must not fire");
}

/// No unified mapping pushed (single-venue deployment) → nothing to price.
#[test]
fn refuses_without_a_unified_mapping() {
    let mut a = adapter();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let got = candidates_of(&mut a, &ctx(&markets, &[], 600, NOW, &fresh));
    assert!(got.is_empty());
}

// ── economics ───────────────────────────────────────────────────────────────

/// No fee schedule in force → no suggestions at all (the fee is a cost
/// parameter of the same order as the edge; guessing zero fabricates profit).
#[test]
fn refuses_without_a_fee_schedule() {
    let mut a = adapter_feeless();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let events = vec![paired_event()];
    let got = candidates_of(&mut a, &ctx(&markets, &events, 600, NOW, &fresh));
    assert!(got.is_empty(), "feeless deployment must fail closed");
}

/// The spread must beat `spread_open_pct` (default 2%): pair cost 0.9755
/// (0.49 + 0.47 + fees ≈ 0.0155) leaves an edge of 2.45%… wait, that fires.
/// This row pins the knife edge: cost 0.9855 leaves 1.45% < 2% → refuse.
#[test]
fn refuses_a_discount_thinner_than_the_open_spread() {
    let mut a = adapter();
    // 0.49 + 0.47 + fees(0.00765703 + 0.00765703) = 0.97531406 ≤ 0.98 fires;
    // move one ask up by a cent: 0.50 + 0.47 + fees = 0.98531406 > 0.98.
    let fresh = pair_books(
        (dec!(0.50), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.47), dec!(250)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let events = vec![paired_event()];
    let got = candidates_of(&mut a, &ctx(&markets, &events, 600, NOW, &fresh));
    assert!(
        got.is_empty(),
        "edge 1.47% < spread_open_pct 2% must not fire: {got:?}"
    );
}

/// One leg's book stale → no pair (a listing that cannot be quoted is an
/// outage, not a discount).
#[test]
fn refuses_when_one_legs_book_is_stale() {
    let mut a = adapter();
    let fresh = move |t: &str| {
        if t == KX_DOWN {
            None // the kx down leg's book is gone
        } else if t == PM_UP {
            Some(leg_book(t, dec!(0.44), dec!(600), NOW))
        } else if t == PM_DOWN {
            Some(leg_book(t, dec!(0.50), dec!(600), NOW))
        } else if t == KX_UP {
            Some(leg_book(t, dec!(0.48), dec!(400), NOW))
        } else {
            None
        }
    };
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let events = vec![paired_event()];
    let got = candidates_of(&mut a, &ctx(&markets, &events, 600, NOW, &fresh));
    assert!(got.is_empty(), "a stale leg is an outage: {got:?}");
}

/// Below the time floor (default 30s) entries stop — the pair attempt must
/// leave enough runway for both legs to fill.
#[test]
fn refuses_entries_below_the_time_floor() {
    let mut a = adapter();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let events = vec![paired_event()];
    let got = candidates_of(&mut a, &ctx(&markets, &events, 20, NOW, &fresh));
    assert!(got.is_empty(), "no fresh attempt under 30s left: {got:?}");
}

// ── lifecycle ───────────────────────────────────────────────────────────────

/// One attempt per event per round: a second evaluation in the same round
/// re-fires nothing.
#[test]
fn one_attempt_per_event_per_round() {
    let mut a = adapter();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let events = vec![paired_event()];
    let got = candidates_of(&mut a, &ctx(&markets, &events, 600, NOW, &fresh));
    assert_eq!(got.len(), 2);
    let got2 = candidates_of(&mut a, &ctx(&markets, &events, 500, NOW + 1_000, &fresh));
    assert!(got2.is_empty(), "latched for the round: {got2:?}");
}

/// 回落平仓: with BOTH legs held, a pair cost that has recovered within
/// `spread_close_pct` of the money emits exit intents for both legs; with
/// the discount still wide, it holds. Exits are { token, reason } only and
/// `breaks` stays empty (the seal).
#[test]
fn a_held_pair_releases_when_the_discount_falls_back() {
    let mut a = adapter();
    // Held: all four possible legs, so the release row is independent of
    // which listing wins each side at the current quotes.
    let held = vec![
        blitzkrieg_core::strategies::HeldPosition {
            strategy: "cross_venue_arb".into(),
            condition_id: PM.into(),
            token_id: PM_UP.into(),
            direction: "up".into(),
            shares: "100".into(),
            entry_price: "0.44".into(),
            round_slot: SLOT,
        },
        blitzkrieg_core::strategies::HeldPosition {
            strategy: "cross_venue_arb".into(),
            condition_id: KX.into(),
            token_id: KX_DOWN.into(),
            direction: "down".into(),
            shares: "100".into(),
            entry_price: "0.42".into(),
            round_slot: SLOT,
        },
        blitzkrieg_core::strategies::HeldPosition {
            strategy: "cross_venue_arb".into(),
            condition_id: KX.into(),
            token_id: KX_UP.into(),
            direction: "up".into(),
            shares: "100".into(),
            entry_price: "0.48".into(),
            round_slot: SLOT,
        },
        blitzkrieg_core::strategies::HeldPosition {
            strategy: "cross_venue_arb".into(),
            condition_id: PM.into(),
            token_id: PM_DOWN.into(),
            direction: "down".into(),
            shares: "100".into(),
            entry_price: "0.50".into(),
            round_slot: SLOT,
        },
    ];
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let events = vec![paired_event()];

    // Cost still 0.8753 (edge ~12.5% ≫ close threshold 1 − 0.5% = 0.995):
    // hold.
    let wide = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    {
        let c = blitzkrieg_core::strategies::StrategyCtx::new(&markets, SLOT, 600, NOW, &wide)
            .with_unified_events(&events)
            .with_positions(&held);
        let got = candidates_of(&mut a, &c);
        assert!(got.is_empty(), "held pair must not re-enter: {got:?}");
        assert!(drain_exits(&mut a).is_empty(), "still wide → hold");
    }

    // The asks recover to 0.50/0.49: cost = 0.99 + fees ≈ 1.0054 > 0.995 →
    // release both legs.
    let tight = pair_books(
        (dec!(0.50), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.49), dec!(250)),
        NOW,
    );
    let c = blitzkrieg_core::strategies::StrategyCtx::new(&markets, SLOT, 600, NOW, &tight)
        .with_unified_events(&events)
        .with_positions(&held);
    let got = candidates_of(&mut a, &c);
    assert!(
        got.is_empty(),
        "a held pair releases, it does not re-enter: {got:?}"
    );
    let exits = drain_exits(&mut a);
    assert_eq!(exits.len(), 2, "one release per pair leg, got {exits:?}");
    // The pair the blueprint picks on the tight books: UP from the listing
    // quoting the cheaper UP ask (kx 0.48 < pm 0.50) and DOWN from the other
    // (pm 0.50 vs kx 0.49 — the OTHER listing's down is the pair complement).
    let tokens: Vec<&str> = exits.iter().map(|e| e.token_id.as_str()).collect();
    assert!(
        tokens.contains(&KX_UP) && tokens.contains(&PM_DOWN),
        "release both pair legs: {exits:?}"
    );
    assert!(
        exits.iter().all(|e| e.reason.contains("release")),
        "release tagged: {exits:?}"
    );
}
