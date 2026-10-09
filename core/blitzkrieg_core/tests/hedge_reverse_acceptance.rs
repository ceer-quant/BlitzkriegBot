//! Issue #426 reverse acceptance — the three MUST-FAIL gates. Each test
//! pins one "the system must never allow this" clause:
//!
//! 1. `reverse_a`: one leg filled, the other failed, and NO protective
//!    action fired → the naked-leg deadline is violated. Red means the
//!    protection path is missing.
//! 2. `reverse_b`: a settlement-discrepancy event gets its two legs hedged
//!    automatically. Red means the #425→#426 handoff (discrepancy-marked
//!    events refuse new hedges) is missing.
//! 3. `reverse_c`: an arb judged viable WITHOUT charging both legs' fees.
//!    Red means the fee gate is missing.
//!
//! Each is the failure mode the mutation check hunts: remove the guard and
//! the test must go red.

use blitzkrieg_core::exit_policy::legacy_quadratic_schedule;
use blitzkrieg_core::hedge::executor::{HedgeExecution, LegSide};
use blitzkrieg_core::hedge::plan::{HedgePlan, PlanError, PlanLeg};
use blitzkrieg_core::hedge::{HedgeAuditSink, HedgeOutcome, NAKED_LEG_DEADLINE_MS};
use blitzkrieg_market_api::unified::{
    EventStatus, ListingStatus, PlatformListing, SettlementSource, UnifiedEvent, Venue,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

fn listing(venue: Venue, cond: &str, rules: &str, source: SettlementSource) -> PlatformListing {
    PlatformListing {
        venue,
        condition_id: cond.into(),
        up_token_id: format!("{cond}-up"),
        down_token_id: format!("{cond}-down"),
        asset: Some("bitcoin".into()),
        title: "Bitcoin Up or Down: October 10, 2PM ET".into(),
        settlement_rules: rules.into(),
        settlement_source: source,
        expires_at_ms: 1_000_000,
        status: ListingStatus::Active,
    }
}

fn paired_event() -> UnifiedEvent {
    let mut listings = BTreeMap::new();
    listings.insert(
        Venue::Polymarket,
        listing(
            Venue::Polymarket,
            "pm-btc",
            "Binance BTCUSDT candle at 2PM ET",
            SettlementSource::Chainlink,
        ),
    );
    listings.insert(
        Venue::Kalshi,
        listing(
            Venue::Kalshi,
            "kx-btc",
            "Binance BTCUSDT candle at 2PM ET",
            SettlementSource::Chainlink,
        ),
    );
    UnifiedEvent {
        id: "pm-btc|kx-btc".into(),
        title: "Bitcoin hour".into(),
        settlement_rules: "Binance BTCUSDT candle at 2PM ET".into(),
        settlement_source: SettlementSource::Chainlink,
        listings,
        discrepancies: Vec::new(),
        status: EventStatus::Paired,
    }
}

/// The one-venue-fills-the-other-refuses scenario, driven exactly as the
/// wiring will drive it: arm → ack both → counter fails → fill lands →
/// clock runs past the naked deadline → SOMETHING must have happened.
#[test]
fn reverse_a_naked_leg_past_deadline_without_protection_is_impossible() {
    struct NeverProtect;
    impl blitzkrieg_core::hedge::executor::ExecutorHooks for NeverProtect {
        fn submit_leg(
            &mut self,
            _leg: &blitzkrieg_core::hedge::HedgeLegId,
            _side: LegSide,
            _price: Decimal,
            _size: Decimal,
        ) {
        }
        fn execute_protective(
            &mut self,
            _action: &blitzkrieg_core::hedge::executor::ProtectiveDemand,
            _at_ms: i64,
        ) -> Option<blitzkrieg_core::hedge::ProtectiveAction> {
            None // protection NEVER fires
        }
        fn cancel_leg(&mut self, _leg: &blitzkrieg_core::hedge::HedgeLegId) {}
    }

    let legs = [
        (
            blitzkrieg_core::hedge::HedgeLegId::new(Venue::Polymarket, "pm-btc-up"),
            LegSide::Up,
        ),
        (
            blitzkrieg_core::hedge::HedgeLegId::new(Venue::Kalshi, "kx-btc-down"),
            LegSide::Down,
        ),
    ];
    let mut ex = HedgeExecution::arm("exec-ra", "pm-btc|kx-btc", legs, 1_000).expect("arms");
    let mut hooks = NeverProtect;
    let audit = HedgeAuditSink::at(None);

    ex.leg_acked(0);
    ex.leg_acked(1);
    ex.leg_failed(1, 1_050);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);

    // Run the clock far past the naked deadline — 3× over.
    let deadline = 1_100 + NAKED_LEG_DEADLINE_MS;
    for t in [deadline, deadline + 1_000, deadline + 10_000] {
        ex.tick(t, &mut hooks, &audit);
    }

    // The acceptance: the machine must have MADE NOISE about the naked leg —
    // a protective demand was either TAKEN (protective_actions) or REFUSED
    // and audited (protective_refusals). A machine that sits silent on a
    // naked leg with no terminal state is the #426 horror: one leg left
    // alone against the market with nobody told.
    let made_noise = !ex.protective_actions.is_empty() || ex.protective_refusals > 0;
    assert!(
        made_noise || ex.outcome.is_some(),
        "naked leg sat past {NAKED_LEG_DEADLINE_MS}ms with NO protective demand \
         (taken or refused) and no terminal state — one leg was left alone \
         against the market in silence"
    );
    // Positive half: with hooks that refuse, the demand was still made —
    // three ticks, three audited refusals, never silence.
    assert!(
        ex.protective_refusals >= 3,
        "each past-deadline tick must retry and audit the refusal; got {} refusals",
        ex.protective_refusals
    );
}

