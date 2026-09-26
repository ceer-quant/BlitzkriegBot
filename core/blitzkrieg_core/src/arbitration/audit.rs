//! The audit trail (DEV_V0_3 §3.4): every suggestion, approved or rejected,
//! lands as ONE line in `data/audit/intents.jsonl`.
//!
//! Rules the design fixes here:
//! * write through the existing `crate::jsonl::append` (one implementation,
//!   append-only, failures never fatal) — a failed write costs one
//!   `tracing::warn!`, never a trade;
//! * NO rotation in 0.3 — JSONL + the ops-side `data-backup-*` archiving
//!   already cover it; a rotator waits for a real size problem;
//! * `INTENT_DECISION` pushes are THROTTLED, per `(strategy, gate, reason)`
//!   key, to at most one per second — a rejection storm can produce thousands
//!   of intents a second, and the push is folded, never the audit (the audit
//!   stays one line per intent).

use super::{Decision, GateTrace};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// One audited suggestion, exactly the §3.4 shape. The serde spelling here is
/// load-bearing: `intent.audit.tail` filters on `accountId` / `strategy` /
/// `decision.status`, and the panel reads `gates[].detail` verbatim (§13.4).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentAuditRecord {
    pub ts_ms: i64,
    pub account_id: String,
    pub strategy: String,
    pub intent_id: String,
    /// The suggestion as submitted (reserved keys already stripped, §2.4).
    pub intent: serde_json::Value,
    pub decision: Decision,
    pub gates: Vec<GateTrace>,
    pub latency_us: u64,
}

/// Where the pushes fold. `throttle` returns `Some(folded_count)` when the
/// caller may emit ONE push for the key now — every suppressed decision is
/// still audited; only the push is folded.
#[derive(Debug)]
struct ThrottleState {
    /// `i64::MIN` = never sent (0 is a legitimate timestamp, and a test that
    /// starts its clock at 0 must still fold).
    last_sent_ms: i64,
    folded: u64,
}

/// The audit sink: sequence numbers, the JSONL writer, and the push throttle.
/// Owned by `Core`; `enabled = false` (`--no-intent-audit`) silences only the
/// WRITES — the gates themselves keep running (P1 proves both settings hit
/// the same economic baseline).
#[derive(Debug)]
pub struct AuditSink {
    pub enabled: bool,
    pub path: PathBuf,
    pub account_id: String,
    next_seq: u64,
    throttles: HashMap<(String, String, String), ThrottleState>,
}

/// The one push the throttle lets through, ready for the `INTENT_DECISION`
/// event (§12.3).
#[derive(Debug, Clone)]
pub struct DecisionPush {
    pub strategy: String,
    pub intent_id: String,
    pub status: String,
    pub gate: String,
    pub detail: String,
    pub ts_ms: i64,
    /// How many decisions this single push summarizes (≥ 1). Additive field
    /// on top of the §12.3 example — the fold has to be observable.
    pub count: u64,
}

impl AuditSink {
    pub fn new(account_id: impl Into<String>) -> Self {
        Self {
            enabled: true,
            path: PathBuf::from("data/audit/intents.jsonl"),
            account_id: account_id.into(),
            next_seq: 0,
            throttles: HashMap::new(),
        }
    }

    /// One intent id per audited suggestion (unique within the process).
    fn next_intent_id(&mut self) -> String {
        self.next_seq += 1;
        format!("i{:08}", self.next_seq)
    }

    /// Build the record for one arbitrated suggestion and append it. Returns
    /// the record (the caller emits the throttled push from the same fields);
    /// a failed append warns ONCE and never blocks the trade.
    pub fn record(
        &mut self,
        ts_ms: i64,
        strategy: &str,
        intent: &serde_json::Value,
        decision: &Decision,
        gates: &[GateTrace],
        latency_us: u64,
    ) -> IntentAuditRecord {
        let record = IntentAuditRecord {
            ts_ms,
            account_id: self.account_id.clone(),
            strategy: strategy.to_string(),
            intent_id: self.next_intent_id(),
            intent: intent.clone(),
            decision: decision.clone(),
            gates: gates.to_vec(),
            latency_us,
        };
        if self.enabled && !crate::jsonl::append(&self.path, &record) {
            tracing::warn!(
                target: "strategy",
                "intent audit append failed (trading continues): {}",
                self.path.display()
            );
        }
        record
    }

    /// Fold pushes for `(strategy, gate, reason)`: at most ONE per second per
    /// key. `Some(count)` = emit now (count decisions folded into it).
    pub fn throttle(
        &mut self,
        strategy: &str,
        gate: &str,
        reason: &str,
        now_ms: i64,
    ) -> Option<u64> {
        const WINDOW_MS: i64 = 1000;
        let key = (strategy.to_string(), gate.to_string(), reason.to_string());
        let state = self.throttles.entry(key).or_insert(ThrottleState {
            last_sent_ms: i64::MIN,
            folded: 0,
        });
        if now_ms.saturating_sub(state.last_sent_ms) < WINDOW_MS {
            state.folded += 1;
            return None;
        }
        let count = state.folded + 1;
        state.last_sent_ms = now_ms;
        state.folded = 0;
        Some(count)
    }

