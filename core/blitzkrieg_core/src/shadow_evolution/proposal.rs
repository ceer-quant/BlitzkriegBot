//! Shadow Evolution — the promotion proposal (E13 / #95).
//!
//! The evaluator used to apply a winning variant outright. With the promotion
//! workflow the same evaluation produces instead a first-class, serialisable
//! **EvolutionProposal**: what would change (per knob), and the FULL side-by-side
//! comparison the decision rests on — closed trades, win rate, payoff ratio,
//! profit factor and net PnL of the variant against the parameters in force,
//! measured over the same window by the same twin machinery.
//!
//! Persistence (both files under the audit directory, one JSON object per line):
//!   - `proposals.jsonl`  — every proposal and every state transition, appended;
//!     a load folds by id (the LAST line for an id wins), so the file is both
//!     the audit trail and the live state.
//!   - `promotions.jsonl` — one record per adoption (auto or accepted) plus one
//!     per rollback; this is what makes a rollback survive a restart.
//!
//! Decisions come from a human (`shadow_evolution.decide`) or from the
//! auto-evolve switch, and are always re-guarded at decision time: a proposal is
//! an observation, never a bypass.

use super::knobs::StrategyParams;
use super::signal::EvolutionReason;
use super::variants::Metrics;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Who decided a proposal's fate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecidedBy {
    /// A human (accept/reject over IPC, or an operator rollback).
    User,
    /// The auto-evolve switch: the evaluator applied it itself.
    Auto,
}

impl std::fmt::Display for DecidedBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecidedBy::User => write!(f, "user"),
            DecidedBy::Auto => write!(f, "auto"),
        }
    }
}

/// Lifecycle of a proposal. Terminal states: Accepted, Rejected, Expired,
/// Superseded. A Deferred proposal can still be decided later — until its TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    /// Awaiting a human decision.
    Proposed,
    /// Explicitly parked, still decidable until the TTL.
    Deferred,
    Accepted,
    Rejected,
    /// TTL ran out without a decision (the 7-day DryRun verification window).
    Expired,
    /// A fresher proposal for the same strategy replaced it.
    Superseded,
}

impl std::fmt::Display for ProposalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Proposed => "proposed",
            Self::Deferred => "deferred",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Superseded => "superseded",
        })
    }
}

/// Why a proposal reached the state it is in — carried on the record itself so a
/// refusal can be explained after the fact (#251).
///
/// Structured rather than a free-text note: a UI says it in its own language
/// ("梯度锁不通过") while the numbers inside stay verbatim, and a machine reader
/// can branch on `kind` without parsing prose. The audit log keeps its own copy
/// of the same refusal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum DecisionReason {
    /// The acceptance re-ran the guard chain and a lock no longer held — the
    /// parameters in force moved while the proposal was held. `guard` names the
    /// lock that failed (declared / domain / gradient / immutable), `detail` is
    /// the guard's own message, numbers included.
    GuardFailed { guard: String, detail: String },
    /// A refusal on purpose: the operator over IPC, or the auto switch closing
    /// out a proposal whose premises moved.
    Rejected,
    /// The TTL ran out with no decision.
    Expired,
    /// A fresher proposal for the same strategy replaced it.
    Superseded {
        #[serde(rename = "byId")]
        by_id: String,
    },
}

/// The comparison block: one side's economics over the evaluation window.
/// Same figures the offline sweep reports, computed from the twin ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeMetrics {
    pub closed: u32,
    pub wins: u32,
    #[serde(with = "crate::decimal")]
    pub win_rate: rust_decimal::Decimal,
    /// Average win / average loss. 0 when there is no loss side to divide by;
    /// capped at 100 the same way profit_factor is.
    #[serde(with = "crate::decimal")]
    pub payoff: rust_decimal::Decimal,
    #[serde(with = "crate::decimal")]
    pub profit_factor: rust_decimal::Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_usd: rust_decimal::Decimal,
    #[serde(with = "crate::decimal")]
    pub gross_profit_usd: rust_decimal::Decimal,
    #[serde(with = "crate::decimal")]
    pub gross_loss_usd: rust_decimal::Decimal,
}