/// The #425→#426 handoff: a discrepancy-marked event (venues disagree on
/// settlement) must NEVER get an automated two-leg hedge — the plan gate
/// refuses before anything is submitted.
#[test]
fn reverse_b_settlement_discrepancy_event_never_gets_hedged() {
    let mut event = paired_event();
    // One venue pays by Chainlink, the other by an official statement: the
    // two venues may not pay the same way.
    event
        .listings
        .get_mut(&Venue::Kalshi)
        .unwrap()
        .settlement_source = SettlementSource::OfficialStatement;
    event.revalidate();
    assert!(
        !event.discrepancies.is_empty(),
        "fixture must carry the discrepancy mark for this test to mean anything"
    );

    let legs = [
        PlanLeg {
            venue: Venue::Polymarket,
            token_id: "pm-btc-up".into(),
            side: LegSide::Up,
            price: dec!(0.60),
            size: dec!(10),
        },
        PlanLeg {
            venue: Venue::Kalshi,
            token_id: "kx-btc-down".into(),
            side: LegSide::Down,
            price: dec!(0.38),
            size: dec!(10),
        },
    ];
    // Even a WILDLY profitable quote is refused: the danger is not the
    // economics, it is the two venues paying differently.
    let err = HedgePlan::assess(&event, dec!(0.35), legs, &legacy_quadratic_schedule())
        .expect_err("discrepancy-marked event must refuse an automated hedge");
    match err {
        PlanError::SettlementDiscrepancy(id) => assert_eq!(id, "pm-btc|kx-btc"),
        other => panic!("wrong refusal: {other:?}"),
    }
}

/// The fee gate: a plan whose gross edge does not survive BOTH legs' fees
/// must not be judged viable. The classic fraud is comparing gross edge to
/// zero — this test pins the fee-loaded arithmetic.
#[test]
fn reverse_c_viability_without_both_legs_fees_is_impossible() {
    let event = paired_event();
    let legs = [
        PlanLeg {
            venue: Venue::Polymarket,
            token_id: "pm-btc-up".into(),
            side: LegSide::Up,
            price: dec!(0.60),
            size: dec!(10),
        },
        PlanLeg {
            venue: Venue::Kalshi,
            token_id: "kx-btc-down".into(),
            side: LegSide::Down,
            price: dec!(0.38),
            size: dec!(10),
        },
    ];
    let plan = HedgePlan::assess(&event, dec!(0.01), legs, &legacy_quadratic_schedule())
        .expect("shape-valid plan assesses");

    // The plan must carry BOTH fee quotes — a gate that hides a leg's fee
    // cannot exist, because the fields are on the decision.
    assert!(
        plan.fees[0].fee_usd > Decimal::ZERO,
        "leg A fee must be charged"
    );
    assert!(
        plan.fees[1].fee_usd > Decimal::ZERO,
        "leg B fee must be charged"
    );

    // The gross edge ($0.10 over 10 shares) is LESS than the two legs' fees
    // (≈$0.116): the plan must be NOT viable. A gate that said "viable"
    // here would be quoting gross as net — the reverse-acceptance fraud.
    assert!(
        plan.total_fees_usd > dec!(0.01) * dec!(10),
        "fixture must have fees exceed the gross edge; got {}",
        plan.total_fees_usd
    );
    assert!(
        !plan.viable,
        "gross edge ${} was judged viable while both legs' fees sum to ${} — \
         viability without fee loading",
        plan.gross_edge_per_share * dec!(10),
        plan.total_fees_usd
    );
    // And the equality that makes the number auditable: net ≡ gross − fees.
    assert_eq!(
        plan.net_edge_usd,
        plan.gross_edge_per_share * dec!(10) - plan.total_fees_usd
    );
}

