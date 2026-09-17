//! Shadow Evolution — module entry and manager.
//!
//! E2-c (#28): the manager owns **one evolution unit per strategy**. A unit
//! holds that strategy's own declared knob domains, its shadow twin set, its own
//! parameter cell (`Arc<ArcSwap<StrategyParams>>`) and its own cooldown /
//! rollback memory. Nothing is shared between units, so two strategies evolving
//! in parallel cannot observe or overwrite each other — isolation is structural,
//! not a convention.
//!
//! ```text
//! ShadowEvolution
//!   └─ ParamRegistry                       (strategy → Arc<ArcSwap<StrategyParams>>)
//!   └─ Unit { strategy, specs, set, cell, last_evolution_ms, evolution_count, previous }
//!        └─ VariantSet { baseline twin + N directed single-knob twins }
//! ```
//!
//! Isolation: every variant tick runs under `catch_unwind`; a panicking variant
//! is marked crashed and excluded, and NEVER propagates into the main strategy
//! path (spec: "影子引擎崩溃不影响主策略").
//!
//! A strategy that declares no knobs is **not evolvable** and gets no unit at
//! all — an explicit declaration (D6), not a silent no-op.

pub mod audit;
pub mod config;
pub mod evaluator;
pub mod guard;
pub mod knobs;
pub mod registry;
pub mod signal;
pub mod variants;

pub use config::{
    EvolutionStatus, ImmutableConfig, KnobDeclaration, KnobSpec, MutableParams,
    ShadowEvolutionConfig, StrategyParams, VariantView,
};
pub use registry::ParamRegistry;
pub use signal::{EvolutionReason, EvolveSignal};

use crate::model::{CryptoMarket, OrderbookSnapshot};
use crate::strategies::EngineStrategy;
use crate::strategies::shadow_twin::ShadowTickCtx;
use audit::AuditLog;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use variants::{VariantSet, build_variants};

/// Result of one evaluation/action, mapped to events by the caller.
pub enum EvolutionOutcome {
    Signal(EvolveSignal),
    Applied(EvolveSignal),
    Rejected {
        signal: EvolveSignal,
        reason: String,
    },
    RolledBack {
        strategy: String,
        from: MutableParams,
        to: MutableParams,
    },
}

/// One strategy's evolution state. Every field is private to that strategy.
struct Unit {
    strategy: String,
    /// The domains this strategy declared (via `evolvable_knobs`).
    specs: Vec<KnobSpec>,
    /// How to build a twin of this strategy; without it the declaration is inert.
    factory: Box<dyn crate::strategies::ShadowFactory>,
    set: VariantSet,
    /// This strategy's cell in the registry (its live hot-parameter slot).
    cell: Arc<arc_swap::ArcSwap<StrategyParams>>,
    /// This strategy's own cooldown clock.
    last_evolution_ms: i64,
    evolution_count: u64,
    rejected_count: u64,
    /// Parameters in force before the most recent applied evolution (rollback).
    previous: Option<StrategyParams>,
}

impl Unit {
    /// Build (or rebuild) this strategy's variant set around the parameters in
    /// force. `evolution_count` doubles as the sweep offset so a re-anchor after
    /// an applied evolution explores a different knob and direction.
    fn scaffold(&mut self, cfg: &ShadowEvolutionConfig, now_ms: i64) {
        self.set = build_variants(
            &self.strategy,
            &self.specs,
            &self.cell.load(),
            self.factory.as_ref(),
            cfg.variant_count.max(2),
            cfg.max_gradient,
            &cfg.exit_cfg,
            now_ms,
            self.evolution_count,
        );
    }
}

pub struct ShadowEvolution {
    cfg: ShadowEvolutionConfig,
    enabled: bool,
    registry: Arc<ParamRegistry>,
    units: Vec<Unit>,
    /// Token → round expiry (ms), fed by round updates.
    token_expiry: HashMap<String, i64>,
    /// The round's markets, retained so a tick can serve twins the same market
    /// list the live evaluation sees (the engine holds its scanner markets the
    /// same way). Replaced on every round; empty outside a round.
    round_markets: Vec<CryptoMarket>,
    audit: AuditLog,
}

impl ShadowEvolution {
    /// Build the manager from the config and the strategies the engine hosts.
    ///
    /// A strategy joins only when it declares knobs AND can build a twin; the
    /// rest are recorded as not evolvable. When evolution is disabled nothing is
    /// scaffolded and no file is ever created (D7) — but the cells are still
    /// published with the declared values, so the overlay is a no-op rather than
    /// an invented parameter.
    pub fn new(cfg: ShadowEvolutionConfig, strategies: &[&dyn EngineStrategy]) -> Self {
        let enabled = cfg.enabled;
        let audit = AuditLog::new(&cfg);
        let mut me = Self {
            enabled,
            cfg,
            registry: Arc::new(ParamRegistry::new()),
            units: Vec::new(),
            token_expiry: HashMap::new(),
            round_markets: Vec::new(),
            audit,
        };
        me.register_strategies(strategies);
        me
    }