impl TradeMetrics {
    /// Fold the twin-ledger window metrics into the comparison view.
    pub fn from_window(m: &Metrics) -> Self {
        let losses = m.sample_count.saturating_sub(m.wins);
        let avg_win = if m.wins > 0 {
            m.gross_profit / rust_decimal::Decimal::from(m.wins)
        } else {
            rust_decimal::Decimal::ZERO
        };
        let avg_loss = if losses > 0 {
            m.gross_loss / rust_decimal::Decimal::from(losses)
        } else {
            rust_decimal::Decimal::ZERO
        };
        let payoff = if avg_loss > rust_decimal::Decimal::ZERO {
            avg_win / avg_loss
        } else if avg_win > rust_decimal::Decimal::ZERO {
            rust_decimal::Decimal::from(100)
        } else {
            rust_decimal::Decimal::ZERO
        };
        Self {
            closed: m.sample_count,
            wins: m.wins,
            win_rate: m.win_rate(),
            payoff,
            profit_factor: m.profit_factor(),
            net_pnl_usd: m.total_pnl,
            gross_profit_usd: m.gross_profit,
            gross_loss_usd: m.gross_loss,
        }
    }
}

/// A held-for-decision evolution: one strategy's parameter change plus the
/// evidence it was measured with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvolutionProposal {
    pub id: String,
    pub strategy: String,
    /// Knob names this proposal moves (1 for a directed step, >=2 for a deep
    /// cycle's compound variant) — the evolution's dimension count.
    pub dims: Vec<String>,
    pub from_params: StrategyParams,
    pub to_params: StrategyParams,
    pub baseline: TradeMetrics,
    pub variant: TradeMetrics,
    pub reason: EvolutionReason,
    #[serde(with = "crate::decimal")]
    pub confidence: rust_decimal::Decimal,
    pub sample_count: u32,
    pub created_at_ms: i64,
    /// After this instant an undecided proposal expires (default 7 days).
    pub expires_at_ms: i64,
    pub state: ProposalState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<DecidedBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at_ms: Option<i64>,
    /// Why it ended where it did. Absent while the proposal is still decidable,
    /// and on an adoption (the comparison block is the reason). A refusal without
    /// this field is what made an auto-rejected proposal look like a glitch: the
    /// state said "rejected" and nothing said why (#251).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_reason: Option<DecisionReason>,
    /// Which evolution cycle produced it (0 = continuous windowing).
    pub cycle_seq: u64,
}

impl EvolutionProposal {
    /// A human decision is legal on Proposed and Deferred only — everything
    /// else is terminal and must say so.
    pub fn is_decidable(&self) -> bool {
        matches!(
            self.state,
            ProposalState::Proposed | ProposalState::Deferred
        )
    }
}

/// One adoption (or rollback) of one strategy's parameters. Append-only; the
/// LAST `rollback:false` record for a strategy is what one-click rollback
/// restores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionRecord {
    pub timestamp: i64,
    pub proposal_id: String,
    pub strategy: String,
    pub from_params: StrategyParams,
    pub to_params: StrategyParams,
    pub decided_by: DecidedBy,
    /// `true` on a rollback record (the restore itself), so a rollback never
    /// becomes the promotion a later rollback would restore.
    pub rollback: bool,
    /// The exit-ladder identity this promotion's comparison was measured under
    /// (#269, [`super::config::exit_caliber`]). Without it a promotion says
    /// "better" without saying better *than what*: the twin replays the live
    /// exit policy, so a ladder change silently re-bases every number in the
    /// log. `default` so records written before the field existed still parse
    /// (an empty caliber means "unknown, pre-#269").
    #[serde(default)]
    pub caliber: String,
}

/// JSONL persistence under the audit directory. Append-only files; disk
/// failures are logged and never block trading (same bargain as the audit log).
pub struct ProposalStore {
    dir: PathBuf,
    /// id → latest state, bounded to the most recent [`CAP`] proposals.
    records: BTreeMap<String, EvolutionProposal>,
    /// creation order (for bounded trimming).
    order: Vec<String>,
    cap: usize,
}

