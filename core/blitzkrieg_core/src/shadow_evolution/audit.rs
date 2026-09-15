//! Shadow Evolution — audit log (per-strategy JSONL on disk + bounded memory).
//!
//! E2-c (#28): each strategy gets its OWN file, `data/evolution/<strategy>.jsonl`.
//! A single shared file would make "which strategy evolved?" a field to parse
//! out; separate files make cross-strategy contamination structurally
//! impossible and let an operator read one strategy's history in isolation.

use super::config::ShadowEvolutionConfig;
use super::knobs::MutableParams;
use super::signal::{EvolutionReason, EvolveSignal};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// One evolution attempt, applied or rejected.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditRecord {
    pub timestamp: i64,
    pub signal_id: String,
    /// The strategy this record belongs to (also the file it was written to).
    pub strategy: String,
    pub from_params: MutableParams,
    pub to_params: MutableParams,
    pub reason: EvolutionReason,
    #[serde(with = "crate::decimal")]
    pub confidence: rust_decimal::Decimal,
    pub sample_count: u32,
    #[serde(with = "crate::decimal")]
    pub expected_improvement: rust_decimal::Decimal,
    pub applied: bool,
    pub gradient_check: String,
    pub immutable_check: String,
    /// When rejected, why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection: Option<String>,
    /// Whether this record is a manual rollback rather than a forward evolution.
    pub rollback: bool,
    /// Whether an operator applied this by hand (`shadow_evolution.apply`) rather
    /// than the evaluator proposing it. Manual overrides used to leave no trace.
    pub manual: bool,
}

/// Per-strategy audit sink: one file per strategy, one bounded history per
/// strategy, all addressed by name.
pub struct AuditLog {
    paths: std::collections::BTreeMap<String, PathBuf>,
    dir: PathBuf,
    cap: usize,
    history: std::collections::BTreeMap<String, VecDeque<AuditRecord>>,
    enabled: bool,
}

impl AuditLog {
    /// An audit log for the strategies in `cfg`, disabled (`enabled=false`) until
    /// a strategy actually declares knobs — an inert evolution engine must not
    /// create files.
    pub fn new(cfg: &ShadowEvolutionConfig) -> Self {
        Self {
            paths: std::collections::BTreeMap::new(),
            dir: PathBuf::from(&cfg.audit_dir),
            cap: 500,
            history: std::collections::BTreeMap::new(),
            enabled: cfg.enabled,
        }
    }

    /// Unused-path constructor kept for tests that want an explicit directory.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        let mut cfg = ShadowEvolutionConfig::default();
        cfg.audit_dir = dir.as_ref().to_string_lossy().into_owned();
        cfg.enabled = true;
        Self::new(&cfg)
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// The path this strategy's records go to.
    pub fn path_for(&self, strategy: &str) -> PathBuf {
        self.paths
            .get(strategy)
            .cloned()
            .unwrap_or_else(|| self.dir.join(format!("{strategy}.jsonl")))
    }

