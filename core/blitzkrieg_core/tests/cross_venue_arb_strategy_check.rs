//! cross_venue_arb acceptance checks — the REAL package
//! (`user_layer/strategies_lua/cross_venue_arb`) driven through the engine
//! adapter, exactly the surface the kernel runs. The 跨所套利弹窗 v2 pairing
//! contract, mapped:
//!
//!   leg config — the `arb_*` params keys the kernel merges on
//!               `arb.config.set` are the pairing authority: unconfigured →
//!               NOTHING pairs (fail-closed), a config naming venues not in
//!               the round pairs nothing either (test 6's fallback seam);
//!   pairing   — per configured asset: one `bk.markets()` row per leg venue,
//!               both rows' UP books fresh; the UP leg = the cheaper UP ask,
//!               the DOWN leg = the other row (venue identity is data);
//!   positive  — all-in pair cost ≤ 1 − spread_open_pct → BOTH legs
//!               suggested, each at its own ask, EQUAL declared share counts,
//!               capacity = min(two legs' ask depths) (issue #427: "容量 =
//!               两平台各自深度的最小值");
//!   stamp     — each leg is stamped with the account its TOKEN'S VENUE
//!               trades under (the `arb.config.set` venue→account map;
//!               v2's dual-leg accounting); unconfigured → the instance
//!               account (pre-v2 shape);
//!   latch     — per (asset, direction): an armed UP never blocks the DOWN
//!               leg of the same asset in the same round (test 5's row);
//!   fees      — BOTH legs' taker fees are charged inside the decision; no
//!               fee schedule in force → no suggestions at all (fail closed);
//!   negative  — pair cost inside the spread, depth below the floor, one
//!               leg's book stale → refuse;
//!   reverse   — a hypothetical adapter that ignored one leg's depth would
//!               size past the thinner leg: asserted here by construction
//!               (the declared shares are ALWAYS ≤ both legs' depths — if
//!               the package ever emitted a size above either leg's depth
//!               this row goes red);
//!   release   — a held leg whose all-in pair cost falls back within
//!               spread_close_pct emits an exit intent at its own bid
//!               ("回落 < Y% 平仓"), and exits are { token, reason } only
//!               (the seal: no reserved keys, `breaks` always empty).
//!
//! The adapter is constructed the way the service does it at load:
//! gate declarations stamped, the kernel's fee schedule injected (here the
//! legacy_quadratic curve the deployment line charges) and the leg
//! configuration hot-pushed (`set_arb_leg_config` — the v2 seam).

use std::path::PathBuf;

use blitzkrieg_core::account::CrossVenueArbConfig;
use blitzkrieg_core::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
use blitzkrieg_core::strategies::{EngineStrategy, StrategyCtx, StrategyExitIntent};
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use blitzkrieg_lua_runtime::FeeScheduleView;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NOW: i64 = 1_700_000_000_000;
const SLOT: i64 = NOW / 300_000;

// Two rows of the SAME bet: the polymarket row quotes the cheaper UP ask,
// the kalshi row the cheaper DOWN ask. The pair therefore legs UP@pm and
// DOWN@kx — deliberately NOT one venue's rows, so a package that traded one
// venue's own UP+DOWN pair cannot pass the cross-leg row.
const PM: &str = "pm-cond";
const KX: &str = "kx-cond";
const PM_UP: &str = "pm-up";
const PM_DOWN: &str = "pm-down";
const KX_UP: &str = "kx-up";
const KX_DOWN: &str = "kx-down";

/// The v2 leg configuration the fixtures push: leg A = polymarket/account-a,
/// leg B = kalshi/account-b, the BTC pair. Accounts exist only as strings
/// here — the ADAPTER never validates them (that is the kernel's job on
/// `arb.config.set`); the stamp just resolves the venue→account map.
fn legs_config() -> CrossVenueArbConfig {
    CrossVenueArbConfig {
        leg_a_venue: "polymarket".into(),
        leg_a_account: "account-a".into(),
        leg_b_venue: "kalshi".into(),
        leg_b_account: "account-b".into(),
        pairs: vec!["BTC".into()],
    }
}

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../user_layer/strategies_lua/cross_venue_arb")
}