const CAP: usize = 500;

impl ProposalStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
            records: BTreeMap::new(),
            order: Vec::new(),
            cap: CAP,
        }
    }

    pub fn proposals_path(&self) -> PathBuf {
        self.dir.join("proposals.jsonl")
    }
    pub fn promotions_path(&self) -> PathBuf {
        self.dir.join("promotions.jsonl")
    }
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// Fold `proposals.jsonl` into memory: the last line per id is the live
    /// state, every line stays on disk as history.
    ///
    /// The fold is read-only, and that is load-bearing. An earlier build folded
    /// through [`Self::put`], whose disk half appends — so every start re-appended
    /// what it had just read and the file doubled per restart. In production that
    /// reached 2.92 GB / 3.3M lines for two live ids, and the resulting startup
    /// cost made the kernel miss its readiness deadline (#243).
    pub fn load(&mut self) {
        let path = self.proposals_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let total = text.lines().filter(|l| !l.trim().is_empty()).count();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(p) = serde_json::from_str::<EvolutionProposal>(line) {
                self.fold(p);
            }
        }
        // A file whose lines are mostly repeat folds of ids the view already holds
        // carries nothing a reader can use, and it is what the NEXT start must read
        // through. Rewrite it as one line per id — the audit and promotion logs own
        // the narrative history; the folded state is what anything reads back.
        if total > 4 * self.records.len().max(1) {
            tracing::warn!(
                lines = total,
                kept = self.records.len(),
                "proposals.jsonl was mostly duplicated folds — rewriting compacted"
            );
            self.compact();
        }
    }

    /// Memory-only half of [`Self::put`]: fold one record into the view and touch
    /// no file. [`Self::load`] uses this — a load that writes is what doubled the
    /// file on every restart.
    fn fold(&mut self, p: EvolutionProposal) {
        let id = p.id.clone();
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        self.records.insert(id, p);
        while self.order.len() > self.cap {
            let dropped = self.order.remove(0);
            self.records.remove(&dropped);
        }
    }

    /// Rewrite `proposals.jsonl` as one line per known id: the folded state, which
    /// is all any reader gets back. tmp+rename like `state.json` — a crash midway
    /// must not truncate the only copy of the live proposals.
    fn compact(&self) {
        let path = self.proposals_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut body = String::new();
        for id in &self.order {
            if let Some(p) = self.records.get(id)
                && let Ok(line) = serde_json::to_string(p)
            {
                body.push_str(&line);
                body.push('\n');
            }
        }
        let tmp = path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    fn append_line(&self, path: &Path, value: &impl Serialize) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(line) = serde_json::to_string(value)
            && let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            use std::io::Write as _;
            let _ = writeln!(f, "{line}");
        }
    }

    /// Record a (new or transitioned) proposal: appends to disk and memory.
    pub fn put(&mut self, p: EvolutionProposal) {
        let id = p.id.clone();
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        self.records.insert(id, p.clone());
        self.append_line(&self.proposals_path(), &p);
        // Bound the in-memory view; the disk file keeps everything.
        while self.order.len() > self.cap {
            let dropped = self.order.remove(0);
            self.records.remove(&dropped);
        }
    }

    pub fn get(&self, id: &str) -> Option<&EvolutionProposal> {
        self.records.get(id)
    }

    /// The one live (Proposed/Deferred) proposal for a strategy, if any.
    pub fn pending_for(&self, strategy: &str) -> Option<&EvolutionProposal> {
        self.records
            .values()
            .filter(|p| p.strategy == strategy && p.is_decidable())
            .max_by_key(|p| p.created_at_ms)
    }

    /// Every decidable proposal, newest first.
    pub fn pending(&self) -> Vec<EvolutionProposal> {
        let mut out: Vec<EvolutionProposal> = self
            .records
            .values()
            .filter(|p| p.is_decidable())
            .cloned()
            .collect();
        out.sort_by_key(|a| std::cmp::Reverse(a.created_at_ms));
        out
    }

    /// The latest states of every known proposal, newest first.
    pub fn latest(&self, limit: usize) -> Vec<EvolutionProposal> {
        let mut out: Vec<EvolutionProposal> = self
            .order
            .iter()
            .filter_map(|id| self.records.get(id))
            .cloned()
            .collect();
        out.sort_by_key(|a| std::cmp::Reverse(a.created_at_ms));
        out.truncate(limit);
        out
    }

    pub fn pending_count(&self) -> usize {
        self.records.values().filter(|p| p.is_decidable()).count()
    }

    /// Expire every decidable proposal past its TTL. Returns how many.
    pub fn expire_stale(&mut self, now_ms: i64) -> usize {
        let mut stale: Vec<EvolutionProposal> = Vec::new();
        for p in self.records.values_mut() {
            if p.is_decidable() && p.expires_at_ms < now_ms {
                p.state = ProposalState::Expired;
                p.decided_at_ms = Some(now_ms);
                p.decided_reason = Some(DecisionReason::Expired);
                stale.push(p.clone());
            }
        }
        for p in &stale {
            self.append_line(&self.proposals_path(), p);
        }
        stale.len()
    }

    /// How many of one strategy's proposals ended adopted, and how many were
    /// refused (`Rejected`). Expired and superseded are neither — they are
    /// visible in the ledger, but nobody said no.
    ///
    /// This is the folded view, which is exactly what a reader gets back, so a
    /// counter seeded from it cannot disagree with the ledger a UI renders —
    /// which is what the in-memory counters did across a restart (#251).
    pub fn terminal_counts(&self, strategy: &str) -> (u64, u64) {
        let mut adopted = 0;
        let mut rejected = 0;
        for p in self.records.values().filter(|p| p.strategy == strategy) {
            match p.state {
                ProposalState::Accepted => adopted += 1,
                ProposalState::Rejected => rejected += 1,
                _ => {}
            }
        }
        (adopted, rejected)
    }

    /// Record an adoption: the promotion the rollback surface restores.
    ///
    /// `caliber` is the exit-ladder identity the comparison was measured under
    /// (#269) — see [`super::config::exit_caliber`].
    // The eight parameters are the record's own fields, one for one; bundling
    // them into a builder would add a type without removing a decision, which is
    // the same trade `variants.rs` and `signal.rs` already make in this module.
    #[allow(clippy::too_many_arguments)]
    pub fn record_promotion(
        &self,
        proposal_id: &str,
        strategy: &str,
        from: &StrategyParams,
        to: &StrategyParams,
        decided_by: DecidedBy,
        now_ms: i64,
        caliber: &str,
    ) {
        self.append_line(
            &self.promotions_path(),
            &PromotionRecord {
                timestamp: now_ms,
                proposal_id: proposal_id.to_string(),
                strategy: strategy.to_string(),
                from_params: from.clone(),
                to_params: to.clone(),
                decided_by,
                rollback: false,
                caliber: caliber.to_string(),
            },
        );
    }

    /// Record a rollback of one strategy (never a restore target itself).
    pub fn record_rollback(
        &self,
        proposal_id: &str,
        strategy: &str,
        from: &StrategyParams,
        to: &StrategyParams,
        now_ms: i64,
        caliber: &str,
    ) {
        self.append_line(
            &self.promotions_path(),
            &PromotionRecord {
                timestamp: now_ms,
                proposal_id: proposal_id.to_string(),
                strategy: strategy.to_string(),
                from_params: from.clone(),
                to_params: to.clone(),
                decided_by: DecidedBy::User,
                rollback: true,
                caliber: caliber.to_string(),
            },
        );
    }

    /// The most recent promotion for a strategy that has not been rolled back
    /// since — the record a cross-restart one-click rollback restores. Reads
    /// the whole file in order: a rollback record for the strategy CLEARS the
    /// restore target, so a second rollback can never re-apply a change the
    /// operator just undid.
    pub fn last_active_promotion(&self, strategy: &str) -> Option<PromotionRecord> {
        let text = std::fs::read_to_string(self.promotions_path()).ok()?;
        let mut hit: Option<PromotionRecord> = None;
        for line in text.lines() {
            let Ok(r) = serde_json::from_str::<PromotionRecord>(line) else {
                continue;
            };
            if r.strategy != strategy {
                continue;
            }
            if r.rollback {
                hit = None;
            } else {
                hit = Some(r);
            }
        }
        hit
    }

    /// Load the runtime evolution state (the two switches + the cycle clock).
    ///
    /// Every field is optional ON READ: a file written by an older kernel has no
    /// `enabled` key, and an absent key must mean "the config file decides" —
    /// never a silent `false` that switches an engine off on restart. `None`
    /// (no file at all) means the same for every field.
    pub fn load_state(&self) -> Option<PersistedState> {
        let text = std::fs::read_to_string(self.state_path()).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        Some(PersistedState {
            enabled: v.get("enabled").and_then(|x| x.as_bool()),
            auto_evolve: v.get("autoEvolve").and_then(|x| x.as_bool()),
            last_cycle_ms: v.get("lastCycleMs").and_then(|x| x.as_i64()).unwrap_or(0),
            cycle_seq: v.get("cycleSeq").and_then(|x| x.as_u64()).unwrap_or(0),
        })
    }

    /// Persist the runtime evolution state (best-effort, atomic replace).
    pub fn save_state(&self, state: PersistedState) {
        let mut body = serde_json::json!({
            "lastCycleMs": state.last_cycle_ms,
            "cycleSeq": state.cycle_seq,
        });
        if let Some(on) = state.enabled {
            body["enabled"] = serde_json::Value::Bool(on);
        }
        if let Some(on) = state.auto_evolve {
            body["autoEvolve"] = serde_json::Value::Bool(on);
        }
        if let Some(parent) = self.state_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(line) = serde_json::to_string(&body) {
            let tmp = self.state_path().with_extension("json.tmp");
            if std::fs::write(&tmp, format!("{line}\n")).is_ok() {
                let _ = std::fs::rename(&tmp, self.state_path());
            }
        }
    }
}