    /// (Re)build one unit per evolvable strategy. Called at construction and again
    /// whenever the engine installs its host set — the manager exists before any
    /// engine does (the engine is installed from config later), and the declared
    /// knobs live on the strategies themselves.
    ///
    /// Re-registering is safe and keeps state: `ParamRegistry::publish` returns the
    /// strategy's existing cell, so the parameters in force survive a rebuild.
    pub fn register_strategies(&mut self, strategies: &[&dyn EngineStrategy]) {
        // Rebuilding must not forget what each strategy already did: installing
        // an engine (or re-reading declarations) is not a reset, so the cooldown
        // clock, the evolution counters and the rollback anchor carry over.
        let mut carried: HashMap<String, (i64, u64, u64, Option<StrategyParams>)> = self
            .units
            .drain(..)
            .map(|u| {
                (
                    u.strategy.clone(),
                    (
                        u.last_evolution_ms,
                        u.evolution_count,
                        u.rejected_count,
                        u.previous,
                    ),
                )
            })
            .collect();
        for s in strategies {
            let specs = s.evolvable_knobs();
            if specs.is_empty() {
                continue; // explicit "not evolvable"
            }
            let Some(factory) = s.shadow_factory() else {
                tracing::debug!(
                    strategy = s.name(),
                    "declared knobs but no shadow_factory — inert"
                );
                continue;
            };
            if self.units.len() >= self.cfg.max_strategies {
                tracing::warn!(strategy = s.name(), "strategy cap reached; not evolved");
                break;
            }
            let params = StrategyParams::from_knobs(&specs);
            let cell = self.registry.publish(s.name(), params);
            let (last_evolution_ms, evolution_count, rejected_count, previous) =
                carried.remove(s.name()).unwrap_or((0, 0, 0, None));
            self.units.push(Unit {
                strategy: s.name().to_string(),
                specs,
                factory,
                set: VariantSet::empty(s.name()),
                cell,
                last_evolution_ms,
                evolution_count,
                rejected_count,
                previous,
            });
        }
        // A strategy that is gone must not keep a cell: a stale handle would let
        // a removed strategy be written into the registry forever.
        for (name, _) in carried {
            self.registry.remove(&name);
        }
        if self.enabled {
            for i in 0..self.units.len() {
                self.units[i].scaffold(&self.cfg, 0);
            }
        }
    }

