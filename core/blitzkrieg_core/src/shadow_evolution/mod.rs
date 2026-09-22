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
//!   └─ ProposalStore                       (E13: held proposals + promotions + runtime state)
//! ```
//!
//! E13 (#95) promotion workflow: by default a qualifying variant becomes an
//! **EvolutionProposal** (full side-by-side comparison, persisted to
//! `proposals.jsonl`) that the operator accepts / rejects / defers over IPC;
//! the `auto_evolve` switch restores unattended application, and every
//! adoption — auto or accepted — is written to `promotions.jsonl` so the
//! one-click rollback survives a restart. Every 72h (configurable) a DEEP
//! round re-anchors each unit with compound multi-knob variants.
//!
//! Isolation: every variant tick runs under `catch_unwind`; a panicking variant
//! is marked crashed and excluded, and NEVER propagates into the main strategy
//! path (spec: "影子引擎崩溃不影响主策略").
//!
//! A strategy that declares no knobs is **not evolvable** and gets no unit at
//! all — an explicit declaration (D6), not a silent no-op.

/// The knob names a proposal would move — the E13 comparison's dimension list.
fn moved_dims(from: &StrategyParams, to: &StrategyParams) -> Vec<String> {
    to.iter()
        .filter(|(n, v)| from.get(n) != Some(*v))
        .map(|(n, _)| n.to_string())
        .collect()
}

/// Wrap ONE strategy's parameters in the aggregate type the signal/audit
/// surfaces carry (same shape as the evaluator's `params_for`).
fn wrap_params(strategy: &str, params: StrategyParams) -> MutableParams {
    let mut m = MutableParams::new();
    m.set_strategy(strategy, params);
    m
}

/// The signal an accepted proposal is audited and reported with. Shared by the
/// operator path and the auto switch's drain (#249), so an adoption looks the
/// same in the audit log however it was decided.
fn adoption_signal(proposal: &EvolutionProposal, now_ms: i64) -> EvolveSignal {
    let strategy = proposal.strategy.clone();
    EvolveSignal::new(
        proposal.id.clone(),
        now_ms,
        strategy.clone(),
        wrap_params(&strategy, proposal.from_params.clone()),
        wrap_params(&strategy, proposal.to_params.clone()),
        proposal.reason,
        proposal.confidence,
        proposal.sample_count,
        proposal.variant.win_rate - proposal.baseline.win_rate,
        "proposal".into(),
    )
}

pub mod audit;
pub mod config;
pub mod evaluator;
pub mod guard;
pub mod knobs;
pub mod proposal;
pub mod registry;
pub mod signal;
pub mod variants;

pub use config::{
    EvolutionStatus, ImmutableConfig, KnobDeclaration, KnobSpec, MutableParams,
    ShadowEvolutionConfig, StrategyParams, VariantView,
};
pub use proposal::{DecidedBy, EvolutionProposal, ProposalState, TradeMetrics};
pub use registry::ParamRegistry;
pub use signal::{EvolutionReason, EvolveSignal};

use crate::model::{CryptoMarket, OrderbookSnapshot};
use crate::strategies::EngineStrategy;
use crate::strategies::shadow_twin::ShadowTickCtx;
use audit::AuditLog;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use variants::{VariantSet, build_deep_variants, build_variants};

/// Result of one evaluation/action, mapped to events by the caller.
///
/// `Proposed` carries a whole proposal record (#251 added the decision reason to
/// it), which is what clippy's `large_enum_variant` points at. Left inline on
/// purpose: one of these is built per strategy per pass and consumed
/// immediately, never held in a large collection, and `EvolutionProposal` is
/// also carried inline by `DecisionResult` and the proposal store — boxing it
/// here alone would make the same record's API inconsistent to save a copy that
/// nothing measures.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum EvolutionOutcome {
    Signal(EvolveSignal),
    Applied(EvolveSignal),
    /// E13: a qualifying variant was HELD as a proposal for a human decision
    /// (the manual mode — `auto_evolve` off).
    Proposed(EvolutionProposal),
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

/// What the operator decided about one proposal (E13 / #95).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    Reject,
    Defer,
}

/// Result of one operator decision, mapped to events by the caller.
#[derive(Debug)]
pub enum DecisionResult {
    Accepted { proposal: EvolutionProposal },
    Rejected { proposal: EvolutionProposal },
    Deferred { proposal: EvolutionProposal },
}

/// One DEEP evolution round (E13): the variant sets were re-anchored with
/// compound multi-knob mutants.
#[derive(Debug, Clone)]
pub struct EvolutionCycleEvent {
    pub cycle_seq: u64,
    pub dims: usize,
    pub strategies: Vec<String>,
    pub at_ms: i64,
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
    /// Adoptions that moved this strategy's parameters (evaluator auto-adoptions
    /// and accepted proposals alike).
    evolution_count: u64,
    /// Refusals recorded for this strategy: the evaluator's guard refusals plus
    /// proposals closed out as rejected. Both are counted so the number a
    /// script reads here equals the 台账's 已拒绝 rows (#251) — the two used to
    /// disagree, and a disagreement about history is worse than no number.
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

    /// E13: build a DEEP round's variant set — compound mutants moving up to
    /// `deep_dims` knobs at once, so a cycle can explore knob combinations the
    /// single-knob rotation never visits. Same sweep argument, same locks.
    fn scaffold_deep(&mut self, cfg: &ShadowEvolutionConfig, now_ms: i64) {
        self.set = build_deep_variants(
            &self.strategy,
            &self.specs,
            &self.cell.load(),
            self.factory.as_ref(),
            cfg.variant_count.max(2),
            cfg.max_gradient,
            &cfg.exit_cfg,
            now_ms,
            self.evolution_count,
            cfg.deep_dims,
        );
    }
}

