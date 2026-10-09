//! The feasibility gate for a two-leg hedge plan (issue #426).
//!
//! An arb is only viable if what is LEFT after both legs' fees is still
//! positive. Reverse acceptance #3 is exact here: a plan that judges
//! feasibility without charging BOTH legs' fees must be refused, and a
//! caller that claims viability without the fee terms attached is refused
//! by construction (the fields exist on the decision, not in a comment).
//!
//! Fees come from the kernel's ONE fee spelling ([`crate::exit_policy::
//! FeeSchedule::fee_per_share`]) so the hedge gate can never drift from what
//! a live fill is actually charged.

use crate::exit_policy::FeeSchedule;
use blitzkrieg_market_api::unified::{HedgeVerdict, UnifiedEvent, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// One leg of a plan, as proposed. Venue is DATA on the leg (issue #427: no
/// logic may branch on it) — feasibility math never reads it; it is carried
/// so the executor submits each leg against its own venue and the audit
/// names it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanLeg {
    pub venue: Venue,
    pub token_id: String,
    pub side: super::executor::LegSide,
    /// Expected entry price per share, in (0, 1).
    #[serde(with = "crate::decimal")]
    pub price: Decimal,
    pub size: Decimal,
}

impl PlanLeg {
    /// Total notional of the leg: price × size.
    pub fn notional(&self) -> Decimal {
        self.price * self.size
    }
}

/// One leg's fee, computed through the kernel's fee curve. Attached to the
/// plan so the viability statement cannot exist without its fee terms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeQuote {
    pub venue: Venue,
    pub token_id: String,
    /// Fee in USD for the whole leg (per-share fee × size).
    #[serde(with = "crate::decimal")]
    pub fee_usd: Decimal,
}

/// A two-leg hedge proposal, priced and fee-quoted. Construct ONLY through
/// [`HedgePlan::assess`] — the constructor is the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HedgePlan {
    pub event_id: String,
    pub legs: [PlanLeg; 2],
    pub fees: [FeeQuote; 2],
    /// Gross edge per share at the proposed prices: buy-leg price minus
    /// sell-leg price, when the plan buys one side and sells the other
    /// (both are quoted as fills the account will pay/receive).
    #[serde(with = "crate::decimal")]
    pub gross_edge_per_share: Decimal,
    /// Fees for the whole plan, both legs summed.
    #[serde(with = "crate::decimal")]
    pub total_fees_usd: Decimal,
    /// `total_fees` subtracted from the gross edge over the size.
    #[serde(with = "crate::decimal")]
    pub net_edge_usd: Decimal,
    /// True only when the NET edge (after BOTH legs' fees) is positive.
    pub viable: bool,
}

/// Why a plan was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// #425's discrepancy marks reach here: a PseudoHedge event refuses new
    /// automated hedges — the two venues may not pay the same way.
    SettlementDiscrepancy(String),
    /// Not confirmed or degraded to single-leg.
    NotHedgeable(String),
    /// The two legs must be the two opposite tokens of the SAME event.
    LegMismatch,
    /// A price is outside (0, 1) or size is non-positive.
    BadQuote,
    /// The legs are both the same side (nothing to hedge).
    SameSide,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::SettlementDiscrepancy(id) => {
                write!(
                    f,
                    "event {id} carries settlement discrepancies — refusing new automated hedge"
                )
            }
            PlanError::NotHedgeable(id) => write!(
                f,
                "event {id} is not hedgeable (not confirmed or single-leg)"
            ),
            PlanError::LegMismatch => {
                write!(f, "plan legs are not the two opposite tokens of one event")
            }
            PlanError::BadQuote => {
                write!(f, "plan quotes are outside (0,1) or size is non-positive")
            }
            PlanError::SameSide => write!(f, "both plan legs are the same side — nothing to hedge"),
        }
    }
}

impl std::error::Error for PlanError {}

/// Per-share fee on one leg through the kernel's ONE fee curve.
fn leg_fee_usd(schedule: &FeeSchedule, leg: &PlanLeg) -> Decimal {
    schedule.fee_per_share(leg.price) * leg.size
}