/// The adapter WITH the v2 leg configuration hot-pushed (the `arb.config.set`
/// runtime shape: `Core::arb_config_set_persisting` → `Engine` fan-out →
/// this cell).
fn adapter() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("cross_venue_arb package must load");
    let mut a = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions);
    a.set_fee_schedule(FeeScheduleView {
        name: "legacy_quadratic".into(),
        rate: "0.125".into(),
        exponent: 2,
    });
    EngineStrategy::set_arb_leg_config(&mut a, Some(legs_config()));
    a
}

/// The same adapter WITHOUT a fee schedule — `bk.fees()` answers nil and the
/// strategy must refuse to price anything.
fn adapter_feeless() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("cross_venue_arb package must load");
    let mut a = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions);
    EngineStrategy::set_arb_leg_config(&mut a, Some(legs_config()));
    a
}

/// The PRE-v2 shape: no leg configuration pushed. The strategy pairs nothing
/// (no `arb_*` keys in the bag) and stamps the instance account.
fn adapter_unconfigured() -> LuaEngineAdapter {
    let loaded = load_lua_package(&package_dir()).expect("cross_venue_arb package must load");
    LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare(loaded.holds_to_settlement, loaded.gate_exemptions)
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
    time_left_sec: i64,
    now_ms: i64,
    fresh: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
) -> StrategyCtx<'a> {
    StrategyCtx::new(markets, SLOT, time_left_sec, now_ms, fresh)
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

/// The pair quotes: UP ask/depth per row and DOWN ask/depth per row.
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

fn two_rows() -> Vec<CryptoMarket> {
    vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ]
}

// ── positive ────────────────────────────────────────────────────────────────

/// pm: 0.44 up / 0.50 down; kx: 0.48 up / 0.42 down.
/// Cross-venue pair = UP@pm 0.44 + DOWN@kx 0.42 = 0.86 + both fees
/// (legacy_quadratic 0.125·(p(1−p))²: 0.00765703 + 0.00765703 ≈ 0.0153)
/// ≈ 0.8753 ≤ 0.98 → fires; one-vs-one row pair would cost 0.94 / 0.90 + fees
/// and must NOT be the suggestion. v2 stamps: the UP leg (a polymarket token)
/// lands on account-a, the DOWN leg (a kalshi token) on account-b — the
/// dual-leg accounting's whole point.
#[test]
fn a_discounted_cross_venue_pair_suggests_both_legs() {
    let mut a = adapter();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let got = candidates_of(&mut a, &ctx(&two_rows(), 600, NOW, &fresh));

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
    // v2 dual-leg stamp: each leg's account = its token's venue's account.
    assert_eq!(
        up.account_id, "account-a",
        "the polymarket leg stamps the polymarket account"
    );
    assert_eq!(
        down.account_id, "account-b",
        "the kalshi leg stamps the kalshi account"
    );
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
    let got = candidates_of(&mut a, &ctx(&two_rows(), 600, NOW, &fresh));
    assert_eq!(got.len(), 2);
    for s in &got {
        let shares = s.shares.expect("declared shares");
        assert!(
            shares <= dec!(40),
            "capacity must be min(two depths) = 40, got {shares}"
        );
    }
}

// ── the leg configuration is the pairing authority (v2) ─────────────────────

/// Unconfigured (no `arb_*` keys merged — the pre-`arb.config.set` state):
/// the strategy pairs NOTHING, even with a perfect discount sitting on both
/// rows. Fail-closed by construction. (Test 4's seam: single venue ⇒ zero
/// intents is this row's sibling — one row present ⇒ no second row ⇒ no pair.)
#[test]
fn unconfigured_pairs_nothing() {
    let mut a = adapter_unconfigured();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let got = candidates_of(&mut a, &ctx(&two_rows(), 600, NOW, &fresh));
    assert!(got.is_empty(), "no leg config ⇒ no pairing at all: {got:?}");
    assert!(drain_exits(&mut a).is_empty());
}

