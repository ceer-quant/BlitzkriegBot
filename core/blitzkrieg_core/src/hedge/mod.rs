//! Cross-venue hedge execution (issue #426).
//!
//! One confirmed [`blitzkrieg_market_api::unified::UnifiedEvent`] lists the
//! same real-world bet on two venues. This module owns the ATOMIC two-leg
//! execution against that pair: submit both legs, and never let one leg sit
//! naked past the exposure deadline — either the other leg fills too, the
//! shortfall is re-hedged, or the filled leg is forcibly unwound.
//!
//! Market-agnostic by construction (issue #427 reverse acceptance): a leg is
//! identified by the venue it lists under, but NO logic branches on which
//! venue that is — `Venue` is data carried on the leg, and the executor's
//! decisions depend only on leg STATES (submitted / filled / failed / timed
//! out), never on venue identity.
//!
//! Layout:
//! - [`plan`]: `HedgePlan` — the feasibility gate. An arb is only ever
//!   "viable" AFTER both legs' fees are charged in full (issue #426 reverse
//!   acceptance #3); a plan that hides either leg's fee is refused before it
//!   can place anything.
//! - [`executor`]: `HedgeExecutor` — the two-leg state machine with the
//!   500 ms submit deadline, the 2 s naked-leg protection path
//!   (re-hedge or forced unwind, itself audited), and cleanup on total
//!   failure (no residual order, no residual position).
//! - [`audit`]: `HedgeAuditSink` — one JSONL line per lifecycle event
//!   (proposed → submitted → filled/failed → protective action → closed),
//!   through the shared `jsonl` rules.
//!
//! #425's settlement discrepancy marks reach this module as
//! [`blitzkrieg_market_api::unified::HedgeVerdict::PseudoHedge`]: an event
//! carrying a discrepancy refuses NEW automated hedges — the executor never
//! sees it as `Allowed`, and the fee/feasibility layer cannot override it.

pub mod audit;
pub mod executor;
pub mod exposure;
pub mod plan;
pub mod settlement;

#[cfg(test)]
mod executor_tests;

#[cfg(test)]
mod plan_tests;

pub use audit::HedgeAuditSink;
pub use executor::{HedgeExecution, HedgeExecutionState, HedgeLegId, LegReport, LegSide, LegState};
pub use plan::{FeeQuote, HedgePlan, PlanError, PlanLeg};

use serde::Serialize;

/// The exposure deadline: a leg submitted alone (counter-leg failed or still
/// unfilled) may not stay naked longer than this. Issue #426 names 2 s; the
/// protective action fires AT the deadline, not after it.
pub const NAKED_LEG_DEADLINE_MS: i64 = 2_000;

/// The submit deadline: both legs must reach the venue within this window of
/// the plan being armed, or the un-acked leg is treated as failed and the
/// protection path decides. Issue #426 names 500 ms.
pub const SUBMIT_DEADLINE_MS: i64 = 500;

/// How many times the counter venue may REFUSE the re-hedge demand before
/// the machine escalates to force-unwind — the last resort that burns fees
/// on both ends and admits the arb lost.
pub const FORCE_UNWIND_AFTER_REFUSALS: u64 = 2;

/// Why a hedge ended (or refused to start). Audited verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HedgeOutcome {
    /// Both legs filled within budget: the arb is on.
    BothFilled,
    /// A leg went naked past the deadline and the shortfall was re-hedged on
    /// the counter venue.
    ReHedged,
    /// A leg went naked past the deadline and the filled leg was forcibly
    /// unwound.
    Unwound,
    /// Both legs failed (or timed out) before either filled: cleanup ran,
    /// nothing is left on any venue.
    BothFailedClean,
    /// The plan was refused before anything was submitted (verdict, fees,
    /// sizing).
    Refused,
}

impl HedgeOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            HedgeOutcome::BothFilled => "both_filled",
            HedgeOutcome::ReHedged => "re_hedged",
            HedgeOutcome::Unwound => "unwound",
            HedgeOutcome::BothFailedClean => "both_failed_clean",
            HedgeOutcome::Refused => "refused",
        }
    }
}

/// One protective action taken by the executor, for the audit trail.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ProtectiveAction {
    /// Which leg triggered it (the naked one).
    pub leg: HedgeLegId,
    /// `re_hedge` = place the missing exposure on the counter venue;
    /// `force_unwind` = flatten the filled leg.
    pub kind: ProtectiveKind,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectiveKind {
    ReHedge,
    ForceUnwind,
}

impl ProtectiveKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProtectiveKind::ReHedge => "re_hedge",
            ProtectiveKind::ForceUnwind => "force_unwind",
        }
    }
}