    pub fn record(&mut self, rec: AuditRecord) {
        let strategy = rec.strategy.clone();
        // Best-effort disk append; an unwritable path must never block trading.
        if self.enabled {
            let path = self.path_for(&strategy);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(line) = serde_json::to_string(&rec) {
                use std::io::Write as _;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
        let ring = self.history.entry(strategy).or_default();
        ring.push_back(rec);
        while ring.len() > self.cap {
            ring.pop_front();
        }
    }

    /// Record a rejected proposal (a safety lock tripped).
    pub fn record_rejection(
        &mut self,
        sig: &EvolveSignal,
        reason: String,
        gradient: &str,
        immutable: &str,
    ) {
        self.record(AuditRecord {
            timestamp: sig.timestamp,
            signal_id: sig.signal_id.clone(),
            strategy: sig.strategy.clone(),
            from_params: sig.from_params.clone(),
            to_params: sig.to_params.clone(),
            reason: sig.reason,
            confidence: sig.confidence,
            sample_count: sig.sample_count,
            expected_improvement: sig.expected_improvement,
            applied: false,
            gradient_check: gradient.to_string(),
            immutable_check: immutable.to_string(),
            rejection: Some(reason),
            rollback: false,
            manual: false,
        });
    }

    /// Record a successful application.
    pub fn record_applied(&mut self, sig: &EvolveSignal) {
        self.record(AuditRecord {
            timestamp: sig.timestamp,
            signal_id: sig.signal_id.clone(),
            strategy: sig.strategy.clone(),
            from_params: sig.from_params.clone(),
            to_params: sig.to_params.clone(),
            reason: sig.reason,
            confidence: sig.confidence,
            sample_count: sig.sample_count,
            expected_improvement: sig.expected_improvement,
            applied: true,
            gradient_check: "passed".into(),
            immutable_check: "passed".into(),
            rejection: None,
            rollback: false,
            manual: false,
        });
    }

    /// Record an operator-initiated parameter change (previously untraced).
    pub fn record_manual(
        &mut self,
        strategy: &str,
        timestamp: i64,
        from: &MutableParams,
        to: &MutableParams,
        note: &str,
    ) {
        self.record(AuditRecord {
            timestamp,
            signal_id: format!("manual-{timestamp}"),
            strategy: strategy.to_string(),
            from_params: from.clone(),
            to_params: to.clone(),
            reason: EvolutionReason::CombinedImprovement,
            confidence: rust_decimal::Decimal::ONE,
            sample_count: 0,
            expected_improvement: rust_decimal::Decimal::ZERO,
            applied: true,
            gradient_check: "passed".into(),
            immutable_check: "passed".into(),
            rejection: Some(note.to_string()),
            rollback: false,
            manual: true,
        });
    }

    /// Record a rollback for one strategy.
    pub fn record_rollback(&mut self, strategy: &str, timestamp: i64, from: &MutableParams, to: &MutableParams) {
        self.record(AuditRecord {
            timestamp,
            signal_id: format!("rollback-{timestamp}"),
            strategy: strategy.to_string(),
            from_params: from.clone(),
            to_params: to.clone(),
            reason: EvolutionReason::CombinedImprovement,
            confidence: rust_decimal::Decimal::ONE,
            sample_count: 0,
            expected_improvement: rust_decimal::Decimal::ZERO,
            applied: true,
            gradient_check: "n/a".into(),
            immutable_check: "n/a".into(),
            rejection: None,
            rollback: true,
            manual: true,
        });
    }

    /// Recent records for one strategy (newest last). An unknown strategy has no
    /// history — never another strategy's.
    pub fn recent(&self, strategy: &str, limit: usize) -> Vec<AuditRecord> {
        let Some(ring) = self.history.get(strategy) else { return Vec::new() };
        let n = limit.min(ring.len());
        ring.iter().skip(ring.len() - n).cloned().collect()
    }

    /// Every strategy that has at least one record.
    pub fn strategies(&self) -> Vec<String> {
        self.history.iter().filter(|(_, r)| !r.is_empty()).map(|(k, _)| k.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.history.values().map(|r| r.len()).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn len_for(&self, strategy: &str) -> usize {
        self.history.get(strategy).map(|r| r.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::ShadowEvolutionConfig;
    use super::super::knobs::StrategyParams;
    use super::*;
    use rust_decimal_macros::dec;

    fn params(v: rust_decimal::Decimal) -> MutableParams {
        let mut p = StrategyParams::new();
        p.set("cap", v);
        let mut m = MutableParams::new();
        m.set_strategy("s", p);
        m
    }

    fn sig(strategy: &str) -> EvolveSignal {
        EvolveSignal::new(
            "s1".into(),
            1000,
            strategy.into(),
            params(dec!(0.45)),
            params(dec!(0.46)),
            EvolutionReason::CombinedImprovement,
            dec!(0.8),
            40,
            dec!(0.08),
            "variant-1".into(),
        )
    }

    fn tmp_cfg(tag: &str) -> ShadowEvolutionConfig {
        let mut cfg = ShadowEvolutionConfig::default();
        // An audit log only writes while enabled (D7), which is what makes an
        // inert engine leave no files behind; these tests are about the writing
        // path, so they opt in.
        cfg.enabled = true;
        cfg.audit_dir = std::env::temp_dir()
            .join(format!("bkevo-{}-{tag}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        cfg
    }

    #[test]
    fn each_strategy_gets_its_own_file_and_history() {
        let cfg = tmp_cfg("files");
        let _ = std::fs::remove_dir_all(&cfg.audit_dir);
        let mut log = AuditLog::new(&cfg);
        log.record_applied(&sig("alpha"));
        log.record_rejection(&sig("beta"), "gradient".into(), "failed", "passed");

        // Per-strategy files, each containing only its own records.
        let a = std::fs::read_to_string(cfg.audit_path_for("alpha")).unwrap();
        let b = std::fs::read_to_string(cfg.audit_path_for("beta")).unwrap();
        assert!(a.contains("\"strategy\":\"alpha\""));
        assert!(!a.contains("beta"), "alpha's file must not mention beta");
        assert!(b.contains("\"strategy\":\"beta\""));
        assert!(!b.contains("alpha"), "beta's file must not mention alpha");
        assert!(a.contains("\"applied\":true"));
        assert!(b.contains("\"gradientCheck\":\"failed\""));

        // And the in-memory histories are equally separate.
        assert_eq!(log.recent("alpha", 10).len(), 1);
        assert_eq!(log.recent("beta", 10).len(), 1);
        assert!(log.recent("alpha", 10)[0].applied);
        assert!(!log.recent("beta", 10)[0].applied);
        assert!(log.recent("gamma", 10).is_empty(), "an unknown strategy has no history");
        assert_eq!(log.strategies(), vec!["alpha".to_string(), "beta".to_string()]);
        let _ = std::fs::remove_dir_all(&cfg.audit_dir);
    }

    #[test]
    fn a_disabled_audit_writes_no_files() {
        let cfg = {
            let mut c = tmp_cfg("off");
            c.enabled = false;
            c
        };
        let _ = std::fs::remove_dir_all(&cfg.audit_dir);
        let mut log = AuditLog::new(&cfg);
        log.record_applied(&sig("alpha"));
        assert!(!cfg.audit_path_for("alpha").exists(), "an inert engine must not create files");
        assert_eq!(log.len(), 1, "the in-memory record is still kept for IPC");
        let _ = std::fs::remove_dir_all(&cfg.audit_dir);
    }

    #[test]
    fn manual_and_rollback_records_are_flagged() {
        let cfg = tmp_cfg("flags");
        let mut log = AuditLog::new(&cfg);
        log.record_manual("alpha", 5, &params(dec!(0.45)), &params(dec!(0.46)), "operator");
        log.record_rollback("alpha", 6, &params(dec!(0.46)), &params(dec!(0.45)));
        let r = log.recent("alpha", 10);
        assert!(r[0].manual && !r[0].rollback);
        assert!(r[1].rollback && r[1].manual);
        assert_eq!(r[0].strategy, "alpha");
        let _ = std::fs::remove_dir_all(&cfg.audit_dir);
    }
}