/// A config naming a venue with NO row in the round (test 6's fallback seam):
/// the asset cannot pair — nothing fires, nothing panics; the OTHER venue's
/// row is never traded on its own. A hypothetical token emitted anyway (an
/// adapter bug) would stamp the INSTANCE account — the fallback the adapter's
/// stamp resolves for any venue outside the map.
#[test]
fn a_row_missing_for_one_leg_venue_pairs_nothing() {
    let mut a = adapter();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    // Only polymarket's row is in the round; kalshi's is missing.
    let markets = vec![market(PM_UP, PM_DOWN, PM, "polymarket")];
    let got = candidates_of(&mut a, &ctx(&markets, 600, NOW, &fresh));
    assert!(got.is_empty(), "one leg short ⇒ no pair: {got:?}");
}

/// A venue NOT in the configuration (a third venue's row) is invisible to
/// the pairing — its quotes never leak into a leg.
#[test]
fn a_third_venue_row_is_ignored() {
    let mut a = adapter();
    // "other-venue" quotes a absurdly cheap UP ask — if the pairing read it,
    // the UP leg would take it. It must not.
    let cheap_up = market("ov-up", "ov-down", "ov-cond", "other-venue");
    let fresh_3v = move |t: &str| match t {
        "ov-up" => Some(leg_book(t, dec!(0.01), dec!(9_999), NOW)),
        t if t == PM_UP => Some(leg_book(t, dec!(0.44), dec!(600), NOW)),
        t if t == PM_DOWN => Some(leg_book(t, dec!(0.50), dec!(600), NOW)),
        t if t == KX_UP => Some(leg_book(t, dec!(0.48), dec!(400), NOW)),
        t if t == KX_DOWN => Some(leg_book(t, dec!(0.42), dec!(250), NOW)),
        _ => None,
    };
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
        cheap_up,
    ];
    let got = candidates_of(&mut a, &ctx(&markets, 600, NOW, &fresh_3v));
    assert_eq!(got.len(), 2, "the configured pair still fires");
    assert!(
        got.iter().all(|s| s.token_id != "ov-up"),
        "the third venue's cheap UP ask must never become a leg: {got:?}"
    );
    assert!(
        got.iter().all(|s| s.account_id != "cross_venue_arb"),
        "configured legs stamp their venue accounts, never the instance: {got:?}"
    );
}

// ── pairing discipline ──────────────────────────────────────────────────────

/// A duplicated row for one leg venue is ambiguous — the asset refuses to
/// pair (fail-closed; the old event-level discipline at the row level).
#[test]
fn refuses_a_duplicated_leg_venue_row() {
    let mut a = adapter();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let markets = vec![
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(PM_UP, PM_DOWN, PM, "polymarket"),
        market(KX_UP, KX_DOWN, KX, "kalshi"),
    ];
    let got = candidates_of(&mut a, &ctx(&markets, 600, NOW, &fresh));
    assert!(got.is_empty(), "two rows claim leg A — refuse: {got:?}");
}

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
    let got = candidates_of(&mut a, &ctx(&two_rows(), 600, NOW, &fresh));
    assert!(got.is_empty(), "feeless deployment must fail closed");
}

/// The spread must beat `spread_open_pct` (default 2%): pair cost 0.98531406
/// leaves an edge of 1.47% < 2% → refuse.
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
    let got = candidates_of(&mut a, &ctx(&two_rows(), 600, NOW, &fresh));
    assert!(
        got.is_empty(),
        "edge 1.47% < spread_open_pct 2% must not fire: {got:?}"
    );
}

/// One leg's book stale → no pair (a row that cannot be quoted is an
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
    let got = candidates_of(&mut a, &ctx(&two_rows(), 600, NOW, &fresh));
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
    let got = candidates_of(&mut a, &ctx(&two_rows(), 20, NOW, &fresh));
    assert!(got.is_empty(), "no fresh attempt under 30s left: {got:?}");
}