/// The runtime evolution state as it lives on disk (`state.json`).
///
/// Read back with every field optional so a file an older kernel wrote keeps
/// meaning what it meant: the keys it has win, the keys it lacks hand the
/// decision back to the config file (#249).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersistedState {
    /// The engine switch. `None` = the key was absent.
    pub enabled: Option<bool>,
    /// The unattended switch. `None` = the key was absent.
    pub auto_evolve: Option<bool>,
    pub last_cycle_ms: i64,
    pub cycle_seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// The exit-ladder identity these tests stamp on a promotion record.
    const CALIBER: &str = "exit-fnv1a64:0123456789abcdef";

    fn metrics(
        closed: u32,
        wins: u32,
        gp: rust_decimal::Decimal,
        gl: rust_decimal::Decimal,
    ) -> Metrics {
        Metrics {
            sample_count: closed,
            wins,
            gross_profit: gp,
            gross_loss: gl,
            total_pnl: gp - gl,
        }
    }

    fn proposal(id: &str, strategy: &str, created: i64, state: ProposalState) -> EvolutionProposal {
        let mut from = StrategyParams::new();
        from.set("cap", dec!(0.40));
        let mut to = from.clone();
        to.set("cap", dec!(0.412));
        EvolutionProposal {
            id: id.into(),
            strategy: strategy.into(),
            dims: vec!["cap".into()],
            from_params: from,
            to_params: to,
            baseline: TradeMetrics::from_window(&metrics(30, 15, dec!(30), dec!(30))),
            variant: TradeMetrics::from_window(&metrics(30, 24, dec!(48), dec!(12))),
            reason: super::super::signal::EvolutionReason::CombinedImprovement,
            confidence: dec!(0.8),
            sample_count: 30,
            created_at_ms: created,
            expires_at_ms: created + 1000,
            state,
            decided_by: None,
            decided_at_ms: None,
            decided_reason: None,
            cycle_seq: 0,
        }
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bkprop-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn trade_metrics_carries_the_full_comparison() {
        let m = TradeMetrics::from_window(&metrics(10, 7, dec!(21), dec!(9)));
        assert_eq!(m.closed, 10);
        assert_eq!(m.wins, 7);
        assert_eq!(m.win_rate, dec!(0.7));
        // avg win 3 / avg loss 3 → payoff 1
        assert_eq!(m.payoff, dec!(1));
        assert_eq!(m.profit_factor.round_dp(9), dec!(2.333333333));
        assert_eq!(m.net_pnl_usd, dec!(12));

        // No loss side: payoff caps at 100 like the profit factor does.
        let m2 = TradeMetrics::from_window(&metrics(5, 5, dec!(10), rust_decimal::Decimal::ZERO));
        assert_eq!(m2.payoff, rust_decimal::Decimal::from(100));
    }

    #[test]
    fn the_store_folds_by_id_and_keeps_decidable_separate() {
        let dir = tmp_dir("fold");
        let mut store = ProposalStore::new(&dir);
        store.put(proposal("p1", "alpha", 10, ProposalState::Proposed));
        store.put(proposal("p2", "alpha", 20, ProposalState::Proposed));
        // Same id re-appended with a newer state → the fold wins.
        store.put(proposal("p1", "alpha", 10, ProposalState::Rejected));

        assert_eq!(store.pending().len(), 1, "p1 is decided now");
        assert_eq!(store.pending()[0].id, "p2");
        assert_eq!(store.get("p1").unwrap().state, ProposalState::Rejected);
        assert_eq!(store.pending_count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pending_proposal_supersedes_the_previous_one() {
        let dir = tmp_dir("super");
        let mut store = ProposalStore::new(&dir);
        store.put(proposal("p1", "alpha", 10, ProposalState::Proposed));
        let older = store.pending_for("alpha").unwrap().id.clone();
        assert_eq!(older, "p1");
        // A different strategy does not collide.
        store.put(proposal("p3", "beta", 15, ProposalState::Proposed));
        assert_eq!(store.pending().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expiry_moves_proposed_and_deferred_to_expired() {
        let dir = tmp_dir("expire");
        let mut store = ProposalStore::new(&dir);
        store.put(proposal("p1", "alpha", 0, ProposalState::Proposed));
        store.put(proposal("p2", "beta", 0, ProposalState::Deferred));
        store.put(proposal("p3", "gamma", 0, ProposalState::Accepted));
        assert_eq!(store.expire_stale(2000), 2, "terminal states are untouched");
        assert_eq!(store.get("p1").unwrap().state, ProposalState::Expired);
        assert_eq!(store.get("p2").unwrap().state, ProposalState::Expired);
        assert_eq!(store.get("p3").unwrap().state, ProposalState::Accepted);
        assert_eq!(store.pending_count(), 0);

        // Reload folds to the persisted terminal states.
        let mut reloaded = ProposalStore::new(&dir);
        reloaded.load();
        assert_eq!(reloaded.pending().len(), 0);
        assert_eq!(reloaded.get("p1").unwrap().state, ProposalState::Expired);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promotion_log_round_trips_and_rollback_is_not_a_restore_target() {
        let dir = tmp_dir("promo");
        let store = ProposalStore::new(&dir);
        let from = {
            let mut p = StrategyParams::new();
            p.set("cap", dec!(0.40));
            p
        };
        let to = {
            let mut p = StrategyParams::new();
            p.set("cap", dec!(0.412));
            p
        };
        store.record_promotion("p1", "alpha", &from, &to, DecidedBy::User, 100, CALIBER);
        let hit = store.last_active_promotion("alpha").unwrap();
        assert_eq!(hit.proposal_id, "p1");
        assert_eq!(hit.to_params.get("cap"), Some(dec!(0.412)));
        // #269: the ladder the comparison was measured under travels with it.
        assert_eq!(hit.caliber, CALIBER);
        assert!(store.last_active_promotion("beta").is_none());

        // After a rollback the promotion must stop being restorable — otherwise
        // a second rollback would re-apply the change the user just undid.
        store.record_rollback("p1", "alpha", &to, &from, 200, CALIBER);
        assert!(
            store.last_active_promotion("alpha").is_none(),
            "a rolled-back promotion is not a restore target"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #269: a promotion record written before the caliber existed must still
    /// parse — the rollback surface reads the whole file, so a hard failure on
    /// one old line would take the restore target with it.
    #[test]
    fn a_promotion_without_a_caliber_still_loads() {
        let dir = tmp_dir("promo-legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let store = ProposalStore::new(&dir);
        let mut params = StrategyParams::new();
        params.set("cap", dec!(0.40));
        // Exactly the shape a pre-#269 kernel wrote: no `caliber` key at all.
        let legacy = serde_json::json!({
            "timestamp": 1,
            "proposalId": "p0",
            "strategy": "alpha",
            "fromParams": params,
            "toParams": params,
            "decidedBy": "user",
            "rollback": false,
        });
        std::fs::write(store.promotions_path(), format!("{legacy}\n")).unwrap();
        let hit = store
            .last_active_promotion("alpha")
            .expect("a pre-#269 record is still a restore target");
        assert_eq!(hit.caliber, "", "an absent caliber reads as unknown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_runtime_state_round_trips_through_disk() {
        let dir = tmp_dir("state");
        let store = ProposalStore::new(&dir);
        store.save_state(PersistedState {
            enabled: Some(true),
            auto_evolve: Some(true),
            last_cycle_ms: 1234,
            cycle_seq: 7,
        });
        let st = store.load_state().unwrap();
        assert_eq!(st.enabled, Some(true));
        assert_eq!(st.auto_evolve, Some(true));
        assert_eq!(st.last_cycle_ms, 1234);
        assert_eq!(st.cycle_seq, 7);
        // A directory with no state file reads as the defaults.
        let empty = ProposalStore::new(tmp_dir("state-missing"));
        assert!(empty.load_state().is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(tmp_dir("state-missing"));
    }

    /// #249: a file written before the engine switch existed must not read as
    /// "the engine is off" — an absent key means the config file decides, which
    /// is how an upgrade keeps an engine the operator had switched on.
    #[test]
    fn a_state_file_without_the_engine_key_leaves_it_undecided() {
        let dir = tmp_dir("state-legacy");
        let store = ProposalStore::new(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            store.state_path(),
            r#"{"autoEvolve":true,"lastCycleMs":99,"cycleSeq":3}"#,
        )
        .unwrap();
        let st = store.load_state().unwrap();
        assert_eq!(st.enabled, None, "an absent key is not a `false`");
        assert_eq!(st.auto_evolve, Some(true));
        assert_eq!(st.last_cycle_ms, 99);
        assert_eq!(st.cycle_seq, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #243: a load must never write. The earlier build folded through `put`,
    /// whose disk half appends, so the file doubled on every start — production
    /// measured 2.92 GB / 3.3M lines for two live ids, and the kernel missed its
    /// readiness deadline reading it back.
    #[test]
    fn loading_folds_without_reappending_and_compacts_a_flooded_file() {
        let dir = tmp_dir("loadfold");
        let path = {
            let mut store = ProposalStore::new(&dir);
            store.put(proposal("p1", "alpha", 10, ProposalState::Proposed));
            store.put(proposal("p2", "beta", 20, ProposalState::Proposed));
            store.proposals_path()
        };
        let before = std::fs::read_to_string(&path).unwrap().lines().count();

        let mut reloaded = ProposalStore::new(&dir);
        reloaded.load();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            before,
            "load must not append anything"
        );
        assert_eq!(reloaded.pending().len(), 2);

        // The damage a legacy build already wrote: the same folds, repeated.
        // A reload compacts back to one line per id.
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.repeat(30)).unwrap();
        let mut healed = ProposalStore::new(&dir);
        healed.load();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            before,
            "compaction keeps one line per id"
        );
        assert_eq!(healed.pending().len(), 2);

        // A load of an already-clean file leaves no temp file behind.
        assert!(!path.with_extension("jsonl.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
