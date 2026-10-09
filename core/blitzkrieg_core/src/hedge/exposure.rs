//! Merged cross-venue exposure + joint risk (issue #426, items ⑤–⑥).
//!
//! The same event hedged on two venues produces TWO positions the kernel
//! already tracks; the panel must show them as ONE event row with the NET
//! direction (what survives if the event resolves) against the GROSS size
//! (what is actually at risk on each venue), and the joint risk gate must
//! cap BOTH: total exposure across all hedged events and concentration in
//! a single event.
//!
//! Venue-agnostic: positions carry their venue as a STRING tag; no logic
//! branches on which venue it is.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One venue-side position fact, as the merged view consumes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegPosition {
    pub venue: String,
    pub token_id: String,
    /// Which side of the event this leg holds (`up`/`down`).
    pub side: String,
    pub shares: Decimal,
    /// Average entry price per share.
    pub entry_price: Decimal,
}

/// One event's merged cross-venue position (the panel row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergedPosition {
    pub event_id: String,
    /// Legs by venue, verbatim.
    pub legs: Vec<LegPosition>,
    /// GROSS: total shares held across venues (both directions).
    pub gross_shares: Decimal,
    /// NET: up-side shares minus down-side shares, signed. Zero = fully
    /// hedged (the resolution outcome moves nothing); positive = net long
    /// the up side; negative = net short (long the down side).
    pub net_direction: Decimal,
    /// Notional at entry, summed over legs: Σ shares × entry_price.
    pub notional_usd: Decimal,
    /// `true` when the net direction is (near) zero — the pair is a hedge,
    /// not a directional bet.
    pub hedged: bool,
}

/// Near-zero threshold for calling a pair hedged: net shares within ±0.5
/// (half a share) of flat count as hedged — fractional-share dust from
/// partial fills should not flip the label.
pub const HEDGED_TOLERANCE_SHARES: Decimal = rust_decimal_macros::dec!(0.5);

/// Build the merged view for one event from its legs.
pub fn merge(event_id: &str, mut legs: Vec<LegPosition>) -> MergedPosition {
    legs.sort_by(|a, b| a.venue.cmp(&b.venue).then(a.token_id.cmp(&b.token_id)));
    let gross_shares = legs.iter().map(|l| l.shares).sum();
    let net_direction = legs
        .iter()
        .map(|l| if l.side == "up" { l.shares } else { -l.shares })
        .sum();
    let notional_usd = legs.iter().map(|l| l.shares * l.entry_price).sum();
    MergedPosition {
        event_id: event_id.to_string(),
        legs,
        gross_shares,
        net_direction,
        notional_usd,
        hedged: net_direction.abs() <= HEDGED_TOLERANCE_SHARES,
    }
}

/// Why the joint risk gate refused a new hedge (or demanded attention).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JointRiskRefusal {
    /// Σ notional across ALL hedged events would exceed the cap.
    TotalExposureCap {
        current_usd: Decimal,
        new_notional_usd: Decimal,
        cap_usd: Decimal,
    },
    /// One event's notional would exceed the concentration cap.
    SingleEventConcentration {
        event_id: String,
        new_notional_usd: Decimal,
        cap_usd: Decimal,
    },
}

impl std::fmt::Display for JointRiskRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JointRiskRefusal::TotalExposureCap {
                current_usd,
                new_notional_usd,
                cap_usd,
            } => write!(
                f,
                "total exposure ${current_usd} + new ${new_notional_usd} would exceed cap ${cap_usd}"
            ),
            JointRiskRefusal::SingleEventConcentration {
                event_id,
                new_notional_usd,
                cap_usd,
            } => write!(
                f,
                "event {event_id} notional ${new_notional_usd} would exceed per-event cap ${cap_usd}"
            ),
        }
    }
}

/// The joint risk gate. Caps are plain data (wired from the systemic risk
/// limits); the check is PURE so the gate cannot drift between the panel
/// and the executor.
#[derive(Debug, Clone)]
pub struct JointRiskGate {
    /// Σ notional across all open hedged events.
    pub total_exposure_cap_usd: Decimal,
    /// Max notional in ONE event.
    pub single_event_cap_usd: Decimal,
}

impl JointRiskGate {
    pub fn new(total_exposure_cap_usd: Decimal, single_event_cap_usd: Decimal) -> Self {
        Self {
            total_exposure_cap_usd,
            single_event_cap_usd,
        }
    }