// ── lifecycle ───────────────────────────────────────────────────────────────

/// One attempt per (asset, direction) per round: a second evaluation in the
/// same round re-fires nothing.
#[test]
fn one_attempt_per_leg_direction_per_round() {
    let mut a = adapter();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let got = candidates_of(&mut a, &ctx(&two_rows(), 600, NOW, &fresh));
    assert_eq!(got.len(), 2);
    let got2 = candidates_of(&mut a, &ctx(&two_rows(), 500, NOW + 1_000, &fresh));
    assert!(got2.is_empty(), "latched for the round: {got2:?}");
}

/// The latch is per DIRECTION: an armed UP never blocks the DOWN leg of the
/// same asset. Simulate the UP leg's order having been taken by the kernel
/// (a held UP position): the DOWN leg must still be offered in the SAME
/// round — one leg's fate never vetoes its sibling's offer.
#[test]
fn an_armed_up_leg_does_not_block_the_down_leg_same_round() {
    let mut a = adapter();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let markets = two_rows();

    // Round 1: the DOWN leg is already HELD from an earlier fill, so ONLY the
    // UP entry is offered — and only the UP latch arms this round.
    let held_down = vec![blitzkrieg_core::strategies::HeldPosition {
        account_id: "account-b".into(),
        strategy: "cross_venue_arb".into(),
        condition_id: KX.into(),
        token_id: KX_DOWN.into(),
        direction: "down".into(),
        shares: "250".into(),
        entry_price: "0.42".into(),
        round_slot: SLOT,
    }];
    let c1 = blitzkrieg_core::strategies::StrategyCtx::new(&markets, SLOT, 600, NOW, &fresh)
        .with_positions(&held_down);
    let got = candidates_of(&mut a, &c1);
    assert_eq!(
        got.len(),
        1,
        "precondition: only the UP leg is offered (the DOWN leg is held)"
    );
    assert_eq!(got[0].token_id, PM_UP);

    // Round 1, next cycle: the roles flipped — the UP leg's entry was taken
    // (held) and the DOWN leg's earlier fill is gone. The DOWN leg must still
    // be offered: an armed UP never vetoes its sibling.
    let held = vec![blitzkrieg_core::strategies::HeldPosition {
        account_id: "account-a".into(),
        strategy: "cross_venue_arb".into(),
        condition_id: PM.into(),
        token_id: PM_UP.into(),
        direction: "up".into(),
        shares: "250".into(),
        entry_price: "0.44".into(),
        round_slot: SLOT,
    }];
    let c = blitzkrieg_core::strategies::StrategyCtx::new(&markets, SLOT, 500, NOW + 1_000, &fresh)
        .with_positions(&held);
    let got2 = candidates_of(&mut a, &c);
    assert_eq!(
        got2.len(),
        1,
        "the DOWN leg is offered in the same round even though UP fired first: {got2:?}"
    );
    assert_eq!(
        got2[0].token_id, KX_DOWN,
        "the surviving leg is the DOWN leg"
    );
    assert_eq!(got2[0].direction, SignalDirection::Down);
    assert_eq!(
        got2[0].account_id, "account-b",
        "the re-offered leg still stamps its own venue's account"
    );
}