/// Wall-clock ms for the entry points no engine clock reaches: the manager's
/// construction. (Every other lifecycle timestamp arrives with a tick or an IPC
/// call.) A variant's age is measured against this stamp, so it is a real clock
/// by contract, never a placeholder (#250).
fn wall_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    /// E13: held proposals, promotion history and the runtime state file,
    /// all under the audit directory.
    proposal_store: proposal::ProposalStore,
    /// E13: the unattended switch (the auto-evolve checkbox in the UIs).
    auto_evolve: bool,
    /// E13: wall-clock ms of the last DEEP round (0 = clock not started).
    last_cycle_ms: i64,
    cycle_seq: u64,
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
        let store = proposal::ProposalStore::new(&cfg.audit_dir);
        // E13 + #249: the runtime state (both switches + the deep-round clock) is
        // persisted under the audit dir and WINS over the config file — it is what
        // the UIs' switches flip, so a restart must not undo the operator. A key
        // an older kernel never wrote hands that decision back to the config.
        let st = store.load_state().unwrap_or_default();
        let mut cfg = cfg;
        if let Some(on) = st.enabled {
            // The persisted switch is the operator's most recent word, so it wins
            // over the file — a deploy editing the file must not undo a switch
            // flipped in the panel. But when the FILE asks for OFF and the runtime
            // says ON, that is a kill switch that did not kill: say so loudly, and
            // name the way out, because "the file says off" is exactly what an
            // incident responder will look at.
            if on != cfg.enabled {
                if on {
                    tracing::warn!(
                        file = false,
                        "the config file disables the evolution engine but the persisted \
                         runtime switch has it ON — the runtime switch wins; use the panel's \
                         engine switch, or remove <audit_dir>/state.json, to turn it off"
                    );
                } else {
                    tracing::info!(
                        file = true,
                        "the persisted runtime switch keeps the evolution engine off; the \
                         file re-enables it only once that switch is turned back on"
                    );
                }
            }
            cfg.enabled = on;
        }
        cfg.auto_evolve = st.auto_evolve.unwrap_or(cfg.auto_evolve);
        let enabled = cfg.enabled;
        // The audit log's own gate is read from this config: an engine the
        // persisted state switched on (and the file did not) must not silently
        // lose every record it writes.
        let audit = AuditLog::new(&cfg);
        let (last_cycle_ms, cycle_seq) = (st.last_cycle_ms, st.cycle_seq);
        let auto_evolve = cfg.auto_evolve;
        let mut me = Self {
            enabled,
            cfg,
            registry: Arc::new(ParamRegistry::new()),
            units: Vec::new(),
            token_expiry: HashMap::new(),
            round_markets: Vec::new(),
            audit,
            auto_evolve,
            last_cycle_ms,
            cycle_seq,
            proposal_store: store,
        };
        me.proposal_store.load();
        me.register_strategies(strategies, wall_now_ms());
        me
    }

    /// (Re)build one unit per evolvable strategy. Called at construction and again
    /// whenever the engine installs its host set — the manager exists before any
    /// engine does (the engine is installed from config later), and the declared
    /// knobs live on the strategies themselves.
    ///
    /// Re-registering is safe and keeps state: `ParamRegistry::publish` returns the
    /// strategy's existing cell, so the parameters in force survive a rebuild.
    ///
    /// `now_ms` is the birth stamp of every variant this rebuild creates, and it
    /// must be a real clock: `created_at_ms` is what the evaluator's observation
    /// floor measures against, so a placeholder here does not merely mislabel a
    /// timestamp — it retires the floor (#250). A rebuild re-anchors each variant
    /// around the parameters in force, so all of them start observing at this
    /// instant by construction.
    pub fn register_strategies(&mut self, strategies: &[&dyn EngineStrategy], now_ms: i64) {
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
            let carried_unit = carried.remove(s.name());
            let (last_evolution_ms, evolution_count, rejected_count, previous) = match carried_unit
            {
                Some(c) => c,
                // #251: a unit born now (process start, or a strategy that joined
                // later) takes its counters from the folded ledger instead of
                // zero. The counters used to be memory-only, so every restart
                // showed "已采纳 0 / 已拒绝 0" next to a ledger listing adoptions
                // and refusals — two readings of the same history that could not
                // both be right. Seeding from the ledger also keeps the sweep
                // offset continuous across a restart.
                None => {
                    let (adopted, refused) = self.proposal_store.terminal_counts(s.name());
                    if adopted > 0 || refused > 0 {
                        tracing::debug!(
                            strategy = s.name(),
                            adopted,
                            refused,
                            "seeded the evolution counters from the proposal ledger"
                        );
                    }
                    (0, adopted, refused, None)
                }
            };
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
        // #245: a promotion used to be memory-only, so a restart dropped the
        // adopted parameters and every strategy fell back to its declared
        // defaults while the promotion log still reported the adopted value —
        // two "current parameters" readings that disagreed. Restore first, so
        // the scaffold below anchors the variants around what was really in
        // force rather than around a value nobody chose.
        self.restore_adopted_params();
        if self.enabled {
            for i in 0..self.units.len() {
                self.units[i].scaffold(&self.cfg, now_ms);
            }
        }
    }

    /// Re-apply the last un-rolled-back promotion of every strategy whose cell
    /// holds nothing but its declared defaults — the exact shape a restart leaves
    /// behind (#245).
    ///
    /// Deliberately narrow: a cell that differs from its declaration was put
    /// there by something else (an operator override, or a promotion restored
    /// earlier), and that is not this function's to overwrite. The restore
    /// re-checks the declaration/domain/immutable locks but NOT the gradient —
    /// the same rule the E13 rollback uses, because it restores a value that was
    /// already in force once, which is not a step in an unbounded direction.
    /// `previous` is left empty on purpose: the E13 rollback then re-derives its
    /// target from the promotion log with the full re-validation, instead of
    /// trusting an anchor read back from disk.
    fn restore_adopted_params(&mut self) {
        for u in self.units.iter_mut() {
            let declared = StrategyParams::from_knobs(&u.specs);
            if **u.cell.load() != declared {
                continue; // something else owns this cell; not ours to overwrite
            }
            let Some(rec) = self.proposal_store.last_active_promotion(&u.strategy) else {
                continue;
            };
            if rec.to_params == declared {
                continue; // the promotion moved nothing this strategy still lacks
            }
            let checked = guard::validate_declared(&rec.to_params, &u.specs)
                .and_then(|_| guard::validate_domain(&rec.to_params, &u.specs))
                .and_then(|_| guard::validate_immutable(&self.cfg.risk));
            match checked {
                Ok(()) => {
                    tracing::info!(
                        strategy = %u.strategy,
                        proposal = %rec.proposal_id,
                        "restored the parameters of the last promotion"
                    );
                    u.cell.store(Arc::new(rec.to_params));
                }
                Err(e) => tracing::warn!(
                    strategy = %u.strategy,
                    proposal = %rec.proposal_id,
                    reason = %e,
                    "the adopted parameters no longer pass the guards — not restored"
                ),
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
    /// Persisted (#249), because the switch is runtime state: a restart that
    /// silently switched the engine back off is what made "自动进化：开" a label
    /// that did nothing.
    pub fn enable(&mut self, now_ms: i64) {
        self.enabled = true;
        self.cfg.enabled = true;
        self.audit.set_enabled(true);
        for i in 0..self.units.len() {
            self.units[i].scaffold(&self.cfg, now_ms);
        }
        self.persist_state();
    }

    pub fn disable(&mut self) {
        self.enabled = false;
        self.cfg.enabled = false;
        self.audit.set_enabled(false);
        for u in self.units.iter_mut() {
            u.set = VariantSet::empty(&u.strategy);
        }
        self.persist_state();
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
                // Two panic surfaces feed the same latch (KI-23): the twin's own
                // code, reported as `true` through the absorbed inner catch, and
                // the replay/exit machinery, still caught by this outer unwind.
                // Absorbing it here without latching would re-tick a permanently
                // panicking twin every tick, silently.
                let panicked = catch_unwind(AssertUnwindSafe(|| v.on_tick(&ctx))).unwrap_or(true);
                if panicked {
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
        // E13: undecided proposals past their TTL expire here, so the pending
        // list never shows a stale decision opportunity. Runs even while the
        // engine is switched off — a held proposal is bookkeeping, and quietly
        // letting its TTL lapse is exactly the staleness the operator cannot see.
        let expired = self.proposal_store.expire_stale(now_ms);
        if expired > 0 {
            tracing::info!(count = expired, "shadow evolution proposals expired");
        }
        // The engine switch is the master switch: while it is off nothing moves,
        // not even a backlog the auto switch would otherwise drain. An operator
        // who switches the engine off to stop the machinery must not find that it
        // applied a held proposal anyway.
        if !self.enabled {
            return Vec::new();
        }
        // #249: in unattended mode a held proposal is a contradiction — nobody is
        // going to answer it — so the backlog is decided (as `auto`) before this
        // pass looks for anything new. Done HERE rather than in `set_auto_evolve`
        // so that flipping the switch does not itself apply a parameter: the next
        // pass applies it, with the guards re-checked against the parameters in
        // force at that moment.
        let mut out = if self.auto_evolve {
            self.adopt_pending(now_ms)
        } else {
            Vec::new()
        };
        for i in 0..self.units.len() {
            if let Some(o) = self.evaluate_unit(i, now_ms) {
                out.push(o);
            }
        }
        out
    }

    /// Decide every held proposal the way the auto switch implies a human would,
    /// under the very same guards and audit trail (`decide_as` with
    /// `DecidedBy::Auto`), so an unattended adoption stays as inspectable as a
    /// hand-clicked one.
    ///
    /// A proposal whose premises moved while it was held (the strategy evolved
    /// again, or an operator edited knobs in between) fails the re-check and is
    /// closed out as rejected WITH the guard's reason — a stale proposal is
    /// recorded, never silently dropped. Returns the adoptions, for the event
    /// stream the UIs follow.
    fn adopt_pending(&mut self, now_ms: i64) -> Vec<EvolutionOutcome> {
        let mut out = Vec::new();
        for p in self.proposal_store.pending() {
            match self.decide_as(&p.id, Decision::Accept, now_ms, DecidedBy::Auto) {
                Ok(DecisionResult::Accepted { proposal }) => {
                    tracing::info!(
                        strategy = %proposal.strategy,
                        proposal = %proposal.id,
                        "auto-evolve adopted a held proposal"
                    );
                    out.push(EvolutionOutcome::Applied(adoption_signal(
                        &proposal, now_ms,
                    )));
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(
                    proposal = %p.id,
                    reason = %e,
                    "auto-evolve could not adopt a held proposal"
                ),
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
        let mut held: Option<EvolutionProposal> = None;
        let mut adopted: Option<EvolutionProposal> = None;
        {
            let u = &mut self.units[i];
            // Baseline metrics are computed here and hoisted so a HELD proposal
            // can carry the same comparison block the auto path is judged on.
            let baseline;
            let (signal, variant_index) = {
                baseline = u.set.baseline_metrics(cfg.evaluation_window_secs, now_ms);
                let sel = evaluator::evaluate(
                    &cfg,
                    &u.strategy,
                    &mut u.set,
                    &baseline,
                    now_ms,
                    u.last_evolution_ms,
                )?;
                (sel.signal, sel.variant_index)
            };
            // Safety locks. Lock 0 (declaration) and Lock 1 (domain) are checked
            // against THIS strategy's own declaration; Lock 2 (gradient) against
            // the parameters in force; Lock 3 against the immutable laws.
            let old: StrategyParams = (**u.cell.load()).clone();
            let Some(new_params) = signal.to_params.for_strategy(&u.strategy).cloned() else {
                return None; // a signal that does not name this strategy is dropped
            };
            let checked = guard::validate_declared(&new_params, &u.specs)
                .and_then(|_| guard::validate_domain(&new_params, &u.specs))
                .and_then(|_| guard::validate_gradient(&old, &new_params, cfg.max_gradient))
                .and_then(|_| guard::validate_immutable(&cfg.risk));

            match checked {
                Err(e) => {
                    u.rejected_count += 1;
                    // Respect the cooldown even on rejection: a rejected proposal
                    // must not become a hot rejection loop.
                    u.last_evolution_ms = now_ms;
                    rejection = Some((signal, e.to_string()));
                }
                Ok(()) if cfg.auto_evolve => {
                    // Unattended mode (E13): publish atomically and re-anchor
                    // THIS strategy, exactly as before the proposal workflow.
                    u.previous = Some(old.clone());
                    u.cell.store(Arc::new(new_params.clone()));
                    u.last_evolution_ms = now_ms;
                    u.evolution_count += 1;
                    // The comparison block is measured BEFORE the re-anchor
                    // clears the twin's history, and the adoption is recorded as
                    // a decided proposal (#251): an unattended adoption is the
                    // one thing an operator most needs to see, and it used to
                    // leave no ledger row at all — the panel showed an empty
                    // 台账 while the parameters had in fact moved.
                    let variant_metrics =
                        u.set.variants[variant_index].metrics(cfg.evaluation_window_secs, now_ms);
                    u.scaffold(&cfg, now_ms);
                    // The adoption is durable: a rollback must be able to undo
                    // it after a restart, so the promotion log gets the record.
                    self.proposal_store.record_promotion(
                        &signal.signal_id,
                        &u.strategy,
                        &old,
                        &new_params,
                        DecidedBy::Auto,
                        now_ms,
                    );
                    adopted = Some(EvolutionProposal {
                        // A distinct prefix: this row is not a held proposal that
                        // was decided, and it must never fold into one that is
                        // (a same-millisecond id would silently replace a row).
                        id: format!("auto-{now_ms}-{}", u.strategy),
                        strategy: u.strategy.clone(),
                        dims: moved_dims(&old, &new_params),
                        from_params: old,
                        to_params: new_params,
                        baseline: proposal::TradeMetrics::from_window(&baseline),
                        variant: proposal::TradeMetrics::from_window(&variant_metrics),
                        reason: signal.reason,
                        confidence: signal.confidence,
                        sample_count: signal.sample_count,
                        created_at_ms: now_ms,
                        expires_at_ms: now_ms,
                        state: proposal::ProposalState::Accepted,
                        decided_by: Some(DecidedBy::Auto),
                        decided_at_ms: Some(now_ms),
                        // An adoption's reason is the comparison block itself.
                        decided_reason: None,
                        cycle_seq: self.cycle_seq,
                    });
                    applied = Some(signal);
                }
                Ok(()) => {
                    // Manual mode (E13): HOLD. The cell does not move and the
                    // twin keeps running; the operator decides over IPC. The
                    // variant's metrics were just measured for the guard pass —
                    // reuse them as the proposal's comparison block.
                    let variant_metrics =
                        u.set.variants[variant_index].metrics(cfg.evaluation_window_secs, now_ms);
                    let dims = moved_dims(&old, &new_params);
                    held = Some(EvolutionProposal {
                        id: format!("prop-{now_ms}-{}", u.strategy),
                        strategy: u.strategy.clone(),
                        dims,
                        from_params: old,
                        to_params: new_params,
                        baseline: proposal::TradeMetrics::from_window(&baseline),
                        variant: proposal::TradeMetrics::from_window(&variant_metrics),
                        reason: signal.reason,
                        confidence: signal.confidence,
                        sample_count: signal.sample_count,
                        created_at_ms: now_ms,
                        expires_at_ms: now_ms + cfg.proposal_ttl_secs * 1000,
                        state: proposal::ProposalState::Proposed,
                        decided_by: None,
                        decided_at_ms: None,
                        decided_reason: None,
                        cycle_seq: self.cycle_seq,
                    });
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
            // #251: the ledger row for the unattended adoption, written outside
            // the unit borrow like every other store write here.
            if let Some(p) = adopted {
                self.proposal_store.put(p);
            }
            outcome = Some(EvolutionOutcome::Applied(signal));
        } else if let Some(proposal) = held {
            // One pending proposal per strategy: a fresher qualifying signal
            // replaces a pending one with different targets (the older
            // comparison is stale), and an identical re-proposal is a no-op.
            let existing = self.proposal_store.pending_for(&proposal.strategy).cloned();
            match existing {
                Some(prev) if prev.to_params == proposal.to_params => {
                    outcome = None; // already held, nothing new to show
                }
                Some(mut prev) => {
                    prev.state = proposal::ProposalState::Superseded;
                    prev.decided_at_ms = Some(now_ms);
                    prev.decided_reason = Some(proposal::DecisionReason::Superseded {
                        by_id: proposal.id.clone(),
                    });
                    self.proposal_store.put(prev);
                    self.proposal_store.put(proposal.clone());
                    outcome = Some(EvolutionOutcome::Proposed(proposal));
                }
                None => {
                    self.proposal_store.put(proposal.clone());
                    outcome = Some(EvolutionOutcome::Proposed(proposal));
                }
            }
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
    ///
    /// E13: when the in-memory anchor is gone (a restart dropped `previous`)
    /// the promotion log takes over — the last un-rolled-back promotion for
    /// this strategy is restored, so the one-click rollback survives a
    /// restart. The restore re-checks the declaration/domain/immutable locks
    /// but NOT the gradient: it restores a value that was already in force
    /// once, which is not a step in an unbounded direction.
    pub fn rollback(&mut self, strategy: &str, now_ms: i64) -> Result<EvolutionOutcome, String> {
        let cfg = self.cfg.clone();
        let u = self
            .units
            .iter_mut()
            .find(|u| u.strategy == strategy)
            .ok_or_else(|| format!("strategy {strategy} is not evolvable"))?;
        let restore = match u.previous.clone() {
            Some(prev) => prev,
            None => {
                let cfg_restored = self
                    .proposal_store
                    .last_active_promotion(strategy)
                    .ok_or_else(|| {
                        format!("strategy {strategy} has no previous parameters to roll back to")
                    })?;
                guard::validate_declared(&cfg_restored.from_params, &u.specs)
                    .map_err(|e| e.to_string())?;
                guard::validate_domain(&cfg_restored.from_params, &u.specs)
                    .map_err(|e| e.to_string())?;
                guard::validate_immutable(&cfg.risk).map_err(|e| e.to_string())?;
                cfg_restored.from_params
            }
        };
        let from: StrategyParams = (**u.cell.load()).clone();
        u.cell.store(Arc::new(restore.clone()));
        u.previous = Some(from.clone());
        u.last_evolution_ms = now_ms;
        // E13: the rollback must also clear the cross-restart restore target —
        // otherwise a second rollback after a restart would re-apply exactly
        // what the operator just undid.
        let undone = self
            .proposal_store
            .last_active_promotion(strategy)
            .map(|r| r.proposal_id)
            .unwrap_or_else(|| "memory".into());
        self.proposal_store
            .record_rollback(&undone, strategy, &from, &restore, now_ms);
        let (mut fm, mut tm) = (MutableParams::new(), MutableParams::new());
        fm.set_strategy(strategy, from);
        tm.set_strategy(strategy, restore);
        self.audit.record_rollback(strategy, now_ms, &fm, &tm);
        Ok(EvolutionOutcome::RolledBack {
            strategy: strategy.to_string(),
            from: fm,
            to: tm,
        })
    }

    // ── E13 (#95): the promotion workflow ────────────────────────────────────

    /// The operator's verdict on ONE held proposal. The acceptance re-runs the
    /// FULL guard chain against the parameters in force at decision time — a
    /// proposal is an observation, never a bypass of the locks.
    pub fn decide(
        &mut self,
        id: &str,
        decision: Decision,
        now_ms: i64,
    ) -> Result<DecisionResult, String> {
        self.decide_as(id, decision, now_ms, DecidedBy::User)
    }

    /// `decide` with the actor named in the record. The auto switch drains its
    /// own backlog through here (#249) so that an unattended adoption travels
    /// the same code path as a hand-clicked one — same guards, same audit, same
    /// promotion log — and differs only in who is on record.
    pub fn decide_as(
        &mut self,
        id: &str,
        decision: Decision,
        now_ms: i64,
        by: DecidedBy,
    ) -> Result<DecisionResult, String> {
        let cfg = self.cfg.clone();
        let mut proposal = self
            .proposal_store
            .get(id)
            .cloned()
            .ok_or_else(|| format!("proposal {id} is unknown"))?;
        if !proposal.is_decidable() {
            return Err(format!(
                "proposal {id} is already {} — a decided proposal cannot be re-decided",
                proposal.state
            ));
        }
        match decision {
            Decision::Accept => {
                let u = self
                    .units
                    .iter_mut()
                    .find(|u| u.strategy == proposal.strategy)
                    .ok_or_else(|| {
                        format!("strategy {} is no longer evolvable", proposal.strategy)
                    })?;
                let old: StrategyParams = (**u.cell.load()).clone();
                // Which lock refused is part of what an operator reads, so the
                // chain names it instead of collapsing four locks into one
                // "guards failed" (#251).
                let checked = guard::validate_declared(&proposal.to_params, &u.specs)
                    .map_err(|e| ("declared", e))
                    .and_then(|_| {
                        guard::validate_domain(&proposal.to_params, &u.specs)
                            .map_err(|e| ("domain", e))
                    })
                    .and_then(|_| {
                        guard::validate_gradient(&old, &proposal.to_params, cfg.max_gradient)
                            .map_err(|e| ("gradient", e))
                    })
                    .and_then(|_| {
                        guard::validate_immutable(&cfg.risk).map_err(|e| ("immutable", e))
                    });
                if let Err((lock, e)) = checked {
                    // The world moved while the proposal was held (the strategy
                    // evolved again, or the operator edited knobs): the stale
                    // proposal is refused explicitly and closed out — and the lock
                    // that refused it is recorded on the record and in the audit
                    // log, because this path used to leave no reason anywhere and
                    // an auto-refusal then read as a glitch (#251).
                    let detail = e.to_string();
                    proposal.state = ProposalState::Rejected;
                    proposal.decided_by = Some(by);
                    proposal.decided_at_ms = Some(now_ms);
                    proposal.decided_reason = Some(proposal::DecisionReason::GuardFailed {
                        guard: lock.to_string(),
                        detail: detail.clone(),
                    });
                    // The ledger row this writes says 已拒绝, so the counter has to
                    // say the same thing (#251).
                    u.rejected_count += 1;
                    self.audit.record_rejection(
                        &EvolveSignal::new(
                            proposal.id.clone(),
                            now_ms,
                            proposal.strategy.clone(),
                            wrap_params(&proposal.strategy, proposal.from_params.clone()),
                            wrap_params(&proposal.strategy, proposal.to_params.clone()),
                            proposal.reason,
                            proposal.confidence,
                            proposal.sample_count,
                            proposal.variant.win_rate - proposal.baseline.win_rate,
                            "proposal".into(),
                        ),
                        format!("{lock} lock no longer holds: {detail}"),
                        if lock == "gradient" { "failed" } else { "n/a" },
                        if lock == "immutable" { "failed" } else { "n/a" },
                    );
                    self.proposal_store.put(proposal.clone());
                    return Err(format!(
                        "proposal {id} no longer passes the {lock} lock: {detail}"
                    ));
                }
                u.previous = Some(old);
                u.cell.store(Arc::new(proposal.to_params.clone()));
                u.last_evolution_ms = now_ms;
                u.evolution_count += 1;
                u.scaffold(&cfg, now_ms);
                proposal.state = ProposalState::Accepted;
                proposal.decided_by = Some(by);
                proposal.decided_at_ms = Some(now_ms);
                let from_params = proposal.from_params.clone();
                let to_params = proposal.to_params.clone();
                let strategy = proposal.strategy.clone();
                let signal = adoption_signal(&proposal, now_ms);
                self.audit.record_applied(&signal);
                self.proposal_store.put(proposal.clone());
                self.proposal_store.record_promotion(
                    id,
                    &strategy,
                    &from_params,
                    &to_params,
                    by,
                    now_ms,
                );
                Ok(DecisionResult::Accepted { proposal })
            }
            Decision::Reject => {
                proposal.state = ProposalState::Rejected;
                proposal.decided_by = Some(by);
                proposal.decided_at_ms = Some(now_ms);
                proposal.decided_reason = Some(proposal::DecisionReason::Rejected);
                if let Some(u) = self
                    .units
                    .iter_mut()
                    .find(|u| u.strategy == proposal.strategy)
                {
                    u.rejected_count += 1;
                }
                self.audit.record_rejection(
                    &EvolveSignal::new(
                        proposal.id.clone(),
                        now_ms,
                        proposal.strategy.clone(),
                        wrap_params(&proposal.strategy, proposal.from_params.clone()),
                        wrap_params(&proposal.strategy, proposal.to_params.clone()),
                        proposal.reason,
                        proposal.confidence,
                        proposal.sample_count,
                        proposal.variant.win_rate - proposal.baseline.win_rate,
                        "proposal".into(),
                    ),
                    "rejected by operator".into(),
                    "n/a",
                    "n/a",
                );
                self.proposal_store.put(proposal.clone());
                Ok(DecisionResult::Rejected { proposal })
            }
            Decision::Defer => {
                proposal.state = ProposalState::Deferred;
                proposal.decided_at_ms = Some(now_ms);
                self.proposal_store.put(proposal.clone());
                Ok(DecisionResult::Deferred { proposal })
            }
        }
    }

    /// Pending (still decidable) proposals, newest first.
    pub fn pending_proposals(&self) -> Vec<EvolutionProposal> {
        self.proposal_store.pending()
    }

    /// Every known proposal's latest state, newest first (IPC surface).
    pub fn all_proposals(&self, limit: usize) -> Vec<EvolutionProposal> {
        self.proposal_store.latest(limit)
    }

    pub fn pending_proposal_count(&self) -> usize {
        self.proposal_store.pending_count()
    }

    /// The unattended switch (the UIs' auto-evolve checkbox). Persisted, so a
    /// restart keeps the last chosen mode.
    pub fn set_auto_evolve(&mut self, on: bool) {
        if self.auto_evolve == on {
            return;
        }
        self.auto_evolve = on;
        self.cfg.auto_evolve = on;
        tracing::info!(auto_evolve = on, "shadow evolution auto-evolve switched");
        self.persist_state();
    }

    pub fn auto_evolve(&self) -> bool {
        self.auto_evolve
    }

    /// `(last_cycle_ms, cycle_seq)` for the status surface.
    pub fn cycle_info(&self) -> (i64, u64) {
        (self.last_cycle_ms, self.cycle_seq)
    }

    /// Seconds between DEEP rounds (the status surface's period display). Sent
    /// with the status because a UI that hard-codes "72 hours" lies the moment
    /// the config file says otherwise.
    pub fn cycle_secs(&self) -> i64 {
        self.cfg.evolution_cycle_secs
    }

    /// When the next DEEP round fires (`None` before the first observation
    /// starts the clock, or while disabled).
    pub fn next_cycle_at_ms(&self, now_ms: i64) -> Option<i64> {
        if !self.enabled {
            return None;
        }
        if self.last_cycle_ms == 0 {
            return None;
        }
        Some(self.last_cycle_ms + self.cfg.evolution_cycle_secs * 1000).filter(|t| *t > now_ms)
    }

    fn persist_state(&self) {
        self.proposal_store.save_state(proposal::PersistedState {
            enabled: Some(self.enabled),
            auto_evolve: Some(self.auto_evolve),
            last_cycle_ms: self.last_cycle_ms,
            cycle_seq: self.cycle_seq,
        });
    }

    /// E13: the 72h DEEP round. First call only starts the clock (a restart
    /// must not fire a round immediately if the persisted clock says
    /// otherwise); every round re-anchors every unit's variant set with
    /// compound multi-knob mutants, so a cycle explores knob COMBINATIONS
    /// between the single-knob re-anchors.
    pub fn maybe_evolution_cycle(&mut self, now_ms: i64) -> Option<EvolutionCycleEvent> {
        if !self.enabled {
            return None;
        }
        if self.last_cycle_ms == 0 {
            self.last_cycle_ms = now_ms;
            self.persist_state();
            return None;
        }
        if now_ms - self.last_cycle_ms < self.cfg.evolution_cycle_secs * 1000 {
            return None;
        }
        for i in 0..self.units.len() {
            self.units[i].scaffold_deep(&self.cfg, now_ms);
        }
        self.last_cycle_ms = now_ms;
        self.cycle_seq += 1;
        self.persist_state();
        let event = EvolutionCycleEvent {
            cycle_seq: self.cycle_seq,
            dims: self.cfg.deep_dims,
            strategies: self.strategy_names(),
            at_ms: now_ms,
        };
        tracing::info!(
            cycle = event.cycle_seq,
            dims = event.dims,
            "deep evolution round re-anchored variant sets"
        );
        Some(event)
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
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
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
                shares: None,
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
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
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
    /// LOSES and a variant WINS: a dip the baseline's own cap admits (mid
    /// 0.39 <= 0.40) that collapses, then — one tick later — a collapsed price
    /// (mid 0.20) a variant that MISSED the dip is free to buy and ride all the
    /// way back up. The baseline is already holding through the crash, so it
    /// stops out at the collapse and keeps samples but a zero win rate, while
    /// the crash-entry variant beats it on win rate and profit factor.
    ///
    /// The dip and crash books are LOCKED (bid = ask = mid): under the F7
    /// fillability rule a resting bid only fills when the offer side reaches
    /// its price, so the offer must sit at the entry level for a position to
    /// open. The rally book's offer deliberately sits ABOVE every entry limit —
    /// a signal to re-enter the rally is therefore not fillable and produces no
    /// trade (F7), keeping the comparison down to executable trades only.
    fn drive_wins(m: &mut ShadowEvolution, name: &str, times: usize, t0: i64) {
        let round = market();
        let i = m.units.iter().position(|u| u.strategy == name).unwrap();
        let mut now = t0;
        for _ in 0..times {
            // A loser the baseline also takes: entry under the cap, then collapse.
            now += 1_000;
            let dip = book(0.39, 0.39); // mid 0.39 — locked, offer at the entry price
            for v in m.units[i].set.variants.iter_mut() {
                v.on_tick(&tick_ctx(
                    std::slice::from_ref(&round),
                    "t",
                    &dip,
                    1,
                    880,
                    now,
                ));
            }
            now += 1_000;
            // LOCKED at the crash level: holders stop out at the 0.20 bid, and a
            // variant that missed the dip may take the collapsed price — its
            // offer must reach the entry level for that entry to be fillable.
            let crash = book(0.20, 0.20); // mid 0.20 → well past the 12% stop
            for v in m.units[i].set.variants.iter_mut() {
                v.on_tick(&tick_ctx(
                    std::slice::from_ref(&round),
                    "t",
                    &crash,
                    1,
                    880,
                    now,
                ));
            }
            // The rally: mid 0.41 clears the 0.40 baseline cap, but its OFFER
            // (0.43) sits above every entry limit, so re-entering here is not
            // fillable (F7) — and its bid stays under the 100% take-profit
            // floor, so the collapsed-price entry rides on to the up tick.
            now += 1_000;
            let shallow = book(0.39, 0.43); // mid 0.41 > the 0.40 baseline cap
            for v in m.units[i].set.variants.iter_mut() {
                v.on_tick(&tick_ctx(
                    std::slice::from_ref(&round),
                    "t",
                    &shallow,
                    1,
                    880,
                    now,
                ));
            }
            now += 1_000;
            let up = book(0.95, 0.97);
            for v in m.units[i].set.variants.iter_mut() {
                v.on_tick(&tick_ctx(
                    std::slice::from_ref(&round),
                    "t",
                    &up,
                    1,
                    880,
                    now,
                ));
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
        // The pre-proposal (unattended) semantics: a qualifying variant applies
        // directly, which is what this acceptance was written against.
        let mut cfg = fast_cfg(false, "parallel");
        cfg.auto_evolve = true;
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
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
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
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
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
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
            good_unit.set.variants.iter().all(|v| !v.crashed),
            "the healthy variant must not be collateral damage"
        );

        // The evaluation pass must also survive reading a twin that panicked.
        let _ = m.evaluate(3_000);
    }

    /// KI-23: the twin's own panic is ABSORBED (nothing propagates), but it must
    /// LATCH `crashed` — a permanently panicking variant is quarantined on its
    /// first panicking tick instead of being re-ticked (and unwinding) forever,
    /// silently. The panic still does not take the engine down, and the second
    /// tick must skip the quarantined variant entirely (no further unwind).
    #[test]
    fn a_permanently_panicking_twin_is_quarantined_on_its_first_panic() {
        let boom = PanicStrategy {
            name: "boom".into(),
        };
        let refs: Vec<&dyn EngineStrategy> = vec![&boom];
        let mut m = ShadowEvolution::new(fast_cfg(true, "quarantine"), &refs);
        m.on_round(&[market()], &[], 1_000);

        // First tick: every variant of "boom" panics inside its own code — the
        // panic is absorbed (this call must not unwind) and `crashed` latches.
        m.on_tick("t", &book(0.39, 0.40), 1_000);
        assert!(
            m.units[0].set.variants.iter().all(|v| v.crashed),
            "a permanently panicking twin must latch `crashed` on its first panic"
        );

        // Second tick: the quarantined variants are skipped — if the flag were
        // not consulted, this would unwind again (and the absorb would hide it,
        // but the skip is what stops the per-tick unwind cost).
        m.on_tick("t", &book(0.39, 0.40), 2_000);

        // The evaluation pass must also survive reading a twin that panicked.
        let _ = m.evaluate(3_000);
    }

    // ── E13 (#95): the proposal workflow ─────────────────────────────────────

    /// MANUAL mode: a qualifying signal does NOT move the cell — it becomes a
    /// pending proposal the operator decides over IPC, and the twin comparison
    /// block carries the same figures the auto path was judged on.
    #[test]
    fn manual_mode_holds_a_proposal_instead_of_applying() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let mut cfg = fast_cfg(false, "hold");
        cfg.auto_evolve = false;
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        m.enable(0);
        m.on_round(&[market()], &[], 0);
        drive_wins(&mut m, "alpha", 2, 10_000);

        let cap_before = m.registry().get("alpha", "cap").unwrap();
        let outcomes = m.evaluate(100_000);
        assert_eq!(outcomes.len(), 1);
        let proposal = match &outcomes[0] {
            EvolutionOutcome::Proposed(p) => p,
            other => panic!("expected a held proposal, got {other:?}"),
        };
        assert_eq!(proposal.strategy, "alpha");
        assert_eq!(proposal.state, ProposalState::Proposed);
        assert_eq!(proposal.baseline.closed, 2, "comparison block populated");
        assert_eq!(proposal.variant.closed, 2);
        assert!(
            proposal.variant.win_rate > proposal.baseline.win_rate,
            "the comparison must show WHY the variant qualifies"
        );
        assert!(
            proposal.expires_at_ms > 100_000,
            "the TTL is set from creation"
        );
        // The cell did not move, and the proposal is on disk.
        assert_eq!(m.registry().get("alpha", "cap").unwrap(), cap_before);
        assert_eq!(m.pending_proposal_count(), 1);

        // An identical re-proposal is a no-op (the operator still has exactly
        // one pending decision for this strategy).
        drive_wins(&mut m, "alpha", 2, 200_000);
        let outcomes2 = m.evaluate(300_000);
        let proposed_again: Vec<_> = outcomes2
            .iter()
            .filter(|o| matches!(o, EvolutionOutcome::Proposed(_)))
            .collect();
        assert!(
            proposed_again.is_empty(),
            "an identical re-proposal is a no-op"
        );
        assert_eq!(m.pending_proposal_count(), 1);
    }

    /// REJECT closes the proposal without touching anything; the same variant
    /// may not re-propose over a decided one silently (the twin keeps its own
    /// cooldown via the shared re-anchor).
    #[test]
    fn rejecting_a_proposal_closes_it_and_moves_nothing() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let cfg = fast_cfg(false, "reject");
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        m.enable(0);
        m.on_round(&[market()], &[], 0);
        drive_wins(&mut m, "alpha", 2, 10_000);
        let outcomes = m.evaluate(100_000);
        let proposal = match &outcomes[0] {
            EvolutionOutcome::Proposed(p) => p.clone(),
            other => panic!("expected a held proposal, got {other:?}"),
        };

        let cap_before = m.registry().get("alpha", "cap").unwrap();
        let result = m
            .decide(&proposal.id, Decision::Reject, 200_000)
            .expect("rejection is a legal decision");
        match result {
            DecisionResult::Rejected { proposal: p } => {
                assert_eq!(p.state, ProposalState::Rejected);
                assert_eq!(p.decided_by, Some(DecidedBy::User));
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert_eq!(m.registry().get("alpha", "cap").unwrap(), cap_before);
        assert_eq!(m.pending_proposal_count(), 0);
        // A decided proposal cannot be re-decided.
        assert!(
            m.decide(&proposal.id, Decision::Accept, 300_000).is_err(),
            "a decided proposal is closed"
        );
    }

    /// ACCEPT runs the FULL guard chain against the parameters in force at
    /// decision time, then hot-swaps, re-anchors, audits and logs a promotion.
    #[test]
    fn accepting_a_proposal_hot_swaps_and_logs_a_durable_promotion() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let cfg = fast_cfg(false, "accept");
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        m.enable(0);
        m.on_round(&[market()], &[], 0);
        drive_wins(&mut m, "alpha", 2, 10_000);
        let outcomes = m.evaluate(100_000);
        let proposal = match &outcomes[0] {
            EvolutionOutcome::Proposed(p) => p.clone(),
            other => panic!("expected a held proposal, got {other:?}"),
        };

        let result = m
            .decide(&proposal.id, Decision::Accept, 200_000)
            .expect("acceptance must pass the guards");
        let accepted = match result {
            DecisionResult::Accepted { proposal: p } => p,
            other => panic!("expected acceptance, got {other:?}"),
        };
        assert_eq!(accepted.state, ProposalState::Accepted);
        assert_ne!(
            m.registry().get("alpha", "cap").unwrap(),
            dec!(0.40),
            "the cell moved"
        );
        assert_eq!(m.evolution_count("alpha"), 1);

        // The promotion is durable and is the restore target for a rollback —
        // including across a restart (a fresh manager reads the same dir).
        let mut m2 = ShadowEvolution::new(fast_cfg(false, "accept"), &refs);
        let rollback = m2
            .rollback("alpha", 400_000)
            .expect("cross-restart rollback must find the promotion");
        match rollback {
            EvolutionOutcome::RolledBack { strategy, .. } => {
                assert_eq!(strategy, "alpha");
            }
            other => panic!("expected a rollback, got {other:?}"),
        }
        assert_eq!(
            m2.registry().get("alpha", "cap").unwrap(),
            dec!(0.40),
            "restored to the pre-promotion parameters"
        );
    }

    /// The auto-evolve switch is persisted: a restart keeps the last chosen
    /// mode, whichever way it was flipped.
    #[test]
    fn the_auto_evolve_switch_survives_a_restart() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let cfg = fast_cfg(false, "autoswitch");
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        assert!(!m.auto_evolve(), "file default is manual");
        m.set_auto_evolve(true);

        let m2 = ShadowEvolution::new(fast_cfg(false, "autoswitch"), &refs);
        assert!(m2.auto_evolve(), "the persisted switch wins over the file");
    }

    /// #249: with the auto switch on there is nobody to answer a held proposal,
    /// so the very next evaluation pass adopts the backlog — as `auto`, through
    /// the same guards and the same promotion log a hand-clicked acceptance
    /// writes. Flipping the switch is not what applies a parameter; the pass
    /// that follows is.
    #[test]
    fn auto_mode_adopts_the_held_backlog_on_the_next_pass() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let cfg = fast_cfg(false, "autodrain"); // manual mode: the proposal is held
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        m.enable(0);
        m.on_round(&[market()], &[], 0);
        drive_wins(&mut m, "alpha", 2, 10_000);
        let held = match &m.evaluate(100_000)[0] {
            EvolutionOutcome::Proposed(p) => p.clone(),
            other => panic!("expected a held proposal, got {other:?}"),
        };
        assert_eq!(m.pending_proposal_count(), 1);

        let before = m.registry().get("alpha", "cap").unwrap();
        m.set_auto_evolve(true);
        assert_eq!(
            m.registry().get("alpha", "cap").unwrap(),
            before,
            "the switch itself applies nothing"
        );

        let outcomes = m.evaluate(200_000);
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, EvolutionOutcome::Applied(s) if s.strategy == "alpha")),
            "the pass that follows the switch adopts the backlog: {outcomes:?}"
        );
        assert_eq!(m.pending_proposal_count(), 0, "auto mode has no backlog");
        assert_eq!(
            m.registry().get("alpha", "cap").unwrap(),
            held.to_params.get("cap").unwrap(),
            "the cell holds the proposal's target"
        );
        let log = std::fs::read_to_string(dir.join("promotions.jsonl")).unwrap();
        assert!(log.contains(&held.id), "the adoption is logged: {log}");
        assert!(
            log.contains("\"decidedBy\":\"auto\""),
            "and recorded as unattended: {log}"
        );
        let decided = m
            .all_proposals(10)
            .into_iter()
            .find(|p| p.id == held.id)
            .unwrap();
        assert_eq!(decided.state, ProposalState::Accepted);
        assert_eq!(decided.decided_by, Some(DecidedBy::Auto));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A backlog entry whose premises moved while it was held (the operator
    /// edited the knob in between) must NOT be applied: the drain closes it out
    /// as rejected with the guard's reason — recorded, never silently dropped,
    /// and never left pending in a mode that has no one to decide it.
    #[test]
    fn auto_mode_closes_out_a_backlog_entry_whose_premises_moved() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let cfg = fast_cfg(false, "autostale");
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        m.enable(0);
        m.on_round(&[market()], &[], 0);
        drive_wins(&mut m, "alpha", 2, 10_000);
        let held = match &m.evaluate(100_000)[0] {
            EvolutionOutcome::Proposed(p) => p.clone(),
            other => panic!("expected a held proposal, got {other:?}"),
        };
        // The operator moves the knob the other way (a legal +2.5% step), so the
        // held proposal's target is now more than one gradient step away.
        let mut p = StrategyParams::new();
        p.set("cap", dec!(0.41));
        m.set_params("alpha", p, 150_000).unwrap();

        m.set_auto_evolve(true);
        let outcomes = m.evaluate(200_000);
        assert!(
            !outcomes
                .iter()
                .any(|o| matches!(o, EvolutionOutcome::Applied(_))),
            "a stale step is not applied: {outcomes:?}"
        );
        assert_eq!(m.pending_proposal_count(), 0, "and is not left pending");
        assert_eq!(
            m.registry().get("alpha", "cap").unwrap(),
            dec!(0.41),
            "the operator's value stands"
        );
        let decided = m
            .all_proposals(10)
            .into_iter()
            .find(|p| p.id == held.id)
            .unwrap();
        assert_eq!(decided.state, ProposalState::Rejected);
        assert_eq!(decided.decided_by, Some(DecidedBy::Auto));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #251: an adoption refused at decision time used to close the proposal
    /// with no reason anywhere — the panel said 已拒绝 and the operator could not
    /// tell "I said no" from "the gradient lock refused it". The lock that
    /// refused it is now on the row, in the audit log, and in the counter the
    /// IPC surface reports; a restart reads all of it back from the ledger.
    #[test]
    fn a_refused_adoption_records_the_lock_that_refused_it() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let cfg = fast_cfg(false, "reftreason");
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        m.enable(0);
        m.on_round(&[market()], &[], 0);
        drive_wins(&mut m, "alpha", 2, 10_000);
        let held = match &m.evaluate(100_000)[0] {
            EvolutionOutcome::Proposed(p) => p.clone(),
            other => panic!("expected a held proposal, got {other:?}"),
        };
        assert_eq!(
            held.decided_reason, None,
            "a held proposal has no decision, so no reason"
        );

        // The operator moves the knob while the proposal is held, so its target
        // is more than one gradient step from the parameters now in force. The
        // guards re-run against THOSE, not against the ones it was held on.
        let mut p = StrategyParams::new();
        p.set("cap", dec!(0.41));
        m.set_params("alpha", p, 150_000).unwrap();

        let err = m
            .decide_as(&held.id, Decision::Accept, 200_000, DecidedBy::Auto)
            .expect_err("a stale target must not be adopted");
        assert!(
            err.contains("gradient"),
            "the refusal names the lock: {err}"
        );

        let stored = m
            .all_proposals(10)
            .into_iter()
            .find(|p| p.id == held.id)
            .unwrap();
        assert_eq!(stored.state, ProposalState::Rejected);
        assert_eq!(stored.decided_by, Some(DecidedBy::Auto));
        match stored
            .decided_reason
            .as_ref()
            .expect("a refusal must say why")
        {
            proposal::DecisionReason::GuardFailed { guard, detail } => {
                assert_eq!(guard, "gradient");
                assert!(!detail.is_empty(), "the guard's own words are kept");
            }
            other => panic!("expected a guard refusal, got {other:?}"),
        }
        // The counter the IPC surface reports counts the same refusal the ledger
        // row records: the two readings of this history must not disagree.
        assert_eq!(m.rejected_count("alpha"), 1);
        assert_eq!(m.evolution_count("alpha"), 0);
        let audit = std::fs::read_to_string(dir.join("alpha.jsonl")).unwrap();
        assert!(
            audit.contains("gradient lock no longer holds"),
            "the audit file carries the same refusal: {audit}"
        );

        // A restart folds the ledger back and reports the same numbers — the
        // counters used to reset to 0/0 next to a ledger listing a refusal.
        let m2 = ShadowEvolution::new(fast_cfg(false, "reftreason"), &refs);
        let after = m2
            .all_proposals(10)
            .into_iter()
            .find(|p| p.id == held.id)
            .unwrap();
        assert_eq!(
            after.decided_reason, stored.decided_reason,
            "the reason survives the restart"
        );
        assert_eq!(m2.rejected_count("alpha"), 1);
        assert_eq!(m2.evolution_count("alpha"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #251: an unattended adoption moved the parameters and left NO ledger row,
    /// so the panel's 台账 was empty while the strategy was in fact running a new
    /// version. It is recorded now, as an accepted proposal decided by `auto` —
    /// and the counters, the ledger and the promotion log all agree about it.
    #[test]
    fn an_unattended_adoption_is_a_ledger_row_that_survives_a_restart() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let cfg = fast_cfg(true, "autoledger");
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        // `enable` re-anchors the twins on the clock the test drives, so the
        // synthetic `now` below is not in their past (#250 made the birth stamp
        // real, and a variant born "later" than the tick never qualifies).
        m.enable(0);
        m.set_auto_evolve(true);
        m.on_round(&[market()], &[], 0);
        drive_wins(&mut m, "alpha", 2, 10_000);
        let outcomes = m.evaluate(100_000);
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, EvolutionOutcome::Applied(_))),
            "auto mode adopts: {outcomes:?}"
        );

        let row = m
            .all_proposals(10)
            .into_iter()
            .find(|p| p.decided_by == Some(DecidedBy::Auto))
            .expect("an unattended adoption leaves a ledger row");
        assert_eq!(row.state, ProposalState::Accepted);
        assert!(
            row.id.starts_with("auto-"),
            "and is never mistakable for a decided held proposal: {}",
            row.id
        );
        assert_eq!(
            row.decided_reason, None,
            "an adoption's reason is the comparison block it carries, not a refusal"
        );
        assert_ne!(
            row.from_params.get("cap"),
            row.to_params.get("cap"),
            "the row records a real move"
        );
        assert_eq!(m.evolution_count("alpha"), 1);

        let m2 = ShadowEvolution::new(fast_cfg(true, "autoledger"), &refs);
        assert!(
            m2.all_proposals(10).iter().any(|p| p.id == row.id),
            "the adoption survives the restart"
        );
        assert_eq!(
            m2.evolution_count("alpha"),
            1,
            "and the counter is seeded from the ledger instead of restarting at 0"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #245: an adoption is memory-only during a run, so a restart used to fall
    /// back to the declared defaults while the promotion log still reported the
    /// adopted value — two "current parameters" readings that disagreed. A fresh
    /// manager over the same audit dir restores what was in force, and the
    /// one-click rollback keeps working behind it.
    #[test]
    fn a_restart_restores_the_adopted_parameters() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let cfg = fast_cfg(false, "restore");
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        m.enable(0);
        m.on_round(&[market()], &[], 0);
        drive_wins(&mut m, "alpha", 2, 10_000);
        let held = match &m.evaluate(100_000)[0] {
            EvolutionOutcome::Proposed(p) => p.clone(),
            other => panic!("expected a held proposal, got {other:?}"),
        };
        m.decide(&held.id, Decision::Accept, 200_000).unwrap();
        let adopted = m.registry().get("alpha", "cap").unwrap();
        assert_ne!(adopted, dec!(0.40), "the acceptance moved the knob");

        // The restart publishes the declared default first and then re-applies
        // the promotion, so the two readings agree again.
        let m2 = ShadowEvolution::new(fast_cfg(false, "restore"), &refs);
        assert_eq!(
            m2.registry().get("alpha", "cap").unwrap(),
            adopted,
            "the adopted parameters survive the restart"
        );
        // A third manager still finds the rollback target through the log.
        let mut m3 = ShadowEvolution::new(fast_cfg(false, "restore"), &refs);
        match m3.rollback("alpha", 300_000).unwrap() {
            EvolutionOutcome::RolledBack { to, .. } => {
                assert_eq!(to.get("alpha", "cap"), Some(dec!(0.40)));
            }
            other => panic!("expected a rollback, got {other:?}"),
        }
        assert_eq!(
            m3.registry().get("alpha", "cap").unwrap(),
            dec!(0.40),
            "and restores the pre-promotion value"
        );
        // A rollback is a decision: the next restart must not re-apply what the
        // operator just undid.
        let m4 = ShadowEvolution::new(fast_cfg(false, "restore"), &refs);
        assert_eq!(
            m4.registry().get("alpha", "cap").unwrap(),
            dec!(0.40),
            "a rolled-back promotion is no longer a restore target"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #249: the engine switch is runtime state too. It used to live in memory
    /// only, so every restart silently switched the engine back to whatever the
    /// config file said — that is how "自动进化：开" became a label with zero
    /// variants behind it.
    #[test]
    fn the_engine_switch_survives_a_restart() {
        let (a, _b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a];
        let dir = std::path::PathBuf::from(&fast_cfg(false, "engineswitch").audit_dir);
        let _ = std::fs::remove_dir_all(&dir);

        // The file says off, the operator switches it on.
        let mut m = ShadowEvolution::new(fast_cfg(false, "engineswitch"), &refs);
        assert!(!m.is_enabled());
        m.enable(0);
        let mut m2 = ShadowEvolution::new(fast_cfg(false, "engineswitch"), &refs);
        assert!(m2.is_enabled(), "the runtime switch wins over the file");
        assert_eq!(
            m2.variant_count(),
            3,
            "and the engine returns with its twins"
        );

        // And off is remembered just as firmly, even against a file saying on.
        m2.disable();
        let m3 = ShadowEvolution::new(fast_cfg(true, "engineswitch"), &refs);
        assert!(!m3.is_enabled(), "an operator's off survives the restart");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 72h DEEP clock: the first observation starts it, the interval
    /// suppresses early firings, and the round itself re-anchors every unit's
    /// variant set with COMPOUND mutants (deep-*).
    #[test]
    fn the_deep_cycle_fires_only_after_the_full_interval() {
        let (a, b) = strategies();
        let refs: Vec<&dyn EngineStrategy> = vec![&a, &b];
        let mut cfg = fast_cfg(false, "cycle");
        cfg.evolution_cycle_secs = 100;
        cfg.deep_dims = 2;
        let dir = std::path::PathBuf::from(&cfg.audit_dir);
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = ShadowEvolution::new(cfg, &refs);
        m.enable(0);
        m.on_round(&[market()], &[], 0);
        m.on_tick("t", &book(0.43, 0.45), 1_000);

        // First call starts the clock: no round, and nothing is scheduled yet
        // (the next-fire time is only known once the clock is running).
        assert!(m.maybe_evolution_cycle(2_000).is_none());
        assert!(m.next_cycle_at_ms(2_000).is_some());
        assert!(
            m.variant_views(2_000)
                .iter()
                .all(|v| !v.label.starts_with("deep-")),
            "no deep round before the interval"
        );

        // Inside the interval: nothing fires.
        assert!(m.maybe_evolution_cycle(50_000).is_none());

        // At the interval boundary (clock started at t=2_000 + 100s): the round
        // fires and re-anchors BOTH strategies with deep-*.
        let event = m
            .maybe_evolution_cycle(102_000)
            .expect("the round fires at the boundary");
        assert_eq!(event.cycle_seq, 1);
        assert_eq!(event.dims, 2);
        assert_eq!(event.strategies.len(), 2);
        let names: Vec<_> = m
            .variant_views(100_001)
            .into_iter()
            .filter(|v| !v.is_baseline)
            .map(|v| v.label)
            .collect();
        assert!(
            names.iter().all(|l| l.starts_with("deep-")),
            "the sets are re-anchored with compound mutants: {names:?}"
        );
        // A second call inside the NEW interval does not fire again.
        assert!(m.maybe_evolution_cycle(150_000).is_none());
        assert_eq!(m.cycle_info().1, 1);
    }
}