impl HedgePlan {
    /// Assess a two-leg proposal against a confirmed event. The gate order
    /// is the issue's order: verdict FIRST (a discrepancy-marked event never
    /// reaches fee math), then shape, then the fully-fee-loaded economics.
    ///
    /// `edge_per_share` is what the caller expects to capture per share:
    /// e.g. selling one side at `p_sell` while buying the other at `p_buy`
    /// captures `p_sell - p_buy` gross. The caller states the direction; the
    /// gate only insists both fees are charged against it.
    pub fn assess(
        event: &UnifiedEvent,
        edge_per_share: Decimal,
        legs: [PlanLeg; 2],
        schedule: &FeeSchedule,
    ) -> Result<HedgePlan, PlanError> {
        // #425 → #426 handoff: the discrepancy choke point is consulted
        // before ANYTHING else. PseudoHedge = settlement sources/rules/expiry
        // disagree across venues; automating a hedge there risks the two
        // venues paying differently — refuse new positions, touch none.
        match HedgeVerdict::for_event(event) {
            HedgeVerdict::Allowed(_) => {}
            HedgeVerdict::PseudoHedge(_) => {
                return Err(PlanError::SettlementDiscrepancy(event.id.clone()));
            }
            HedgeVerdict::NotHedgeable => {
                return Err(PlanError::NotHedgeable(event.id.clone()));
            }
        }

        let [leg_a, leg_b] = &legs;
        if leg_a.token_id == leg_b.token_id {
            return Err(PlanError::LegMismatch);
        }
        // The legs must be the event's two opposite tokens (one up, one
        // down), on the pair of venues the mapping lists.
        let token_ids: Vec<&str> = event
            .listings
            .values()
            .flat_map(|l| [l.up_token_id.as_str(), l.down_token_id.as_str()])
            .collect();
        let both_known = token_ids.contains(&leg_a.token_id.as_str())
            && token_ids.contains(&leg_b.token_id.as_str());
        if !both_known {
            return Err(PlanError::LegMismatch);
        }
        if leg_a.side == leg_b.side {
            return Err(PlanError::SameSide);
        }
        for leg in &legs {
            if leg.price <= Decimal::ZERO || leg.price >= Decimal::ONE || leg.size <= Decimal::ZERO
            {
                return Err(PlanError::BadQuote);
            }
        }

        // BOTH legs' fees, through the one curve, before any viability claim.
        let fees = [
            FeeQuote {
                venue: legs[0].venue,
                token_id: legs[0].token_id.clone(),
                fee_usd: leg_fee_usd(schedule, &legs[0]),
            },
            FeeQuote {
                venue: legs[1].venue,
                token_id: legs[1].token_id.clone(),
                fee_usd: leg_fee_usd(schedule, &legs[1]),
            },
        ];
        let total_fees_usd = fees[0].fee_usd + fees[1].fee_usd;
        let gross_edge_usd = edge_per_share * legs[0].size;
        let net_edge_usd = gross_edge_usd - total_fees_usd;
        let plan = HedgePlan {
            event_id: event.id.clone(),
            legs,
            fees,
            gross_edge_per_share: edge_per_share,
            total_fees_usd,
            net_edge_usd,
            viable: net_edge_usd > Decimal::ZERO,
        };
        Ok(plan)
    }

    /// The per-share fee drag of the whole plan — the number that must be
    /// beaten by the gross edge for viability. Exposed so the reverse
    /// acceptance can prove the gate CHARGED it (a plan judged viable while
    /// this drag exceeds the edge is the bug the test hunts).
    pub fn fee_drag_per_share(&self) -> Decimal {
        if self.legs[0].size.is_zero() {
            return Decimal::ZERO;
        }
        self.total_fees_usd / self.legs[0].size
    }

    /// Minimum common size across legs (they may be sized differently; the
    /// hedge is as good as the SHORTER leg).
    pub fn hedge_size(&self) -> Decimal {
        self.legs[0].size.min(self.legs[1].size)
    }
}