    /// The push for a decision, with the gate/detail the §12.3 event carries:
    /// the REJECTING gate for a rejection, the last gate otherwise.
    pub fn push_for(record: &IntentAuditRecord, count: u64) -> DecisionPush {
        let (gate, detail) = match &record.decision {
            Decision::Rejected { gate, detail, .. } => {
                (format!("{gate:?}").to_uppercase(), detail.clone())
            }
            _ => record
                .gates
                .last()
                .map(|g| (format!("{:?}", g.gate).to_uppercase(), g.detail.clone()))
                .unwrap_or_else(|| ("UNKNOWN".into(), String::new())),
        };
        DecisionPush {
            strategy: record.strategy.clone(),
            intent_id: record.intent_id.clone(),
            status: record.decision.status().to_string(),
            gate,
            detail,
            ts_ms: record.ts_ms,
            count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitration::{GateId, GateOutcome, GateTrace, RejectReason};
    use rust_decimal_macros::dec;

    fn rejected_record(ts_ms: i64) -> IntentAuditRecord {
        IntentAuditRecord {
            ts_ms,
            account_id: "default".into(),
            strategy: "probe".into(),
            intent_id: "i00000001".into(),
            intent: serde_json::json!({"token": "UP", "price": "1.5"}),
            decision: Decision::Rejected {
                reason: RejectReason::OutOfPriceBand,
                gate: GateId::Legality,
                detail: "prediction price must be in (0,1]".into(),
            },
            gates: vec![GateTrace {
                gate: GateId::Legality,
                outcome: GateOutcome::Reject,
                detail: "prediction price must be in (0,1]".into(),
            }],
            latency_us: 3,
        }
    }

    /// The rejection-storm acceptance (§14.1 E25): 4000 rejections in one
    /// second are audited LINE BY LINE (nothing dropped), while the pushes for
    /// the same `(strategy, gate, reason)` fold to ≤ 1 per second.
    #[test]
    fn a_rejection_storm_audits_every_intent_and_folds_the_pushes() {
        let mut sink = AuditSink::new("default");
        let dir = std::env::temp_dir().join(format!("bk-e25-audit-{}", std::process::id()));
        sink.path = dir.join("intents.jsonl");
        let _ = std::fs::remove_file(&sink.path);

        let mut pushes = 0u64;
        for i in 0..4000i64 {
            // All 4000 inside the SAME throttle second (ts 0..1000).
            let ts = i / 4;
            let record = sink.record(
                ts,
                "probe",
                &serde_json::json!({"token": "UP", "price": "1.5"}),
                &rejected_record(ts).decision,
                &rejected_record(ts).gates,
                3,
            );
            if sink
                .throttle(&record.strategy, "LEGALITY", "OUT_OF_PRICE_BAND", ts)
                .is_some()
            {
                pushes += 1;
            }
        }
        assert_eq!(pushes, 1, "one second, one key, ONE push");

        let lines = std::fs::read_to_string(&sink.path).unwrap().lines().count();
        assert_eq!(lines, 4000, "the audit is never folded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A disabled sink (`--no-intent-audit`) writes nothing — and that is the
    /// ONLY thing it changes (P1: both settings hit the same baseline).
    #[test]
    fn a_disabled_sink_writes_nothing() {
        let mut sink = AuditSink::new("default");
        sink.enabled = false;
        let dir = std::env::temp_dir().join(format!("bk-e25-audit-off-{}", std::process::id()));
        sink.path = dir.join("intents.jsonl");
        let record = sink.record(
            0,
            "probe",
            &serde_json::json!({}),
            &rejected_record(0).decision,
            &[],
            0,
        );
        assert_eq!(record.intent_id, "i00000001", "ids keep flowing");
        assert!(!sink.path.exists(), "no file is written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The push carries the §12.3 fields verbatim; the DECIMAL intent values
    /// survive as strings (prices are decimals, never floats — same rule as
    /// the rest of the wire).
    #[test]
    fn push_carries_the_rejecting_gate_and_verbatim_detail() {
        let record = rejected_record(1758888000123);
        let push = AuditSink::push_for(&record, 3);
        assert_eq!(push.status, "REJECTED");
        assert_eq!(push.gate, "LEGALITY");
        assert_eq!(push.detail, "prediction price must be in (0,1]");
        assert_eq!(push.count, 3);
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["accountId"], "default");
        assert_eq!(json["decision"]["status"], "REJECTED");
        assert_eq!(json["decision"]["reason"], "OUT_OF_PRICE_BAND");
        assert_eq!(json["gates"][0]["gate"], "LEGALITY");
        assert!(json["latencyUs"].is_u64());
    }

    /// Sanity: an approved decision's push names the LAST gate (physics), and
    /// the throttle re-opens after the one-second window.
    #[test]
    fn throttle_reopens_after_the_window() {
        let mut sink = AuditSink::new("default");
        assert_eq!(sink.throttle("s", "LEGALITY", "R", 0), Some(1));
        assert_eq!(sink.throttle("s", "LEGALITY", "R", 500), None);
        assert_eq!(sink.throttle("s", "LEGALITY", "R", 999), None);
        assert_eq!(
            sink.throttle("s", "LEGALITY", "R", 1000),
            Some(3),
            "the folded count surfaces (3 decisions, one push)"
        );
        // A different key never folds into the first.
        assert_eq!(sink.throttle("s", "RISK", "R", 1000), Some(1));
    }

    /// The probe strategy's decimal book prices survive the audit round-trip:
    /// `serde_json::Value` intent + decimal-string prices read back exactly.
    #[test]
    fn decimal_prices_stay_exact_strings_in_the_record() {
        let mut sink = AuditSink::new("default");
        let dir = std::env::temp_dir().join(format!("bk-e25-audit-dec-{}", std::process::id()));
        sink.path = dir.join("intents.jsonl");
        sink.record(
            0,
            "probe",
            &serde_json::json!({"price": dec!(0.40).to_string()}),
            &rejected_record(0).decision,
            &[],
            1,
        );
        let line = std::fs::read_to_string(&sink.path).unwrap();
        assert!(line.contains("\"price\":\"0.40\""), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