/// Companion (positive) control for reverse_a: when the counter venue CAN
/// take the re-hedge, the machine demands it and lands on ReHedged.
#[test]
fn control_a_protection_fires_and_lands_rehedged() {
    struct TakeReHedge;
    impl blitzkrieg_core::hedge::executor::ExecutorHooks for TakeReHedge {
        fn submit_leg(
            &mut self,
            _leg: &blitzkrieg_core::hedge::HedgeLegId,
            _side: LegSide,
            _price: Decimal,
            _size: Decimal,
        ) {
        }
        fn execute_protective(
            &mut self,
            _action: &blitzkrieg_core::hedge::executor::ProtectiveDemand,
            at_ms: i64,
        ) -> Option<blitzkrieg_core::hedge::ProtectiveAction> {
            Some(blitzkrieg_core::hedge::ProtectiveAction {
                leg: blitzkrieg_core::hedge::HedgeLegId::new(Venue::Kalshi, "kx-btc-down"),
                kind: blitzkrieg_core::hedge::ProtectiveKind::ReHedge,
                at_ms,
            })
        }
        fn cancel_leg(&mut self, _leg: &blitzkrieg_core::hedge::HedgeLegId) {}
    }

    let legs = [
        (
            blitzkrieg_core::hedge::HedgeLegId::new(Venue::Polymarket, "pm-btc-up"),
            LegSide::Up,
        ),
        (
            blitzkrieg_core::hedge::HedgeLegId::new(Venue::Kalshi, "kx-btc-down"),
            LegSide::Down,
        ),
    ];
    let mut ex = HedgeExecution::arm("exec-ca", "pm-btc|kx-btc", legs, 1_000).expect("arms");
    let mut hooks = TakeReHedge;
    let audit = HedgeAuditSink::at(None);

    ex.leg_acked(0);
    ex.leg_acked(1);
    ex.leg_failed(1, 1_050);
    ex.leg_filled(0, dec!(10), dec!(0.60), 1_100);
    ex.tick(1_100 + NAKED_LEG_DEADLINE_MS, &mut hooks, &audit);
    assert_eq!(
        ex.protective_actions.len(),
        1,
        "re-hedge demanded at the deadline"
    );
    // The re-hedge fills to the same size: whole again, labeled ReHedged.
    ex.leg_filled(1, dec!(10), dec!(0.40), 1_100 + NAKED_LEG_DEADLINE_MS + 100);
    assert_eq!(ex.outcome, Some(HedgeOutcome::ReHedged));
}

/// Companion (positive) control for reverse_b: the SAME quotes on a CLEAN
/// event go through — proving the refusal in reverse_b is the discrepancy,
/// not the shape of the quotes.
#[test]
fn control_b_clean_event_with_same_quotes_is_hedgeable() {
    let event = paired_event();
    let legs = [
        PlanLeg {
            venue: Venue::Polymarket,
            token_id: "pm-btc-up".into(),
            side: LegSide::Up,
            price: dec!(0.60),
            size: dec!(10),
        },
        PlanLeg {
            venue: Venue::Kalshi,
            token_id: "kx-btc-down".into(),
            side: LegSide::Down,
            price: dec!(0.38),
            size: dec!(10),
        },
    ];
    let plan = HedgePlan::assess(&event, dec!(0.35), legs, &legacy_quadratic_schedule())
        .expect("clean event with fat edge assesses");
    assert!(plan.viable);
    assert_eq!(plan.event_id, "pm-btc|kx-btc");
}
