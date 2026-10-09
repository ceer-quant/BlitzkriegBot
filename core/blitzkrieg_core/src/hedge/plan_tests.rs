//! Plan gate tests: the fully-fee-loaded feasibility decision, the
//! discrepancy refusal (#425→#426 handoff), and the shape refusals.

use super::plan::{FeeQuote, HedgePlan, PlanError, PlanLeg};
use crate::exit_policy::{FeeSchedule, legacy_quadratic_schedule};
use blitzkrieg_market_api::unified::{
    DiscrepancyKind, EventStatus, ListingStatus, PlatformListing, SettlementSource, UnifiedEvent,
    Venue,
};
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

/// A minimal confirmed event: BTC up/down on Polymarket + Kalshi, active,
/// no discrepancies.
fn paired_event() -> UnifiedEvent {
    let pm = PlatformListing {
        venue: Venue::Polymarket,
        condition_id: "pm-btc".into(),
        up_token_id: "pm-up".into(),
        down_token_id: "pm-down".into(),
        asset: Some("bitcoin".into()),
        title: "Bitcoin Up or Down: October 10, 2PM ET".into(),
        settlement_rules: "Binance BTCUSDT candle at 2PM ET".into(),
        settlement_source: SettlementSource::Chainlink,
        expires_at_ms: 1_000_000,
        status: ListingStatus::Active,
    };
    let kx = PlatformListing {
        venue: Venue::Kalshi,
        condition_id: "kx-btc".into(),
        up_token_id: "kx-up".into(),
        down_token_id: "kx-down".into(),
        asset: Some("bitcoin".into()),
        title: "Bitcoin up or down at 2pm ET on October 10".into(),
        settlement_rules: "Binance BTCUSDT candle at 2PM ET".into(),
        settlement_source: SettlementSource::Chainlink,
        expires_at_ms: 1_000_000,
        status: ListingStatus::Active,
    };
    let mut listings = BTreeMap::new();
    listings.insert(Venue::Polymarket, pm);
    listings.insert(Venue::Kalshi, kx);
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

fn legs() -> [PlanLeg; 2] {
    [
        PlanLeg {
            venue: Venue::Polymarket,
            token_id: "pm-up".into(),
            side: crate::hedge::LegSide::Up,
            price: dec!(0.60),
            size: dec!(10),
        },
        PlanLeg {
            venue: Venue::Kalshi,
            token_id: "kx-down".into(),
            side: crate::hedge::LegSide::Down,
            price: dec!(0.38),
            size: dec!(10),
        },
    ]
}

/// The legacy quadratic curve: `0.125*(p*(1-p))^2` per share.
fn sched() -> FeeSchedule {
    legacy_quadratic_schedule()
}

#[test]
fn viable_plan_charges_both_legs_fees() {
    // Sell-side 0.60 vs buy-side 0.38: gross edge 0.22/share × 10 = $2.20.
    // Fees: 0.125*(0.6*0.4)^2 = 0.0072/share and 0.125*(0.38*0.62)^2 ≈
    // 0.004376/share; total ≈ $0.1158. Net must be gross MINUS fees, and
    // viability is the NET sign.
    let event = paired_event();
    let plan =
        HedgePlan::assess(&event, dec!(0.22), legs(), &sched()).expect("valid shape assesses");
    assert!(plan.viable, "net {:#}", plan.net_edge_usd);
    // The fee quotes carry both venues.
    assert_eq!(plan.fees[0].venue, Venue::Polymarket);
    assert_eq!(plan.fees[1].venue, Venue::Kalshi);
    assert!(plan.fees[0].fee_usd > dec!(0));
    assert!(plan.fees[1].fee_usd > dec!(0));
    // Net is exactly gross minus the sum of BOTH legs' fees.
    let gross = dec!(0.22) * dec!(10);
    assert_eq!(plan.net_edge_usd, gross - plan.total_fees_usd);
}

#[test]
fn fee_drag_can_flip_viability_off() {
    // A thin edge that the fees eat: gross 0.01/share × 10 = $0.10, fees
    // ≈ $0.1158 — the plan must come out NOT viable.
    let event = paired_event();
    let plan =
        HedgePlan::assess(&event, dec!(0.01), legs(), &sched()).expect("valid shape assesses");
    assert!(!plan.viable);
    assert!(plan.total_fees_usd > dec!(0.10));
}

#[test]
fn discrepancy_marked_event_refuses_new_hedges() {
    // #425 → #426 handoff: a settlement-source discrepancy marks the event
    // PseudoHedge; the plan gate must refuse BEFORE fee math.
    let mut event = paired_event();
    event.discrepancies = vec![DiscrepancyKind::SettlementSource];
    let err = HedgePlan::assess(&event, dec!(0.30), legs(), &sched())
        .expect_err("discrepancy-marked event must refuse");
    match err {
        PlanError::SettlementDiscrepancy(id) => assert_eq!(id, "pm-btc|kx-btc"),
        other => panic!("wrong refusal: {other:?}"),
    }
}

#[test]
fn unpaired_event_refuses() {
    let mut event = paired_event();
    event.status = EventStatus::SingleLegAvailable;
    let err = HedgePlan::assess(&event, dec!(0.30), legs(), &sched())
        .expect_err("single-leg event must refuse");
    assert!(matches!(err, PlanError::NotHedgeable(_)));
}

#[test]
fn legs_from_other_tokens_refuse() {
    let event = paired_event();
    let mut l = legs();
    l[0].token_id = "totally-other".into();
    let err =
        HedgePlan::assess(&event, dec!(0.30), l, &sched()).expect_err("foreign tokens must refuse");
    assert!(matches!(err, PlanError::LegMismatch));
}

#[test]
fn same_side_legs_refuse() {
    let event = paired_event();
    let mut l = legs();
    l[1].side = crate::hedge::LegSide::Up;
    let err =
        HedgePlan::assess(&event, dec!(0.30), l, &sched()).expect_err("same side must refuse");
    assert!(matches!(err, PlanError::SameSide));
}

#[test]
fn bad_quotes_refuse() {
    let event = paired_event();
    for bad in [dec!(0), dec!(1), dec!(1.5)] {
        let mut l = legs();
        l[0].price = bad;
        let err =
            HedgePlan::assess(&event, dec!(0.30), l, &sched()).expect_err("bad price refuses");
        assert!(matches!(err, PlanError::BadQuote));
    }
    let mut l = legs();
    l[1].size = dec!(0);
    let err = HedgePlan::assess(&event, dec!(0.30), l, &sched()).expect_err("zero size refuses");
    assert!(matches!(err, PlanError::BadQuote));
}

#[test]
fn fee_drag_per_share_is_fees_over_size() {
    let event = paired_event();
    let plan =
        HedgePlan::assess(&event, dec!(0.22), legs(), &sched()).expect("valid shape assesses");
    assert_eq!(plan.fee_drag_per_share(), plan.total_fees_usd / dec!(10));
    // And the viability statement is exactly: gross edge > fee drag.
    assert_eq!(
        plan.viable,
        plan.gross_edge_per_share > plan.fee_drag_per_share()
    );
}

#[test]
fn fee_quote_shapes_serialize() {
    let q = FeeQuote {
        venue: Venue::Kalshi,
        token_id: "kx-up".into(),
        fee_usd: dec!(0.5),
    };
    let s = serde_json::to_string(&q).expect("serializes");
    assert!(s.contains("kx-up"));
}