/// 回落平仓: with BOTH legs held, a pair cost that has recovered within
/// `spread_close_pct` of the money emits exit intents for both legs; with
/// the discount still wide, it holds. Exits are { token, reason } only and
/// `breaks` stays empty (the seal). v2: each exit intent carries its leg's
/// account — the B leg's exit resolves to the B account's row (test 3).
#[test]
fn a_held_pair_releases_when_the_discount_falls_back() {
    let mut a = adapter();
    // Held: all four possible legs, so the release row is independent of
    // which row wins each side at the current quotes.
    let held = vec![
        blitzkrieg_core::strategies::HeldPosition {
            account_id: "account-a".into(),
            strategy: "cross_venue_arb".into(),
            condition_id: PM.into(),
            token_id: PM_UP.into(),
            direction: "up".into(),
            shares: "100".into(),
            entry_price: "0.44".into(),
            round_slot: SLOT,
        },
        blitzkrieg_core::strategies::HeldPosition {
            account_id: "account-b".into(),
            strategy: "cross_venue_arb".into(),
            condition_id: KX.into(),
            token_id: KX_DOWN.into(),
            direction: "down".into(),
            shares: "100".into(),
            entry_price: "0.42".into(),
            round_slot: SLOT,
        },
        blitzkrieg_core::strategies::HeldPosition {
            account_id: "account-b".into(),
            strategy: "cross_venue_arb".into(),
            condition_id: KX.into(),
            token_id: KX_UP.into(),
            direction: "up".into(),
            shares: "100".into(),
            entry_price: "0.48".into(),
            round_slot: SLOT,
        },
        blitzkrieg_core::strategies::HeldPosition {
            account_id: "account-a".into(),
            strategy: "cross_venue_arb".into(),
            condition_id: PM.into(),
            token_id: PM_DOWN.into(),
            direction: "down".into(),
            shares: "100".into(),
            entry_price: "0.50".into(),
            round_slot: SLOT,
        },
    ];
    let markets = two_rows();

    // Cost still 0.8753 (edge ~12.5% ≫ close threshold 1 − 0.5% = 0.995):
    // hold.
    let wide = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    {
        let c = blitzkrieg_core::strategies::StrategyCtx::new(&markets, SLOT, 600, NOW, &wide)
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
        .with_positions(&held);
    let got = candidates_of(&mut a, &c);
    assert!(
        got.is_empty(),
        "a held pair releases, it does not re-enter: {got:?}"
    );
    let exits = drain_exits(&mut a);
    assert_eq!(exits.len(), 2, "one release per pair leg, got {exits:?}");
    // The pair the blueprint picks on the tight books: UP from the row
    // quoting the cheaper UP ask (kx 0.48 < pm 0.50) and DOWN from the other
    // (pm 0.50 vs kx 0.49 — the OTHER row's down is the pair complement).
    let tokens: Vec<&str> = exits.iter().map(|e| e.token_id.as_str()).collect();
    assert!(
        tokens.contains(&KX_UP) && tokens.contains(&PM_DOWN),
        "release both pair legs: {exits:?}"
    );
    assert!(
        exits.iter().all(|e| e.reason.contains("release")),
        "release tagged: {exits:?}"
    );
    // v2 dual-leg stamp on the way out: the kx token exits under the B
    // account, the pm token under the A account — each closes its OWN row.
    for e in &exits {
        let expected = if e.token_id.starts_with("kx") {
            "account-b"
        } else {
            "account-a"
        };
        assert_eq!(
            e.account_id, expected,
            "the exit intent stamps the token's venue account: {e:?}"
        );
    }
}

/// The unconfigured adapter's stamps are the INSTANCE account (the pre-v2
/// shape the fallback contract preserves): even if some token were emitted
/// (here: none — unconfigured pairs nothing, so this row pins the stamp
/// resolution directly through the adapter's exit surface with a fake
/// holding only if the pairing ran; the honest assertion is the empty one),
/// the v2 fallback lives at the venue level — a configured map that does not
/// cover a token's venue stamps the instance account. That path is exercised
/// in the KernelServer check (a row whose venue is configured but whose
/// account fell back is not reachable from a correctly validated config).
#[test]
fn instance_account_is_the_unconfigured_stamp() {
    let mut a = adapter_unconfigured();
    let fresh = pair_books(
        (dec!(0.44), dec!(600), dec!(0.50), dec!(600)),
        (dec!(0.48), dec!(400), dec!(0.42), dec!(250)),
        NOW,
    );
    let got = candidates_of(&mut a, &ctx(&two_rows(), 600, NOW, &fresh));
    assert!(
        got.is_empty(),
        "unconfigured ⇒ no pairing, no stamp: {got:?}"
    );
    assert!(drain_exits(&mut a).is_empty());
}