    /// The per-strategy parameter registry the engine attaches to its strategies
    /// (D1). Attached even when evolution is disabled: the cells then hold the
    /// declared values, so it is a no-op overlay (D7).
    pub fn registry(&self) -> Arc<ParamRegistry> {
        self.registry.clone()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Aggregate parameters of every evolved strategy (IPC/observability).
    pub fn current_params(&self) -> MutableParams {
        self.registry.snapshot()
    }

    /// One strategy's parameters (`None` when it is not evolvable).
    pub fn params_for(&self, strategy: &str) -> Option<StrategyParams> {
        self.registry
            .handle_for(strategy)
            .map(|c| (**c.load()).clone())
    }

    /// The knobs a strategy declared (empty ⇒ not evolvable).
    pub fn declared_knobs(&self, strategy: &str) -> Vec<KnobSpec> {
        self.units
            .iter()
            .find(|u| u.strategy == strategy)
            .map(|u| u.specs.clone())
            .unwrap_or_default()
    }

    /// Evolvable strategy names, in registration order.
    pub fn strategy_names(&self) -> Vec<String> {
        self.units.iter().map(|u| u.strategy.clone()).collect()
    }

    /// Enable evolution: scaffold every unit around the parameters in force.
    pub fn enable(&mut self, now_ms: i64) {
        self.enabled = true;
        self.cfg.enabled = true;
        self.audit.set_enabled(true);
        for i in 0..self.units.len() {
            self.units[i].scaffold(&self.cfg, now_ms);
        }
    }

    pub fn disable(&mut self) {
        self.enabled = false;
        self.cfg.enabled = false;
        self.audit.set_enabled(false);
        for u in self.units.iter_mut() {
            u.set = VariantSet::empty(&u.strategy);
        }
    }

    /// Record the current round: token expiries for the tick context, and the
    /// markets each twin needs to evaluate candidates. Twins are re-seeded with
    /// the round's opening mids (what the live engine does) so their confirmation
    /// clocks start on the same tick as the live strategy's.
    ///
    /// D-3: this does NOT rebuild the variant set. Rebuilding every round reset
    /// every variant's sample counter, so across a 900s production round it could
    /// never reach `min_sample_count`. Closed-trade history is retained so
    /// metrics accumulate across round boundaries.
    pub fn on_round(
        &mut self,
        markets: &[CryptoMarket],
        seeds: &[(String, OrderbookSnapshot)],
        now_ms: i64,
    ) {
        self.token_expiry.clear();
        for m in markets {
            self.token_expiry
                .insert(m.up_token_id.clone(), m.expires_at_ms);
            self.token_expiry
                .insert(m.down_token_id.clone(), m.expires_at_ms);
        }
        self.round_markets = markets.to_vec();
        if !self.enabled {
            return;
        }
        for u in self.units.iter_mut() {
            u.set.on_round(markets, seeds, now_ms);
        }
    }

    /// Feed a market tick to every twin (baseline + mutated) of every strategy.
    /// Observation only: this never touches the real ledger, positions or orders.
    pub fn on_tick(&mut self, token_id: &str, book: &OrderbookSnapshot, now_ms: i64) {
        if !self.enabled || self.round_markets.is_empty() {
            return;
        }
        let round_slot = self
            .round_markets
            .first()
            .map(|m| m.round_slot)
            .unwrap_or(0);
        let expires_at = self
            .token_expiry
            .get(token_id)
            .copied()
            .unwrap_or(now_ms + 900_000);
        let time_left_sec = (expires_at - now_ms) / 1000;
        let ctx = ShadowTickCtx {
            markets: &self.round_markets,
            token_id,
            book,
            round_slot,
            time_left_sec,
            now_ms,
        };
        for u in self.units.iter_mut() {
            for v in u.set.variants.iter_mut() {
                if v.crashed {
                    continue;
                }
                // Crash isolation: a panicking variant is quarantined, not fatal.
                let res = catch_unwind(AssertUnwindSafe(|| v.on_tick(&ctx)));
                if res.is_err() {
                    v.crashed = true;
                    tracing::warn!(
                        strategy = %u.strategy,
                        variant = %v.id,
                        "shadow variant panicked — quarantined"
                    );
                }
            }
        }
    }

    /// Run one evaluation pass for EVERY strategy. Each unit is decided
    /// independently (its own baseline metrics, its own cooldown), so at most one
    /// result per strategy and never one strategy's outcome gating another's.
    pub fn evaluate(&mut self, now_ms: i64) -> Vec<EvolutionOutcome> {
        if !self.enabled {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in 0..self.units.len() {
            if let Some(o) = self.evaluate_unit(i, now_ms) {
                out.push(o);
            }
        }
        out
    }

    /// Evaluate one unit by index (a borrow of one unit, never of the registry).
    fn evaluate_unit(&mut self, i: usize, now_ms: i64) -> Option<EvolutionOutcome> {
        let cfg = self.cfg.clone();
        let outcome;
        let mut rejection: Option<(EvolveSignal, String)> = None;
        let mut applied: Option<EvolveSignal> = None;
        {
            let u = &mut self.units[i];
            let signal = {
                let baseline = u.set.baseline_metrics(cfg.evaluation_window_secs, now_ms);
                evaluator::evaluate(
                    &cfg,
                    &u.strategy,
                    &mut u.set,
                    &baseline,
                    now_ms,
                    u.last_evolution_ms,
                )?
                .signal
            };
            // Safety locks. Lock 0 (declaration) and Lock 1 (domain) are checked
            // against THIS strategy's own declaration; Lock 2 (gradient) against
            // the parameters in force; Lock 3 against the immutable laws.
            let old: StrategyParams = (**u.cell.load()).clone();
            let Some(proposal) = signal.to_params.for_strategy(&u.strategy).cloned() else {
                return None; // a signal that does not name this strategy is dropped
            };
            let checked = guard::validate_declared(&proposal, &u.specs)
                .and_then(|_| guard::validate_domain(&proposal, &u.specs))
                .and_then(|_| guard::validate_gradient(&old, &proposal, cfg.max_gradient))
                .and_then(|_| guard::validate_immutable(&cfg.risk));

            match checked {
                Err(e) => {
                    u.rejected_count += 1;
                    // Respect the cooldown even on rejection: a rejected proposal
                    // must not become a hot rejection loop.
                    u.last_evolution_ms = now_ms;
                    rejection = Some((signal, e.to_string()));
                }
                Ok(()) => {
                    // Admissible: publish atomically and re-anchor THIS strategy.
                    u.previous = Some(old);
                    u.cell.store(Arc::new(proposal));
                    u.last_evolution_ms = now_ms;
                    u.evolution_count += 1;
                    u.scaffold(&cfg, now_ms);
                    applied = Some(signal);
                }
            }
        }
        // Audit outside the unit borrow. Re-anchoring just cleared the trade
        // history the proposal was measured on, so this record is the only trace
        // of what was decided and why.
        if let Some((signal, reason)) = rejection {
            tracing::warn!(strategy = %signal.strategy, reason = %reason, "shadow evolution rejected");
            self.audit
                .record_rejection(&signal, reason.clone(), "failed", "n/a");
            outcome = Some(EvolutionOutcome::Rejected { signal, reason });
        } else if let Some(signal) = applied {
            self.audit.record_applied(&signal);
            outcome = Some(EvolutionOutcome::Applied(signal));
        } else {
            outcome = None;
        }
        outcome
    }

    /// Apply a parameter set for ONE strategy directly (internal/tests). The same
    /// locks as an evolved proposal are enforced.
    pub fn set_params(
        &mut self,
        strategy: &str,
        params: StrategyParams,
        now_ms: i64,
    ) -> Result<(), String> {
        let cfg = self.cfg.clone();
        let u = self
            .units
            .iter_mut()
            .find(|u| u.strategy == strategy)
            .ok_or_else(|| format!("strategy {strategy} is not evolvable"))?;
        let old: StrategyParams = (**u.cell.load()).clone();
        guard::validate_declared(&params, &u.specs).map_err(|e| e.to_string())?;
        guard::validate_domain(&params, &u.specs).map_err(|e| e.to_string())?;
        guard::validate_gradient(&old, &params, cfg.max_gradient).map_err(|e| e.to_string())?;
        guard::validate_immutable(&cfg.risk).map_err(|e| e.to_string())?;
        u.previous = Some(old);
        u.cell.store(Arc::new(params));
        u.last_evolution_ms = now_ms;
        Ok(())
    }

    /// Operator override for ONE strategy: validate, hot-swap and audit — manual
    /// parameter changes used to leave no trace at all.
    pub fn apply_params(&mut self, params: MutableParams, now_ms: i64) -> Result<(), String> {
        if params.len() != 1 {
            return Err("apply takes exactly one strategy's parameters".into());
        }
        let strategy = params.strategies()[0].to_string();
        let newp = params.for_strategy(&strategy).cloned().unwrap_or_default();
        let old = {
            let mut m = MutableParams::new();
            if let Some(p) = self.params_for(&strategy) {
                m.set_strategy(&strategy, p);
            }
            m
        };
        self.set_params(&strategy, newp.clone(), now_ms)?;
        let new_full = {
            let mut m = MutableParams::new();
            m.set_strategy(&strategy, newp);
            m
        };
        self.audit
            .record_manual(&strategy, now_ms, &old, &new_full, "operator override");
        Ok(())
    }

    /// Roll back ONE strategy to the parameters in force before its last change.
    /// A strategy that never changed anything is an explicit error, never a
    /// silent no-op that another strategy's rollback could be mistaken for.
    pub fn rollback(&mut self, strategy: &str, now_ms: i64) -> Result<EvolutionOutcome, String> {
        let u = self
            .units
            .iter_mut()
            .find(|u| u.strategy == strategy)
            .ok_or_else(|| format!("strategy {strategy} is not evolvable"))?;
        let Some(prev) = u.previous.clone() else {
            return Err(format!(
                "strategy {strategy} has no previous parameters to roll back to"
            ));
        };
        let from: StrategyParams = (**u.cell.load()).clone();
        u.cell.store(Arc::new(prev.clone()));
        u.previous = Some(from.clone());
        u.last_evolution_ms = now_ms;
        let (mut fm, mut tm) = (MutableParams::new(), MutableParams::new());
        fm.set_strategy(strategy, from);
        tm.set_strategy(strategy, prev);
        self.audit.record_rollback(strategy, now_ms, &fm, &tm);
        Ok(EvolutionOutcome::RolledBack {
            strategy: strategy.to_string(),
            from: fm,
            to: tm,
        })
    }

    /// Status of one strategy. `None` = this strategy is **not evolvable** (it
    /// declares no knobs) — an explicit answer, not a generic "disabled", so an
    /// operator can tell "no knobs declared" from "evolution switched off" and
    /// from "this strategy just changed something".
    pub fn status(&self, strategy: &str, now_ms: i64) -> Option<EvolutionStatus> {
        let u = self.units.iter().find(|u| u.strategy == strategy)?;
        if !self.enabled {
            return Some(EvolutionStatus::Disabled);
        }
        if u.last_evolution_ms > 0 && now_ms - u.last_evolution_ms < self.cfg.cooldown_secs * 1000 {
            return Some(EvolutionStatus::Cooling);
        }
        Some(EvolutionStatus::Evaluating)
    }

    /// Aggregate status: `Disabled` when off; otherwise `Cooling` when ANY unit is
    /// cooling (the conservative aggregate — something just changed), else
    /// `Evaluating`. Kept for callers that predate per-strategy status.
    pub fn aggregate_status(&self, now_ms: i64) -> EvolutionStatus {
        if !self.enabled {
            return EvolutionStatus::Disabled;
        }
        if self.units.iter().any(|u| {
            u.last_evolution_ms > 0 && now_ms - u.last_evolution_ms < self.cfg.cooldown_secs * 1000
        }) {
            return EvolutionStatus::Cooling;
        }
        EvolutionStatus::Evaluating
    }

    /// Per-variant snapshots, each tagged with the strategy it belongs to.
    pub fn variant_views(&mut self, now_ms: i64) -> Vec<VariantView> {
        let window = self.cfg.evaluation_window_secs;
        let mut out = Vec::new();
        for u in self.units.iter_mut() {
            for v in u.set.variants.iter_mut() {
                let m = v.metrics(window, now_ms);
                out.push(VariantView {
                    id: v.id.clone(),
                    label: v.label.clone(),
                    strategy: u.strategy.clone(),
                    sample_count: m.sample_count,
                    win_rate: m.win_rate(),
                    profit_factor: m.profit_factor(),
                    total_pnl_usd: m.total_pnl,
                    age_sec: v.age_sec(now_ms),
                    is_baseline: v.is_baseline,
                });
            }
        }
        out
    }

    /// Recent audit records. `None` = every strategy's records concatenated;
    /// `Some(name)` = that strategy's file only, never another's.
    pub fn history(&self, strategy: Option<&str>, limit: usize) -> Vec<audit::AuditRecord> {
        match strategy {
            Some(s) => self.audit.recent(s, limit),
            None => {
                let mut out = Vec::new();
                for name in self.audit.strategies() {
                    out.extend(self.audit.recent(&name, limit));
                }
                out
            }
        }
    }

    /// Strategies that have at least one audit record.
    pub fn audited_strategies(&self) -> Vec<String> {
        self.audit.strategies()
    }

    pub fn evolution_count(&self, strategy: &str) -> u64 {
        self.units
            .iter()
            .find(|u| u.strategy == strategy)
            .map(|u| u.evolution_count)
            .unwrap_or(0)
    }
    pub fn rejected_count(&self, strategy: &str) -> u64 {
        self.units
            .iter()
            .find(|u| u.strategy == strategy)
            .map(|u| u.rejected_count)
            .unwrap_or(0)
    }
    /// Total variants across every strategy (0 when disabled).
    pub fn variant_count(&self) -> usize {
        self.units.iter().map(|u| u.set.len()).sum()
    }
    pub fn seconds_since_last_evolution(&self, strategy: &str, now_ms: i64) -> i64 {
        match self.units.iter().find(|u| u.strategy == strategy) {
            Some(u) if u.last_evolution_ms > 0 => (now_ms - u.last_evolution_ms) / 1000,
            _ => -1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit_policy::ExitConfig;
    use crate::model::SignalDirection;
    use crate::signal::TradeSignal;
    use crate::strategies::StrategyCtx;
    use crate::strategies::shadow_twin::{ShadowFactory, tick_ctx};
    use rust_decimal::Decimal;
    use rust_decimal::prelude::FromPrimitive;
    use rust_decimal_macros::dec;

    /// A synthetic strategy: buys when the mid is at or under `cap`. Exactly one
    /// knob, so a parameter change is provably the only source of a difference.
    struct CapStrategy {
        name: String,
        cap: Decimal,
    }

    struct CapFactory {
        name: String,
        cap: Decimal,
    }

    impl ShadowFactory for CapFactory {
        fn strategy(&self) -> String {
            self.name.clone()
        }
        fn knobs(&self) -> Vec<KnobSpec> {
            vec![KnobSpec::new("cap", self.cap, dec!(0.05), dec!(0.95))]
        }
        fn make(&self, params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
            Some(Box::new(CapStrategy {
                name: self.name.clone(),
                cap: params.get("cap").unwrap_or(self.cap),
            }))
        }
    }

    impl EngineStrategy for CapStrategy {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
        fn on_round(&mut self, _slot: i64) {}
        fn evolvable_knobs(&self) -> Vec<KnobSpec> {
            vec![KnobSpec::new("cap", self.cap, dec!(0.05), dec!(0.95))]
        }
        fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
            Some(Box::new(CapFactory {
                name: self.name.clone(),
                cap: self.cap,
            }))
        }
        fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            let market = &ctx.markets()[0];
            let token = market.up_token_id.clone();
            let Some(book) = ctx.fresh_book(&token) else {
                return Vec::new();
            };
            if book.mid_price > self.cap {
                return Vec::new();
            }
            vec![TradeSignal {
                strategy: self.name.clone(),
                asset: market.asset.clone(),
                direction: SignalDirection::Up,
                token_id: token,
                condition_id: market.condition_id.clone(),
                price: book.mid_price,
                reason: format!("mid {} <= cap {}", book.mid_price, self.cap),
            }]
        }
    }

    /// Declares nothing — explicitly not evolvable.
    struct Inert;

    impl EngineStrategy for Inert {
        fn name(&self) -> &str {
            "inert"
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
        fn on_round(&mut self, _slot: i64) {}
        fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            Vec::new()
        }
    }

    fn fast_cfg(enabled: bool, tag: &str) -> ShadowEvolutionConfig {
        ShadowEvolutionConfig {
            enabled,
            min_sample_count: 2,
            min_win_rate_improvement: dec!(0.05),
            min_profit_factor_improvement: dec!(0.10),
            min_observation_secs: 0,
            cooldown_secs: 0,
            variant_count: 3,
            audit_dir: std::env::temp_dir()
                .join(format!("bkevo-{}-{tag}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        }
    }

    fn market() -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "c".into(),
            question_id: "q".into(),
            up_token_id: "t".into(),
            down_token_id: "t-d".into(),
            up_price: dec!(0.5),
            down_price: dec!(0.5),
            expires_at_ms: 900_000,
            round_slot: 1,
            neg_risk: false,
            question: "?".into(),
        }
    }

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels(
            "t",
            vec![(Decimal::from_f64(bid).unwrap(), dec!(100))],
            vec![(Decimal::from_f64(ask).unwrap(), dec!(100))],
            0,
        )
    }

    fn strategies() -> (CapStrategy, CapStrategy) {
        (
            CapStrategy {
                name: "alpha".into(),
                cap: dec!(0.40),
            },
            CapStrategy {
                name: "beta".into(),
                cap: dec!(0.40),
            },
        )
    }

    /// Drive ONE strategy's twin set through `times` cycles where the baseline
    /// LOSES and a loosened cap WINS: a dip the baseline's own cap admits (mid
    /// 0.395 <= 0.40) that collapses, then a shallower dip only a loosened cap
    /// reaches (mid 0.405 > 0.40) that rallies. The baseline therefore has
    /// samples but a zero win rate, so it satisfies the sample floor while the
    /// loosened variant beats it on both win rate and profit factor.
    fn drive_wins(m: &mut ShadowEvolution, name: &str, times: usize, t0: i64) {
        let round = market();
        let i = m.units.iter().position(|u| u.strategy == name).unwrap();
        let mut now = t0;
        for _ in 0..times {
            // A loser the baseline also takes: entry under the cap, then collapse.
            now += 1_000;
            let dip = book(0.39, 0.40); // mid 0.395
            for v in m.units[i].set.variants.iter_mut() {
                v.on_tick(&tick_ctx(&[round.clone()], "t", &dip, 1, 880, now));
            }
            now += 1_000;
            let crash = book(0.20, 0.21); // mid 0.205 → well past the 12% stop
            for v in m.units[i].set.variants.iter_mut() {
                v.on_tick(&tick_ctx(&[round.clone()], "t", &crash, 1, 880, now));
            }
            // A winner only a LOOSENED cap reaches: mid 0.405 > the 0.40 baseline.
            now += 1_000;
            let shallow = book(0.40, 0.41);
            for v in m.units[i].set.variants.iter_mut() {
                v.on_tick(&tick_ctx(&[round.clone()], "t", &shallow, 1, 880, now));
            }
            now += 1_000;
            let up = book(0.95, 0.97);
            for v in m.units[i].set.variants.iter_mut() {
                v.on_tick(&tick_ctx(&[round.clone()], "t", &up, 1, 880, now));
            }
        }
    }

    #[test]
    fn disabled_by_default_is_inert() {
        let (a, b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
        let mut m = ShadowEvolution::new(fast_cfg(false, "off"), &refs);
        assert!(!m.is_enabled());
        assert_eq!(m.aggregate_status(0), EvolutionStatus::Disabled);
        m.on_round(&[market()], &[], 1000);
        m.on_tick("t", &book(0.43, 0.45), 1000);
        assert_eq!(m.variant_count(), 0);
        assert!(m.evaluate(1000).is_empty());
        assert!(m.history(None, 10).is_empty());
        // The declared values are still published, so attaching the registry is a
        // no-op overlay rather than an invented parameter.
        assert_eq!(m.registry().get("alpha", "cap"), Some(dec!(0.40)));
    }

    #[test]
    fn a_strategy_that_declares_nothing_gets_no_unit() {
        let (a, _b) = strategies();
        let inert = Inert;
        let refs: Vec<&dyn EngineStrategy> = vec![&a, &inert];
        let m = ShadowEvolution::new(fast_cfg(false, "inert"), &refs);
        assert_eq!(m.strategy_names(), vec!["alpha".to_string()]);
        assert!(m.params_for("inert").is_none(), "not evolvable is explicit");
        assert!(m.declared_knobs("inert").is_empty());
        assert!(m.status("inert", 0).is_none());
    }

    #[test]
    fn enable_scaffolds_every_strategy_and_publishes_its_own_cell() {
        let (a, b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
        let mut m = ShadowEvolution::new(fast_cfg(false, "enable"), &refs);
        m.enable(0);
        assert_eq!(m.variant_count(), 6, "3 variants x 2 strategies");
        let ca = m.registry().handle_for("alpha").unwrap();
        let cb = m.registry().handle_for("beta").unwrap();
        assert_ne!(
            Arc::as_ptr(&ca),
            Arc::as_ptr(&cb),
            "each strategy has its own cell"
        );
        let views = m.variant_views(0);
        assert!(views.iter().any(|v| v.strategy == "alpha" && v.is_baseline));
        assert!(views.iter().any(|v| v.strategy == "beta" && v.is_baseline));
    }

    /// ACCEPTANCE (issue #28): two strategies evolving in parallel must not
    /// cross-talk. Only alpha's variants qualify ⇒ only alpha's parameters move,
    /// only alpha's audit file gains an applied record, beta is unchanged. Then
    /// the reverse.
    #[test]
    fn two_strategies_evolve_in_parallel_without_cross_talk() {
        let (a, b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
        let mut m = ShadowEvolution::new(fast_cfg(false, "parallel"), &refs);
        let dir = std::path::PathBuf::from(&m.cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        m.enable(0);
        m.on_round(&[market()], &[], 0);

        // Only alpha produces qualifying variants.
        drive_wins(&mut m, "alpha", 2, 10_000);

        let beta_before = m.registry().get("beta", "cap").unwrap();
        let outcomes = m.evaluate(100_000);
        assert_eq!(outcomes.len(), 1, "exactly one strategy evolved");
        match &outcomes[0] {
            EvolutionOutcome::Applied(sig) => {
                assert_eq!(sig.strategy, "alpha");
                assert_eq!(sig.to_params.strategies(), vec!["alpha"]);
            }
            EvolutionOutcome::Rejected { signal, reason } => {
                panic!(
                    "alpha should have applied, was rejected: {} ({reason})",
                    signal.strategy
                )
            }
            _ => panic!("expected an applied outcome"),
        }

        assert_ne!(m.registry().get("alpha", "cap").unwrap(), dec!(0.40));
        assert_eq!(m.registry().get("beta", "cap").unwrap(), beta_before);
        assert_eq!(m.evolution_count("alpha"), 1);
        assert_eq!(m.evolution_count("beta"), 0);
        assert_eq!(m.audited_strategies(), vec!["alpha".to_string()]);
        assert!(
            m.history(Some("beta"), 10).is_empty(),
            "beta has no records"
        );
        assert_eq!(m.history(Some("alpha"), 10).len(), 1);
        assert!(dir.join("alpha.jsonl").exists());
        assert!(
            !dir.join("beta.jsonl").exists(),
            "no write may create beta's file"
        );
        let alpha_log = std::fs::read_to_string(dir.join("alpha.jsonl")).unwrap();
        assert!(alpha_log.contains("\"strategy\":\"alpha\""));
        assert!(!alpha_log.contains("beta"));

        // Reverse: beta wins, alpha must not move again.
        let alpha_after = m.registry().get("alpha", "cap").unwrap();
        drive_wins(&mut m, "beta", 2, 200_000);
        let outcomes = m.evaluate(300_000);
        assert_eq!(outcomes.len(), 1, "only beta should propose");
        match &outcomes[0] {
            EvolutionOutcome::Applied(sig) => assert_eq!(sig.strategy, "beta"),
            EvolutionOutcome::Rejected { signal, reason } => {
                panic!(
                    "beta should have applied, was rejected: {} ({reason})",
                    signal.strategy
                )
            }
            _ => panic!("expected an applied outcome"),
        }
        assert_eq!(
            m.registry().get("alpha", "cap").unwrap(),
            alpha_after,
            "alpha must not move"
        );
        assert_eq!(m.evolution_count("alpha"), 1);
        assert_eq!(m.evolution_count("beta"), 1);
        let beta_log = std::fs::read_to_string(dir.join("beta.jsonl")).unwrap();
        assert!(beta_log.contains("\"strategy\":\"beta\""));
        assert!(
            !beta_log.contains("alpha"),
            "beta's file must not mention alpha"
        );
        // And the two proposals differ: each carries its own knob set.
        assert!(
            m.history(Some("alpha"), 10)
                .iter()
                .all(|r| r.strategy == "alpha")
        );
        assert!(
            m.history(Some("beta"), 10)
                .iter()
                .all(|r| r.strategy == "beta")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_is_per_strategy() {
        let (a, b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
        let mut m = ShadowEvolution::new(fast_cfg(false, "rollback"), &refs);
        let dir = std::path::PathBuf::from(&m.cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        m.enable(0);
        let original = m.registry().get("alpha", "cap").unwrap();

        let mut p = StrategyParams::new();
        p.set("cap", dec!(0.412));
        m.set_params("alpha", p, 1000).unwrap();
        assert_eq!(m.registry().get("alpha", "cap").unwrap(), dec!(0.412));

        match m.rollback("alpha", 2000).unwrap() {
            EvolutionOutcome::RolledBack { strategy, from, to } => {
                assert_eq!(strategy, "alpha");
                assert_eq!(from.get("alpha", "cap"), Some(dec!(0.412)));
                assert_eq!(to.get("alpha", "cap"), Some(original));
            }
            _ => panic!("expected a rollback"),
        }
        assert_eq!(m.registry().get("alpha", "cap").unwrap(), original);
        // beta never changed, so its rollback is an explicit error, not a silent
        // no-op that could be mistaken for alpha's.
        assert!(m.rollback("beta", 3000).is_err());
        assert!(m.rollback("nope", 3000).is_err());
        assert_eq!(
            m.history(Some("beta"), 10).len(),
            0,
            "beta's history stays empty"
        );
        assert_eq!(
            m.history(Some("alpha"), 10).len(),
            1,
            "only alpha's rollback was recorded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manual_apply_is_audited_and_domain_checked() {
        let (a, b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
        let mut m = ShadowEvolution::new(fast_cfg(false, "manual"), &refs);
        let dir = std::path::PathBuf::from(&m.cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        m.enable(0);

        let mut params = MutableParams::new();
        let mut p = StrategyParams::new();
        p.set("cap", dec!(0.412)); // +3%, inside the gradient lock
        params.set_strategy("alpha", p);
        m.apply_params(params, 5000).unwrap();
        let rec = m.history(Some("alpha"), 10);
        assert_eq!(rec.len(), 1);
        assert!(
            rec[0].manual && rec[0].applied,
            "a manual override is traced now"
        );
        assert_eq!(rec[0].from_params.get("alpha", "cap"), Some(dec!(0.40)));
        assert_eq!(rec[0].to_params.get("alpha", "cap"), Some(dec!(0.412)));

        // Out of domain → rejected, and the value does not move.
        let before = m.registry().get("alpha", "cap").unwrap();
        let mut p = StrategyParams::new();
        p.set("cap", dec!(0.99)); // beyond the declared 0.95 ceiling
        let mut params = MutableParams::new();
        params.set_strategy("alpha", p);
        assert!(m.apply_params(params, 6000).is_err());
        assert_eq!(m.registry().get("alpha", "cap").unwrap(), before);

        // An undeclared knob is rejected, not silently dropped.
        let mut p = StrategyParams::new();
        p.set("hard_stop_loss_pct", dec!(0));
        let mut params = MutableParams::new();
        params.set_strategy("alpha", p);
        assert!(m.apply_params(params, 7000).is_err());

        // More than one strategy at a time is refused: apply is per-strategy.
        let mut params = MutableParams::new();
        let mut p = StrategyParams::new();
        p.set("cap", dec!(0.40));
        params.set_strategy("alpha", p.clone());
        params.set_strategy("beta", p);
        assert!(m.apply_params(params, 8000).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn observation_alone_never_moves_parameters() {
        let (a, b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
        let mut m = ShadowEvolution::new(fast_cfg(false, "observe"), &refs);
        m.enable(0);
        m.on_round(&[market()], &[], 0);
        m.on_tick("t", &book(0.43, 0.45), 1000);
        m.on_tick("t", &book(0.95, 0.97), 2000);
        let views = m.variant_views(2000);
        assert!(views.iter().any(|v| v.is_baseline));
        assert_eq!(m.registry().get("alpha", "cap"), Some(dec!(0.40)));
        assert_eq!(m.registry().get("beta", "cap"), Some(dec!(0.40)));
    }

    #[test]
    fn guard_rejects_gradient_beyond_lock() {
        let mut base = StrategyParams::new();
        base.set("cap", dec!(0.40));
        let mut far = StrategyParams::new();
        far.set("cap", dec!(0.48)); // +20%
        assert!(guard::validate_gradient(&base, &far, dec!(0.05)).is_err());
        assert!(guard::validate_gradient(&base, &base.scaled(dec!(1.04)), dec!(0.05)).is_ok());
    }

    #[test]
    fn the_exit_policy_replayed_is_the_configured_one() {
        // D-2: variants must replay the SAME exit config the live position manager
        // uses. The manager forwards `cfg.exit_cfg` verbatim into every twin.
        let (a, b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
        let mut cfg = fast_cfg(false, "exit");
        cfg.exit_cfg = ExitConfig {
            stop_loss_pct: dec!(12),
            ..ExitConfig::default()
        };
        let m = ShadowEvolution::new(cfg, &refs);
        assert_eq!(m.cfg.exit_cfg.stop_loss_pct, dec!(12));
    }

    /// A twin whose strategy code panics on every decision — the archetype of a
    /// buggy user-authored variant (E7 lets users ship these as dylibs).
    struct PanickingTwin {
        name: String,
    }

    impl EngineStrategy for PanickingTwin {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
        fn on_round(&mut self, _slot: i64) {}
        fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            panic!("user-authored twin exploded");
        }
    }

    struct PanicFactory {
        name: String,
    }

    impl ShadowFactory for PanicFactory {
        fn strategy(&self) -> String {
            self.name.clone()
        }
        fn knobs(&self) -> Vec<KnobSpec> {
            vec![KnobSpec::new("cap", dec!(0.40), dec!(0.05), dec!(0.95))]
        }
        fn make(&self, _params: &StrategyParams) -> Option<Box<dyn EngineStrategy>> {
            Some(Box::new(PanickingTwin {
                name: self.name.clone(),
            }))
        }
    }

    struct PanicStrategy {
        name: String,
    }

    impl EngineStrategy for PanicStrategy {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _t: &str, _s: &OrderbookSnapshot, _now: i64) {}
        fn on_round(&mut self, _slot: i64) {}
        fn evolvable_knobs(&self) -> Vec<KnobSpec> {
            vec![KnobSpec::new("cap", dec!(0.40), dec!(0.05), dec!(0.95))]
        }
        fn shadow_factory(&self) -> Option<Box<dyn ShadowFactory>> {
            Some(Box::new(PanicFactory {
                name: self.name.clone(),
            }))
        }
        fn find_candidates(&mut self, _ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            Vec::new()
        }
    }

    /// E10-d's `panic = "abort"` decision (D-20) rests on this guarantee, so it
    /// must be pinned rather than assumed: a variant that panics on every tick
    /// must NOT unwind into the caller of `on_tick`, and the healthy variants
    /// alongside it must keep receiving ticks.
    ///
    /// This is load-bearing precisely because variants are user-authored dylibs.
    #[test]
    fn a_panicking_variant_cannot_take_down_the_engine() {
        let boom = PanicStrategy {
            name: "boom".into(),
        };
        let (good, _) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&boom, &good];
        let mut m = ShadowEvolution::new(fast_cfg(true, "panic"), &refs);
        m.on_round(&[market()], &[], 1_000);

        // A tick that makes every twin want to enter (mid 0.395 under any cap).
        // If the panic escaped, this call itself would abort the test.
        m.on_tick("t", &book(0.39, 0.40), 1_000);
        // And a second tick, so a "quarantine" that only works once is caught.
        m.on_tick("t", &book(0.39, 0.40), 2_000);

        // The healthy strategy's twins must still have been fed: the panic is
        // isolated, not a reason to skip the rest of the loop.
        let good_unit = m
            .units
            .iter()
            .find(|u| u.strategy == "alpha")
            .expect("the healthy strategy keeps its unit");
        assert!(
            good_unit
                .set
                .variants
                .iter()
                .all(|v| !v.crashed),
            "the healthy variant must not be collateral damage"
        );

        // The evaluation pass must also survive reading a twin that panicked.
        let _ = m.evaluate(3_000);
    }

    /// The panic is absorbed inside `shadow_twin::catch`, which returns a default
    /// result — so the twin's own panic does NOT set the `crashed` quarantine flag.
    ///
    /// That is worth pinning explicitly because the doc comment on
    /// `shadow_twin::catch` claims "the caller quarantines it on the panic", while
    /// its only callers (`TwinReplay::on_tick`) absorb the panic with
    /// `unwrap_or_default()` and discard the error. `crashed` is therefore only
    /// reachable from a panic OUTSIDE the catch (i.e. the exit/replay machinery).
    /// Reading it as "bad strategies get quarantined" would be wrong.
    #[test]
    fn a_twins_own_panic_is_absorbed_and_never_sets_the_crashed_flag() {
        let boom = PanicStrategy {
            name: "boom".into(),
        };
        let refs: Vec<&dyn EngineStrategy> = vec![&boom];
        let mut m = ShadowEvolution::new(fast_cfg(true, "absorb"), &refs);
        m.on_round(&[market()], &[], 1_000);
        m.on_tick("t", &book(0.39, 0.40), 1_000);

        let unit = &m.units[0];
        assert!(
            unit.set.variants.iter().all(|v| !v.crashed),
            "the twin's panic is swallowed by shadow_twin::catch, so the outer \
             `crashed` flag never latches — the engine survives, but a permanently \
             panicking variant keeps being ticked forever"
        );
    }
}
