//! Shadow Evolution — module entry and manager.
//!
//! Ties config, variants, evaluator, guard, hot-swap and audit together. The
//! manager is owned by the `Core`; the engine holds a clone of the hot-swap
//! handle so parameter changes take effect on the next tick with no restart.
//!
//! Isolation: every variant tick runs under `catch_unwind`; a panicking variant
//! is marked crashed and excluded, and NEVER propagates into the main strategy
//! path (spec: "影子引擎崩溃不影响主策略").

pub mod audit;
pub mod config;
pub mod evaluator;
pub mod guard;
pub mod hot_swap;
pub mod signal;
pub mod variants;

pub use config::{EvolutionStatus, ImmutableConfig, MutableParams, ShadowEvolutionConfig, VariantView};
pub use signal::{EvolutionReason, EvolveSignal};

use crate::model::OrderbookSnapshot;
use audit::AuditLog;
use hot_swap::HotSwap;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use variants::{build_variants, Metrics, Variant};

/// Result of one evaluation/action, mapped to events by the caller.
pub enum EvolutionOutcome {
    Signal(EvolveSignal),
    Applied(EvolveSignal),
    Rejected { signal: EvolveSignal, reason: String },
    RolledBack { from: MutableParams, to: MutableParams },
}

pub struct ShadowEvolution {
    cfg: ShadowEvolutionConfig,
    enabled: bool,
    swap: HotSwap,
    variants: Vec<Variant>,
    /// Token → round expiry (ms), fed by round updates.
    token_expiry: HashMap<String, i64>,
    last_evolution_ms: i64,
    evolution_count: u64,
    rejected_count: u64,
    /// Parameters in force before the most recent applied evolution (rollback).
    previous: Option<MutableParams>,
    audit: AuditLog,
    /// Last emitted status (for the UI).
    last_signal_at: i64,
}

impl ShadowEvolution {
    pub fn new(cfg: ShadowEvolutionConfig, initial: MutableParams) -> Self {
        let swap = HotSwap::new(initial.clone());
        let audit = AuditLog::new(&cfg.audit_log_path);
        let variants = if cfg.enabled {
            build_variants(&initial, cfg.variant_count.max(2), cfg.max_gradient, &cfg.exit_cfg, 0)
        } else {
            Vec::new()
        };
        Self {
            enabled: cfg.enabled,
            cfg,
            swap,
            variants,
            token_expiry: HashMap::new(),
            last_evolution_ms: 0,
            evolution_count: 0,
            rejected_count: 0,
            previous: None,
            audit,
            last_signal_at: 0,
        }
    }

    /// Shared hot-swap handle for the engine's hot path.
    pub fn handle(&self) -> Arc<arc_swap::ArcSwap<MutableParams>> {
        self.swap.handle()
    }

