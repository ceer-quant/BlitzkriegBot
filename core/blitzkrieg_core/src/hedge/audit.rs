//! The hedge audit trail (issue #426, item 8): every two-leg lifecycle
//! event — proposed → submitted → filled/failed → protective action →
//! closed — is ONE line in the hedge audit JSONL, through the shared
//! `crate::jsonl` rules (append-only, failures never fatal).
//!
//! Same shape philosophy as the intent audit (`arbitration::audit`): the
//! serde spelling is load-bearing, the sink is owned by `Core`, and a
//! failed write costs a `tracing::warn!`, never a hedge.

use serde::Serialize;
use std::path::PathBuf;

use super::ProtectiveKind;
use super::{HedgeLegId, LegSide};

/// One audited hedge lifecycle event. `action` is the closed vocabulary the
/// report and any panel filter will key on.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct HedgeAuditRecord {
    pub ts_ms: i64,
    pub execution_id: String,
    pub event_id: String,
    pub action: &'static str,
    /// Which leg the action concerns (`None` for whole-execution actions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leg: Option<HedgeLegId>,
    /// Side of the leg, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<LegSide>,
    /// `re_hedge` / `force_unwind`, when the action is protective.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protective: Option<ProtectiveKind>,
    /// Free-form detail: prices, sizes, reasons. Names real quantities, not
    /// prose — the mock-venue fault-matrix report greps these.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Where hedge audit lines land. Owned by the hedge surface; disabled in
/// tests/backtests by passing a `None` path (state machine still runs).
#[derive(Debug)]
pub struct HedgeAuditSink {
    pub enabled: bool,
    pub path: Option<PathBuf>,
}

impl HedgeAuditSink {
    /// A sink writing to `path`; `None` disables writes entirely (the
    /// backtester and unit tests use this — no disk, no noise).
    pub fn at(path: Option<PathBuf>) -> Self {
        Self {
            enabled: path.is_some(),
            path,
        }
    }

    /// Append one record. Best effort by construction: a failed write is a
    /// `warn`, never a trading error (shared jsonl contract).
    pub fn record(&self, rec: &HedgeAuditRecord) {
        if !self.enabled {
            return;
        }
        let Some(path) = &self.path else {
            return;
        };
        if !crate::jsonl::append(path, rec) {
            tracing::warn!(
                path = %path.display(),
                execution = %rec.execution_id,
                "hedge audit line failed to persist (state stays authoritative)"
            );
        }
    }
}