    /// May a new hedge with this notional open, given the current open
    /// events' notionals? Checks concentration FIRST (the tighter, more
    /// local fact), then the portfolio cap.
    pub fn check(
        &self,
        open_notional_by_event: &BTreeMap<String, Decimal>,
        event_id: &str,
        new_notional_usd: Decimal,
    ) -> Result<(), JointRiskRefusal> {
        let event_current = open_notional_by_event
            .get(event_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        if event_current + new_notional_usd > self.single_event_cap_usd {
            return Err(JointRiskRefusal::SingleEventConcentration {
                event_id: event_id.to_string(),
                new_notional_usd,
                cap_usd: self.single_event_cap_usd,
            });
        }
        let total: Decimal = open_notional_by_event.values().sum();
        if total + new_notional_usd > self.total_exposure_cap_usd {
            return Err(JointRiskRefusal::TotalExposureCap {
                current_usd: total,
                new_notional_usd,
                cap_usd: self.total_exposure_cap_usd,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn leg(venue: &str, side: &str, shares: Decimal, price: Decimal) -> LegPosition {
        LegPosition {
            venue: venue.to_string(),
            token_id: format!("{venue}-tok"),
            side: side.to_string(),
            shares,
            entry_price: price,
        }
    }

    #[test]
    fn fully_hedged_pair_shows_net_zero() {
        let m = merge(
            "ev",
            vec![
                leg("polymarket", "up", dec!(10), dec!(0.60)),
                leg("kalshi", "down", dec!(10), dec!(0.38)),
            ],
        );
        assert_eq!(m.gross_shares, dec!(20));
        assert_eq!(m.net_direction, dec!(0));
        assert!(m.hedged);
        assert_eq!(m.notional_usd, dec!(9.8)); // 10×0.60 + 10×0.38
        // Legs sorted by venue for a stable panel order.
        assert_eq!(m.legs[0].venue, "kalshi");
        assert_eq!(m.legs[1].venue, "polymarket");
    }

    #[test]
    fn unhedged_residual_shows_net_direction() {
        // Re-hedge filled only 6 against 10: net 4 short the down side.
        let m = merge(
            "ev",
            vec![
                leg("polymarket", "up", dec!(10), dec!(0.60)),
                leg("kalshi", "down", dec!(6), dec!(0.38)),
            ],
        );
        assert_eq!(m.net_direction, dec!(4));
        assert!(!m.hedged);
    }

    #[test]
    fn single_leg_is_fully_directional() {
        let m = merge("ev", vec![leg("polymarket", "down", dec!(10), dec!(0.40))]);
        assert_eq!(m.net_direction, dec!(-10));
        assert!(!m.hedged);
    }

    #[test]
    fn dust_within_tolerance_still_hedged() {
        let m = merge(
            "ev",
            vec![
                leg("polymarket", "up", dec!(10), dec!(0.60)),
                leg("kalshi", "down", dec!(9.8), dec!(0.38)),
            ],
        );
        assert!(m.hedged, "0.2 share residual is dust, not a direction");
    }

    #[test]
    fn concentration_cap_refuses() {
        let gate = JointRiskGate::new(dec!(1000), dec!(200));
        let mut open = BTreeMap::new();
        open.insert("ev1".to_string(), dec!(150));
        let err = gate
            .check(&open, "ev1", dec!(60))
            .expect_err("150+60 > 200 per-event cap");
        assert!(matches!(
            err,
            JointRiskRefusal::SingleEventConcentration { .. }
        ));
        // Same notional on a fresh event passes concentration...
        gate.check(&open, "ev2", dec!(60)).expect("ev2 0+60 ≤ 200");
        // ...but breaches the portfolio cap: 150+60=210 ≤ 1000, actually fine.
    }

    #[test]
    fn total_exposure_cap_refuses() {
        let gate = JointRiskGate::new(dec!(1000), dec!(500));
        let mut open = BTreeMap::new();
        open.insert("ev1".to_string(), dec!(600));
        open.insert("ev2".to_string(), dec!(350));
        let err = gate
            .check(&open, "ev3", dec!(100))
            .expect_err("950+100 > 1000 total cap");
        assert!(matches!(err, JointRiskRefusal::TotalExposureCap { .. }));
    }

    #[test]
    fn within_both_caps_passes() {
        let gate = JointRiskGate::new(dec!(1000), dec!(500));
        let mut open = BTreeMap::new();
        open.insert("ev1".to_string(), dec!(600));
        // ev1: 600+200=800 ≤ 500? NO — 800 > 500, so this must refuse on
        // concentration; the test's real positive case is a different event.
        let err = gate
            .check(&open, "ev1", dec!(200))
            .expect_err("800 > 500 per-event cap");
        assert!(matches!(
            err,
            JointRiskRefusal::SingleEventConcentration { .. }
        ));
        // ev2 is fresh: 0+200 ≤ 500 per-event, 600+200=800 ≤ 1000 total.
        gate.check(&open, "ev2", dec!(200))
            .expect("within both caps");
    }

    #[test]
    fn unknown_event_starts_at_zero() {
        let gate = JointRiskGate::new(dec!(1000), dec!(200));
        let open = BTreeMap::new();
        gate.check(&open, "ev-new", dec!(150))
            .expect("fresh event within caps");
        let err = gate
            .check(&open, "ev-new", dec!(250))
            .expect_err("fresh event above single cap");
        assert!(matches!(
            err,
            JointRiskRefusal::SingleEventConcentration { .. }
        ));
    }
}
