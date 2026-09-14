//! Shadow Evolution — configuration and the mutable/immutable parameter split.
//!
//! SAFETY SPINE: parameters are split into two halves.
//!   - `MutableParams`   — legitimately evolvable knobs (entry/exit tuning).
//!   - `ImmutableConfig` — "physics": hard stop, loss breaker, daily loss cap,
//!     per-order notional. Shadow evolution can NEVER touch these, because they
//!     are not part of the swapped object at all (structural, not a check).

use crate::exit_policy::ExitConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Evolution-tunable strategy parameters (the only thing an ArcSwap can hold).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutableParams {
    /// Trend confirmation threshold (a token must hold above this).
    #[serde(with = "crate::decimal")]
    pub trend_min_price: Decimal,
    /// Resting bid = mid * factor.
    #[serde(with = "crate::decimal")]
    pub trend_entry_factor: Decimal,
    /// Never rest a bid above this price.
    #[serde(with = "crate::decimal")]
    pub trend_max_entry_price: Decimal,
    /// A confirmed trend below this is treated as reversed.
    #[serde(with = "crate::decimal")]
    pub trend_broken_price: Decimal,
}

impl Default for MutableParams {
    fn default() -> Self {
        // Mirrors the live spread_arb defaults so evolution starts from reality.
        Self {
            trend_min_price: dec!(0.55),
            trend_entry_factor: dec!(0.98),
            trend_max_entry_price: dec!(0.45),
            trend_broken_price: dec!(0.35),
        }
    }
}

impl MutableParams {
    /// Named fields for gradient checking and variant generation.
    pub fn fields(&self) -> [(&'static str, Decimal); 4] {
        [
            ("trend_min_price", self.trend_min_price),
            ("trend_entry_factor", self.trend_entry_factor),
            ("trend_max_entry_price", self.trend_max_entry_price),
            ("trend_broken_price", self.trend_broken_price),
        ]
    }

    /// Set one field by name (used by the guard's exact-application path).
    pub fn set(&mut self, name: &str, value: Decimal) {
        match name {
            "trend_min_price" => self.trend_min_price = value,
            "trend_entry_factor" => self.trend_entry_factor = value,
            "trend_max_entry_price" => self.trend_max_entry_price = value,
            "trend_broken_price" => self.trend_broken_price = value,
            _ => {}
        }
    }

    /// Read one field by name (inverse of `set`; used by directed variant
    /// generation to step a single knob).
    pub fn get(&self, name: &str) -> Decimal {
        match name {
            "trend_min_price" => self.trend_min_price,
            "trend_entry_factor" => self.trend_entry_factor,
            "trend_max_entry_price" => self.trend_max_entry_price,
            "trend_broken_price" => self.trend_broken_price,
            _ => Decimal::ZERO,
        }
    }

    /// Apply a multiplicative factor to every field (used to build variants and
    /// to step toward a target under the gradient limit).
    pub fn scaled(&self, factor: Decimal) -> Self {
        Self {
            trend_min_price: self.trend_min_price * factor,
            trend_entry_factor: self.trend_entry_factor * factor,
            trend_max_entry_price: self.trend_max_entry_price * factor,
            trend_broken_price: self.trend_broken_price * factor,
        }
    }
}

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
    /// Number of shadow variants to run (>= 2 per spec).
    pub variant_count: usize,
    /// Audit JSONL path (relative to CWD).
    pub audit_log_path: String,
    /// Risk parameters used for the virtual exit simulation (immutable laws).
    pub risk: ImmutableConfig,
    /// Exit policy the virtual variants replay. This MUST be the SAME config the
    /// live position manager uses (D-2), otherwise a variant is judged against an
    /// exit mechanism the live path never runs — a biased counterfactual. The
    /// caller passes `PositionConfig.exit`.
    pub exit_cfg: ExitConfig,
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
            audit_log_path: "data/evolution/evolution.jsonl".into(),
            risk: ImmutableConfig::default(),
            exit_cfg: ExitConfig::default(),
        }
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
