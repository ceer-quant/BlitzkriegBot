//! Shadow Evolution — configuration and the mutable/immutable parameter split.
//!
//! SAFETY SPINE: parameters are split into two halves.
//!   - `MutableParams`   — the per-strategy evolvable knob sets (E2-c / #28).
//!   - `ImmutableConfig` — "physics": hard stop, loss breaker, daily loss cap,
//!     per-order notional. Shadow evolution can NEVER touch these, because they
//!     are not part of the swapped object at all (structural, not a check).
//!
//! A strategy's own evolvable surface — which knobs, and within which domain —
//! is declared by the strategy itself ([`super::knobs::KnobSpec`], produced by
//! `EngineStrategy::evolvable_knobs` in-tree and by the optional
//! `bk_strategy_evolvable_knobs` symbol across the C ABI). The kernel never
//! invents a knob for a strategy.

use crate::exit_policy::ExitConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

pub use super::knobs::{KnobDeclaration, KnobSpec, MutableParams, StrategyParams};

/// Parameters that are PHYSICAL LAW and must never evolve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImmutableConfig {
    /// Hard stop-loss percent (never widened).
    #[serde(with = "crate::decimal")]
    pub hard_stop_loss_pct: Decimal,
    /// Consecutive-loss circuit breaker threshold (never disabled).
    pub max_consecutive_losses: u32,
    /// Daily loss cap in USD (never raised).
    #[serde(with = "crate::decimal")]
    pub max_daily_loss_usd: Decimal,
    /// Per-order notional cap in USD (never raised).
    #[serde(with = "crate::decimal")]
    pub max_order_notional: Decimal,
}

impl Default for ImmutableConfig {
    fn default() -> Self {
        Self {
            hard_stop_loss_pct: dec!(50),
            max_consecutive_losses: 3,
            max_daily_loss_usd: dec!(200),
            max_order_notional: dec!(2.5),
        }
    }
}

/// Shadow-evolution configuration. Disabled by default (opt-in).
#[derive(Debug, Clone)]
pub struct ShadowEvolutionConfig {
    pub enabled: bool,
    /// Rolling evaluation window (seconds) for the metrics comparison.
    pub evaluation_window_secs: i64,
    /// Minimum closed virtual trades before a variant is considered.
    pub min_sample_count: u32,
    /// Variant win rate must exceed the baseline by at least this (0.05 = +5%).
    pub min_win_rate_improvement: Decimal,
    /// Variant profit factor must exceed baseline * (1 + this).
    pub min_profit_factor_improvement: Decimal,
    /// A variant must have existed at least this long before it can trigger.
    pub min_observation_secs: i64,
    /// Minimum gap between two applied evolutions.
    pub cooldown_secs: i64,
    /// Maximum per-field relative change per evolution step (0.05 = ±5%).
    pub max_gradient: Decimal,
    /// Number of shadow variants to run **per strategy** (>= 2 per spec).
    pub variant_count: usize,
    /// Directory holding the audit logs. Each strategy writes its OWN file,
    /// `<dir>/<strategy>.jsonl` (E2-c / #28), so one strategy's history can
    /// never be read as another's.
    pub audit_dir: String,
    /// Risk parameters used for the virtual exit simulation (immutable laws).
    pub risk: ImmutableConfig,
    /// Exit policy the virtual variants replay. This MUST be the SAME config the
    /// live position manager uses (D-2), otherwise a variant is judged against an
    /// exit mechanism the live path never runs — a biased counterfactual. The
    /// caller passes `PositionConfig.exit`.
    pub exit_cfg: ExitConfig,
    /// Cap on strategies tracked simultaneously (bounded memory / audit fan-out).
    pub max_strategies: usize,
    /// E13 (#95): when false (the default) a qualifying variant becomes a
    /// held **EvolutionProposal** the operator accepts/rejects/defers; when
    /// true the evaluator applies it itself (the unattended mode, still under
    /// every guard). Toggleable at runtime via `shadow_evolution.set_auto`.
    pub auto_evolve: bool,
    /// Seconds between DEEP evolution rounds (default 72h): each round
    /// re-anchors every unit's variant set with compound multi-knob mutants,
    /// exploring combinations the single-knob rotation never visits.
    pub evolution_cycle_secs: i64,
    /// How long an undecided proposal stays decidable (default 7 days — the
    /// DryRun verification window). Past it the proposal expires.
    pub proposal_ttl_secs: i64,
    /// How many knobs one DEEP-cycle variant moves simultaneously (>= 1).
    /// 1 collapses the deep round back to the directed single-knob sweep.
    pub deep_dims: usize,
}

impl Default for ShadowEvolutionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            evaluation_window_secs: 1800,
            min_sample_count: 30,
            min_win_rate_improvement: dec!(0.05),
            min_profit_factor_improvement: dec!(0.10),
            min_observation_secs: 300,
            cooldown_secs: 600,
            max_gradient: dec!(0.05),
            variant_count: 3,
            audit_dir: "data/evolution".into(),
            risk: ImmutableConfig::default(),
            exit_cfg: ExitConfig::default(),
            max_strategies: 16,
            auto_evolve: false,
            evolution_cycle_secs: 72 * 3600,
            proposal_ttl_secs: 7 * 24 * 3600,
            deep_dims: 2,
        }
    }
}

impl ShadowEvolutionConfig {
    /// Audit path for one strategy: `<audit_dir>/<strategy>.jsonl`.
    ///
    /// The strategy name is used verbatim because it is already a
    /// kernel-validated identifier (registry-unique); the only sanitisation
    /// needed is to keep a path separator out, so a name can never escape the
    /// audit directory.
    pub fn audit_path_for(&self, strategy: &str) -> std::path::PathBuf {
        let safe: String = strategy
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        std::path::Path::new(&self.audit_dir).join(format!("{safe}.jsonl"))
    }
}

/// Evolution status reported to the UI/IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionStatus {
    Disabled,
    Evaluating,
    Cooling,
}

/// Per-variant snapshot for observability.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VariantView {
    pub id: String,
    pub label: String,
    /// Which strategy this variant belongs to (E2-c: variants are per-strategy).
    pub strategy: String,
    pub sample_count: u32,
    #[serde(with = "crate::decimal")]
    pub win_rate: Decimal,
    #[serde(with = "crate::decimal")]
    pub profit_factor: Decimal,
    #[serde(with = "crate::decimal")]
    pub total_pnl_usd: Decimal,
    pub age_sec: i64,
    pub is_baseline: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_path_is_per_strategy_and_sanitised() {
        let cfg = ShadowEvolutionConfig::default();
        assert_eq!(
            cfg.audit_path_for("spread_arb").to_string_lossy(),
            "data/evolution/spread_arb.jsonl"
        );
        assert_eq!(
            cfg.audit_path_for("dog_strategy").to_string_lossy(),
            "data/evolution/dog_strategy.jsonl"
        );
        // A separator cannot escape the audit directory.
        assert_eq!(
            cfg.audit_path_for("../evil").to_string_lossy(),
            "data/evolution/.._evil.jsonl"
        );
        assert_ne!(cfg.audit_path_for("a"), cfg.audit_path_for("b"));
    }
}