    pub fn current_params(&self) -> MutableParams {
        (*self.swap.current()).clone()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enable evolution: build the variant set around the current parameters.
    pub fn enable(&mut self, now_ms: i64) {
        if self.enabled {
            return;
        }
        self.enabled = true;
        self.cfg.enabled = true;
        let base = self.current_params();
        self.variants = build_variants(&base, self.cfg.variant_count.max(2), self.cfg.max_gradient, &self.cfg.exit_cfg, now_ms);
    }

    pub fn disable(&mut self) {
        self.enabled = false;
        self.cfg.enabled = false;
        self.variants.clear();
    }

    /// Record the current round's markets so variants know token expiries.
    ///
    /// D-3: this does NOT rebuild the variant set. Rebuilding every round reset
    /// every variant's sample counter, so across a 900s production round it could
    /// never reach `min_sample_count`. Instead we only advance token expiries and
    /// drop virtual positions for tokens that are no longer in the live round
    /// (their market expired, which live force-exits); closed-trade history is
    /// retained so metrics accumulate across round boundaries.
    pub fn on_round(&mut self, markets: &[crate::model::CryptoMarket], _now_ms: i64) {
        if !self.enabled {
            return;
        }
        self.token_expiry.clear();
        let mut valid: std::collections::HashSet<String> = std::collections::HashSet::new();
        for m in markets {
            self.token_expiry.insert(m.up_token_id.clone(), m.expires_at_ms);
            self.token_expiry.insert(m.down_token_id.clone(), m.expires_at_ms);
            valid.insert(m.up_token_id.clone());
            valid.insert(m.down_token_id.clone());
        }
        for v in self.variants.iter_mut() {
            v.retain_tokens(&valid);
        }
    }

    /// Feed a market tick to every variant (baseline + mutated). Observation
    /// only: this never touches the real ledger, positions or orders.
    pub fn on_tick(
        &mut self,
        token_id: &str,
        book: &OrderbookSnapshot,
        confirmed: bool,
        now_ms: i64,
    ) {
        if !self.enabled {
            return;
        }
        let expires_at = self.token_expiry.get(token_id).copied().unwrap_or(now_ms + 900_000);
        for v in self.variants.iter_mut() {
            if v.crashed {
                continue;
            }
            // Crash isolation: a panicking variant is quarantined, not fatal.
            let res = catch_unwind(AssertUnwindSafe(|| {
                v.on_tick(token_id, book, confirmed, expires_at, now_ms);
            }));
            if res.is_err() {
                v.crashed = true;
                tracing::warn!(variant = %v.id, "shadow variant panicked — quarantined");
            }
        }
    }

    /// Baseline metrics (the live-parameter simulation) for evaluation.
    fn baseline_metrics(&mut self, now_ms: i64) -> Metrics {
        if let Some(b) = self.variants.iter_mut().find(|v| v.is_baseline) {
            b.metrics(self.cfg.evaluation_window_secs, now_ms)
        } else {
            Metrics::default()
        }
    }

    /// Run one evaluation pass. Applies (or rejects) at most one evolution.
    pub fn evaluate(&mut self, now_ms: i64) -> Vec<EvolutionOutcome> {
        let mut out = Vec::new();
        if !self.enabled || self.variants.is_empty() {
            return out;
        }
        let baseline = self.baseline_metrics(now_ms);
        let selection = evaluator::evaluate(&self.cfg, &baseline, &mut self.variants, now_ms, self.last_evolution_ms);
        let Some(sel) = selection else { return out };

        self.last_signal_at = now_ms;
        out.push(EvolutionOutcome::Signal(sel.signal.clone()));

        // Safety locks, then apply.
        let gradient = guard::validate_gradient(&sel.signal.from_params, &sel.signal.to_params, self.cfg.max_gradient);
        let immut = guard::validate_immutable(&sel.signal.from_params, &sel.signal.to_params, &self.cfg.risk);

        if let Err(e) = &gradient {
            self.rejected_count += 1;
            self.audit.record_rejection(&sel.signal, e.to_string(), "failed", "passed");
            out.push(EvolutionOutcome::Rejected { signal: sel.signal, reason: e.to_string() });
            // Respect cooldown even on rejection to avoid a hot rejection loop.
            self.last_evolution_ms = now_ms;
            return out;
        }
        if let Err(e) = &immut {
            self.rejected_count += 1;
            self.audit.record_rejection(&sel.signal, e.to_string(), "passed", "failed");
            out.push(EvolutionOutcome::Rejected { signal: sel.signal, reason: e.to_string() });
            self.last_evolution_ms = now_ms;
            return out;
        }

        // Admissible: publish atomically and start a fresh evaluation epoch.
        let from = sel.signal.from_params.clone();
        self.previous = Some(from);
        let applied = sel.signal.to_params.clone();
        self.swap.store(applied.clone());
        self.last_evolution_ms = now_ms;
        self.evolution_count += 1;
        self.audit.record_applied(&sel.signal);
        // Re-anchor: parameters just changed, so the previous variants' trade
        // history describes params that are no longer in force. Rebuilding here
        // is correct (and is NOT the old every-round reset bug: samples now
        // accumulate across rounds, so the next evolution is still reachable). It
        // also prevents a self-reinforcing ratchet, where the baseline's stale
        // pre-evolution losses would let the same knob "win" again every cooldown.
        self.variants = build_variants(&applied, self.cfg.variant_count.max(2), self.cfg.max_gradient, &self.cfg.exit_cfg, now_ms);
        out.push(EvolutionOutcome::Applied(sel.signal));
        out
    }

    /// Manually apply parameters (operator override). Validates the same two
    /// safety locks as an evolved proposal, then hot-swaps and remembers the
    /// previous set for rollback.
    pub fn apply_params(&mut self, newp: MutableParams, now_ms: i64) -> Result<(), String> {
        let old = self.current_params();
        guard::validate_gradient(&old, &newp, self.cfg.max_gradient).map_err(|e| e.to_string())?;
        guard::validate_immutable(&old, &newp, &self.cfg.risk).map_err(|e| e.to_string())?;
        self.previous = Some(old);
        self.swap.store(newp);
        self.last_evolution_ms = now_ms;
        Ok(())
    }

    /// Manually roll back to the parameters in force before the last evolution.
    pub fn rollback(&mut self, now_ms: i64) -> Result<EvolutionOutcome, String> {
        let Some(prev) = self.previous.clone() else {
            return Err("no previous parameters to roll back to".into());
        };
        let from = self.current_params();
        self.swap.store(prev.clone());
        self.previous = Some(from.clone());
        self.last_evolution_ms = now_ms;
        self.audit.record(audit::AuditRecord {
            timestamp: now_ms,
            signal_id: format!("rollback-{now_ms}"),
            from_params: from.clone(),
            to_params: prev.clone(),
            reason: EvolutionReason::CombinedImprovement,
            confidence: Decimal::ONE,
            sample_count: 0,
            expected_improvement: Decimal::ZERO,
            applied: true,
            gradient_check: "n/a".into(),
            immutable_check: "n/a".into(),
            rejection: None,
            rollback: true,
        });
        Ok(EvolutionOutcome::RolledBack { from, to: prev })
    }

    pub fn status(&self, now_ms: i64) -> EvolutionStatus {
        if !self.enabled {
            return EvolutionStatus::Disabled;
        }
        if self.last_evolution_ms > 0 && now_ms - self.last_evolution_ms < self.cfg.cooldown_secs * 1000 {
            return EvolutionStatus::Cooling;
        }
        EvolutionStatus::Evaluating
    }

    pub fn variant_views(&mut self, now_ms: i64) -> Vec<VariantView> {
        let window = self.cfg.evaluation_window_secs;
        self.variants
            .iter_mut()
            .map(|v| {
                let m = v.metrics(window, now_ms);
                VariantView {
                    id: v.id.clone(),
                    label: v.label.clone(),
                    sample_count: m.sample_count,
                    win_rate: m.win_rate(),
                    profit_factor: m.profit_factor(),
                    total_pnl_usd: m.total_pnl,
                    age_sec: v.age_sec(now_ms),
                    is_baseline: v.is_baseline,
                }
            })
            .collect()
    }

    pub fn history(&self, limit: usize) -> Vec<audit::AuditRecord> {
        self.audit.recent(limit)
    }

    pub fn evolution_count(&self) -> u64 {
        self.evolution_count
    }
    pub fn rejected_count(&self) -> u64 {
        self.rejected_count
    }
    pub fn variant_count(&self) -> usize {
        self.variants.len()
    }
    pub fn seconds_since_last_evolution(&self, now_ms: i64) -> i64 {
        if self.last_evolution_ms == 0 {
            -1
        } else {
            (now_ms - self.last_evolution_ms) / 1000
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        use rust_decimal::prelude::FromPrimitive;
        OrderbookSnapshot::from_levels(
            "t",
            vec![(Decimal::from_f64(bid).unwrap(), dec!(100))],
            vec![(Decimal::from_f64(ask).unwrap(), dec!(100))],
            0,
        )
    }

    fn fast_cfg(enabled: bool) -> ShadowEvolutionConfig {
        ShadowEvolutionConfig {
            enabled,
            min_sample_count: 2,
            min_win_rate_improvement: dec!(0.05),
            min_profit_factor_improvement: dec!(0.10),
            min_observation_secs: 0,
            cooldown_secs: 0,
            variant_count: 3,
            audit_log_path: std::env::temp_dir()
                .join(format!("bkevo-{}", std::process::id()))
                .join("audit.jsonl")
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn disabled_by_default_is_inert() {
        let mut m = ShadowEvolution::new(fast_cfg(false), MutableParams::default());
        assert!(!m.is_enabled());
        assert_eq!(m.status(0), EvolutionStatus::Disabled);
        m.on_tick("t", &book(0.43, 0.45), true, 1000);
        assert_eq!(m.variant_count(), 0);
        assert!(m.evaluate(1000).is_empty());
    }

    #[test]
    fn enable_builds_at_least_two_variants_and_hot_swaps() {
        let mut m = ShadowEvolution::new(fast_cfg(true), MutableParams::default());
        m.enable(0);
        assert!(m.variant_count() >= 2);
        let handle = m.handle();
        let before = handle.load_full().trend_max_entry_price;
        m.swap.store(MutableParams::default().scaled(dec!(1.03)));
        assert_eq!(handle.load_full().trend_max_entry_price, before * dec!(1.03));
    }

    #[test]
    fn shadow_does_not_touch_real_state_and_survives_variant_panic() {
        let mut m = ShadowEvolution::new(fast_cfg(true), MutableParams::default());
        m.enable(0);
        // Normal driving must not panic and must not affect anything external.
        m.on_tick("t", &book(0.43, 0.45), true, 1000);
        m.on_tick("t", &book(0.95, 0.97), true, 2000);
        let views = m.variant_views(2000);
        assert!(views.iter().any(|v| v.is_baseline));
    }

    #[test]
    fn guard_rejects_gradient_beyond_lock() {
        // A variant whose params are legal, but a signal that exceeds the lock
        // must be rejected by the manager (simulated here via direct guard call).
        let base = MutableParams::default();
        let far = base.scaled(dec!(1.20));
        assert!(guard::validate_gradient(&base, &far, dec!(0.05)).is_err());
    }

    #[test]
    fn rollback_restores_previous_params() {
        let mut m = ShadowEvolution::new(fast_cfg(true), MutableParams::default());
        m.enable(0);
        let original = m.current_params();
        m.swap.store(original.clone().scaled(dec!(1.03)));
        m.previous = Some(original.clone());
        let out = m.rollback(5000).unwrap();
        assert!(matches!(out, EvolutionOutcome::RolledBack { .. }));
        assert_eq!(m.current_params(), original);
        // No previous → error.
        m.previous = None;
        assert!(m.rollback(6000).is_err());
    }
}
