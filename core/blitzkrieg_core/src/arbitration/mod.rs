//! Arbitration — DEV_V0_3 §3: the strategy advises, the kernel adjudicates.
//!
//! A strategy (Rust cdylib or Lua state machine) is a ZERO-TRUST component: it
//! sees books, never the signer, the order channel, the ledger or credentials.
//! Everything it can do is submit a SUGGESTION; `pipeline::process_intent`
//! runs every suggestion through the four gates and returns one
//! [`Decision`]. The gates REUSE the kernel's existing implementations —
//! `OrderIntent::validate`, `RiskGate::check_with_equity` + the loss breakers,
//! `Ledger::reserve`, and the exit policy's own stop ladder — so no threshold
//! ever has two sources of truth (§3.3 rule 3: a PR that duplicates a judgment
//! is refused).
//!
//! Every suggestion is audited to `data/audit/intents.jsonl` (`audit.rs`),
//! including the rejected ones; the audit write is never allowed to block or
//! break trading.

mod audit;
mod pipeline;

pub use audit::{AuditSink, DecisionPush, IntentAuditRecord};
pub use pipeline::{IntentCtx, Outcome, StrategyIntent, process_intent};

use serde::Serialize;

/// Which gate produced a trace. The audit carries this enum, never an index —
/// an index is not a stable identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GateId {
    Legality,
    Risk,
    Reservation,
    Physics,
}

/// One gate's result. The trace is what makes "which layer changed this
/// intent" reviewable instead of only the final Decision.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateTrace {
    pub gate: GateId,
    pub outcome: GateOutcome,
    /// The gate's human-readable justification (log + audit, not the UI's
    /// main view — the UI prints this verbatim, §13.4).
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GateOutcome {
    Pass,
    Modify,
    Reject,
}

/// The ONLY two modifications the kernel makes to a suggestion.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Modification {
    /// Risk shrank the size: only ever down (`approved <= suggested`).
    SizeReduced {
        suggested: rust_decimal::Decimal,
        approved: rust_decimal::Decimal,
        limit: String,
    },
    /// Tick/step alignment: the kernel picks the direction and records it.
    PriceClamped {
        suggested: rust_decimal::Decimal,
        approved: rust_decimal::Decimal,
        tick: rust_decimal::Decimal,
    },
}

/// Survival binding (Gate 4's product). This RECORDS the exit discipline the
/// kernel will enforce; it adds no trigger path — firing stays with the
/// existing exit_policy / position machinery.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicsBinding {
    /// Hard-stop price, from the existing `effective_stop_pct` at entry time.
    pub stop_price: rust_decimal::Decimal,
    /// Forced exit seconds before the round ends (`ExitConfig::force_exit_sec`).
    pub force_exit_sec: i64,
    /// Ladder snapshot. With no ladder configured this is the existing exit
    /// policy's PROJECTION (§4.3): one step, `close_ratio = 1.0` — the same
    /// "close it all at once" semantics the shipped kernel has.
    pub ladder: Vec<LadderStep>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LadderStep {
    /// Trigger threshold (positive = profit %, negative = loss %).
    pub at_pct: rust_decimal::Decimal,
    /// Fraction to close, in [0, 1].
    pub close_ratio: rust_decimal::Decimal,
    /// Where the stop moves after this step (`None` = unchanged).
    pub move_stop_to: Option<rust_decimal::Decimal>,
}

/// The verdict. `Approved`/`Modified` hand the SAME request to the existing
/// `Core::place()` path; `Rejected` means the intent never reaches OME.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Decision {
    Approved {
        request_id: String,
        shares: rust_decimal::Decimal,
        price: rust_decimal::Decimal,
        physics: PhysicsBinding,
    },
    Modified {
        request_id: String,
        modification: Modification,
        shares: rust_decimal::Decimal,
        price: rust_decimal::Decimal,
        physics: PhysicsBinding,
    },
    Rejected {
        reason: RejectReason,
        gate: GateId,
        detail: String,
    },
}

