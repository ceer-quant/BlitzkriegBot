//! Shadow Evolution — the evolve signal.

use super::config::MutableParams;
use rust_decimal::Decimal;
use serde::Serialize;

/// Why the evaluator proposed an evolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionReason {
    HigherWinRate,
    BetterProfitFactor,
    CombinedImprovement,
}

/// A proposal to switch the live mutable parameters to a variant's.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvolveSignal {
    pub signal_id: String,
    pub timestamp: i64,
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
    pub fn new(
        signal_id: String,
        timestamp: i64,
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
