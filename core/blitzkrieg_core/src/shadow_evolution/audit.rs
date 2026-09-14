//! Shadow Evolution — audit log (JSONL on disk + bounded in-memory history).

use super::config::MutableParams;
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
}

pub struct AuditLog {
    path: PathBuf,
    history: VecDeque<AuditRecord>,
    cap: usize,
}

impl AuditLog {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_path_buf(), history: VecDeque::new(), cap: 500 }
    }

    pub fn record(&mut self, rec: AuditRecord) {
        // Append to disk (best effort), then keep in the bounded ring buffer.
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(line) = serde_json::to_string(&rec) {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
                let _ = writeln!(f, "{line}");
            }
        }
        self.history.push_back(rec);
        if self.history.len() > self.cap {
            self.history.pop_front();
        }
    }

    /// Record a rejected proposal (safety lock tripped).
    pub fn record_rejection(&mut self, sig: &EvolveSignal, reason: String, gradient: &str, immutable: &str) {
        self.record(AuditRecord {
            timestamp: sig.timestamp,
            signal_id: sig.signal_id.clone(),
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
        });
    }

    /// Record a successful application.
    pub fn record_applied(&mut self, sig: &EvolveSignal) {
        self.record(AuditRecord {
            timestamp: sig.timestamp,
            signal_id: sig.signal_id.clone(),
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
        });
    }

    /// Recent records (newest last), up to `limit`.
    pub fn recent(&self, limit: usize) -> Vec<AuditRecord> {
        let n = limit.min(self.history.len());
        self.history.iter().skip(self.history.len() - n).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::signal::{EvolutionReason, EvolveSignal};
    use rust_decimal_macros::dec;

    fn sig() -> EvolveSignal {
        EvolveSignal::new(
            "s1".into(),
            1000,
            MutableParams::default(),
            MutableParams::default().scaled(dec!(1.03)),
            EvolutionReason::CombinedImprovement,
            dec!(0.8),
            40,
            dec!(0.08),
            "variant-1".into(),
        )
    }

    #[test]
    fn records_and_returns_recent() {
        let dir = std::env::temp_dir().join(format!("bkevo-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut log = AuditLog::new(&path);
        log.record_applied(&sig());
        log.record_rejection(&sig(), "gradient".into(), "failed", "passed");
        assert_eq!(log.len(), 2);
        let recent = log.recent(10);
        assert_eq!(recent.len(), 2);
        assert!(recent[0].applied);
        assert!(!recent[1].applied);
        assert_eq!(recent[1].rejection.as_deref(), Some("gradient"));
        // File written.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"applied\":true"));
        assert!(text.contains("\"gradientCheck\":\"failed\""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