impl Decision {
    /// The `decision.status` string the audit record, the `intent.audit.tail`
    /// filter and the panel all read (`APPROVED` / `MODIFIED` / `REJECTED`).
    pub fn status(&self) -> &'static str {
        match self {
            Decision::Approved { .. } => "APPROVED",
            Decision::Modified { .. } => "MODIFIED",
            Decision::Rejected { .. } => "REJECTED",
        }
    }
}

/// Why a suggestion was refused. Every variant maps onto an EXISTING
/// [`blitzkrieg_market_api::CoreErrorCode`] — the UI and the gates read the
/// kernel's own error vocabulary, never a second one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RejectReason {
    /// Gate 1
    Malformed,
    OutOfPriceBand,
    BadTick,
    NotInRound,
    /// Gate 2
    AccountLimit,
    GlobalLimit,
    LossBreaker,
    Cooldown,
    KillSwitch,
    Capacity,
    /// Gate 3
    InsufficientFunds,
    /// The pipeline itself (never silently swallowed)
    Internal,
}

/// The one mapping table: each [`RejectReason`] names the existing
/// [`CoreErrorCode`] the kernel already uses for that failure class. Pinned by
/// a test over every variant, so adding a variant without a mapping fails to
/// compile AND the mapping cannot drift from the error vocabulary.
pub fn reason_code(reason: RejectReason) -> blitzkrieg_market_api::CoreErrorCode {
    use RejectReason as R;
    use blitzkrieg_market_api::CoreErrorCode as C;
    match reason {
        R::Malformed => C::InvalidParams,
        R::OutOfPriceBand => C::InvalidParams,
        R::BadTick => C::InvalidTickSize,
        R::NotInRound => C::InvalidParams,
        R::AccountLimit => C::RiskRejected,
        R::GlobalLimit => C::RiskRejected,
        R::LossBreaker => C::RiskRejected,
        R::Cooldown => C::RiskRejected,
        R::KillSwitch => C::KillSwitchActive,
        R::Capacity => C::RiskRejected,
        R::InsufficientFunds => C::InsufficientFunds,
        R::Internal => C::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitzkrieg_market_api::CoreErrorCode;

    /// §3.2: every RejectReason variant maps onto an existing CoreErrorCode —
    /// the mapping is exhaustive (compile-enforced by the match above) and
    /// each arm names the code the kernel already uses for that class.
    #[test]
    fn reason_code_covers_every_variant_with_the_existing_vocabulary() {
        let cases = [
            (RejectReason::Malformed, CoreErrorCode::InvalidParams),
            (RejectReason::OutOfPriceBand, CoreErrorCode::InvalidParams),
            (RejectReason::BadTick, CoreErrorCode::InvalidTickSize),
            (RejectReason::NotInRound, CoreErrorCode::InvalidParams),
            (RejectReason::AccountLimit, CoreErrorCode::RiskRejected),
            (RejectReason::GlobalLimit, CoreErrorCode::RiskRejected),
            (RejectReason::LossBreaker, CoreErrorCode::RiskRejected),
            (RejectReason::Cooldown, CoreErrorCode::RiskRejected),
            (RejectReason::KillSwitch, CoreErrorCode::KillSwitchActive),
            (RejectReason::Capacity, CoreErrorCode::RiskRejected),
            (
                RejectReason::InsufficientFunds,
                CoreErrorCode::InsufficientFunds,
            ),
            (RejectReason::Internal, CoreErrorCode::Internal),
        ];
        for (reason, want) in cases {
            assert_eq!(reason_code(reason), want, "{reason:?}");
        }
    }

    /// The audit's `decision.status` and the `intent.audit.tail` filter
    /// (`decision=rejected`) must spell the statuses the same way.
    #[test]
    fn decision_status_matches_the_wire_filter_vocabulary() {
        let rejected = Decision::Rejected {
            reason: RejectReason::Malformed,
            gate: GateId::Legality,
            detail: "x".into(),
        };
        assert_eq!(rejected.status(), "REJECTED");
        let json = serde_json::to_value(&rejected).unwrap();
        assert_eq!(json["status"], "REJECTED");
        assert_eq!(json["gate"], "LEGALITY");
    }
}
