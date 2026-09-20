//! Shadow Evolution — the evolve signal (per strategy, E2-c / #28).

use super::knobs::MutableParams;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Why the evaluator proposed an evolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionReason {
    HigherWinRate,
    BetterProfitFactor,
    CombinedImprovement,
}

impl EvolutionReason {
    /// Human-readable label for the UIs.
    pub fn label(&self) -> &'static str {
        match self {
            EvolutionReason::HigherWinRate => "win rate",
            EvolutionReason::BetterProfitFactor => "profit factor",
            EvolutionReason::CombinedImprovement => "win rate + profit factor",
        }
    }
}

/// A proposal to switch ONE strategy's parameters to a variant's.
///
/// `strategy` is the isolation key: the signal names whose parameters move, so
/// the guard, the hot swap and the audit file all address exactly one strategy.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvolveSignal {
    pub signal_id: String,
    pub timestamp: i64,
    /// The strategy whose parameters this proposal would change.
    pub strategy: String,
    pub from_params: MutableParams,
    pub to_params: MutableParams,
    pub reason: EvolutionReason,
    /// 0..1 confidence derived from the magnitude of improvement.
    #[serde(with = "crate::decimal")]
    pub confidence: Decimal,
    pub sample_count: u32,
    #[serde(with = "crate::decimal")]
    pub expected_improvement: Decimal,
    /// The variant that produced this signal.
    pub variant_id: String,
}

impl EvolveSignal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        signal_id: String,
        timestamp: i64,
        strategy: String,
        from_params: MutableParams,
        to_params: MutableParams,
        reason: EvolutionReason,
        confidence: Decimal,
        sample_count: u32,
        expected_improvement: Decimal,
        variant_id: String,
    ) -> Self {
        Self {
            signal_id,
            timestamp,
            strategy,
            from_params,
            to_params,
            reason,
            confidence: confidence.clamp(Decimal::ZERO, Decimal::ONE),
            sample_count,
            expected_improvement,
            variant_id,
        }
    }
}
