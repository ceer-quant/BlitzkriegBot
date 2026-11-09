//! #861: the RUNTIME risk state's durable side file — the same family as the
//! daily-loss budget's `positions.daily-loss.json` (one small JSON doc beside
//! the position log, atomic rewrite, memory authoritative, load never blocks
//! startup).
//!
//! What lives here is the state that used to be SESSION-SCOPED and died with
//! the process (the #854 audit's finding: set a limit → restart → gone):
//!
//! - the hot ENTRY caps `risk.setLimits` wrote (`maxOrderNotional`,
//!   `maxOrderNotionalPct`, `maxOpenNotionalUsd`, `minShares`, `maxShares`);
//! - the GLOBAL kill switch (`killed` + the reason an operator must see);
//! - the freeze book: every account freeze with its reason, plus the
//!   `auto_frozen` stamps that separate kernel circumstantial freezes from
//!   operator verdicts (#685: a manual freeze has no stamp and therefore
//!   never self-lifts, across restarts too).
//!
//! NOT here any more: the systemic bounds `risk.setSystemic` wrote. Their
//! durable carrier is the `.env` file since issue #966 — the write face
//! upserts every changed bound as its `BK_RISK_*` key, and the boot re-arms
//! them through `SystemicRiskLimits::from_env` with the honest `env`
//! provenance (issue #965 reconciliation: one carrier, CLI > env(.env) >
//! default). The file cells this file used to carry stamped every restored
//! bound `flag` — a lie about where the value now comes from — and a stale
//! cell would silently shadow a later hand edit of `.env`. A legacy file's
//! `systemic` map is ignored with a loud warning (see [`load_risk_state_file`]).
//!
//! Tiering (deliberate, documented in the PR): the file records only what the
//! RUNTIME writes hot — a field the operator never touched at runtime stays
//! governed by the boot chain (flag > env > default). A hot value restored at
//! boot OVERRIDES the boot-resolved value for that one field: the operator's
//! last deliberate act outranks yesterday's flags, and #861's acceptance is
//! "kill -9 演练后 caps 恢复".
//!
//! A file that EXISTS but does not parse is fail-CLOSED, never fail-open: no
//! restore (the boot-resolved caps stand), and the corrupt-boot freeze
//! applies — the loud error log is the contract (the daily-loss store's #463
//! posture).

use crate::service::CoreConfig;
use blitzkrieg_market_api::net::now_ms;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;

/// The global kill switch, present only while it is engaged (an absent cell
/// is "not killed" — the same absent-means-quiet posture the round-duration
/// cell uses).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KillCell {
    pub reason: String,
}

/// The persisted envelope. Every map is keyed the way the kernel already
/// names things: limits by their wire (camelCase) field name — the same
/// spelling the `risk.setLimits` audit rows carry — freezes by account id.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct RiskStateFile {
    /// Wall-clock of the last write (observability only).
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) saved_at_ms: i64,
    /// `risk.setLimits` hot values: wire field name → decimal string. Only
    /// fields the runtime actually wrote appear — untouched caps stay
    /// boot-governed.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) limits: BTreeMap<String, String>,
    /// The global kill switch, engaged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) kill: Option<KillCell>,
    // #1045: `account_freezes` / `auto_frozen` retired from this file — account
    // entry gates are pure in-memory (`Core.account_gates`) and a restart starts
    // clean. An OLD file that still carries those keys deserializes fine here
    // (serde ignores unknown keys); the boot restore ignores them, so no freeze
    // is ever replayed and no account is frozen at boot. (The next `save`
    // rewrites the file without those stale rows.)
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// The in-memory mirror of the risk-state file: one cell per core, read once
/// at boot, rewritten whole on every risk mutation (the
/// [`crate::position::DailyLossStore`] save-whole-state pattern — no
/// read-modify-write races, the single-threaded core is the only writer).
#[derive(Debug)]
pub(crate) struct RiskStateCell {
    pub(crate) path: PathBuf,
    pub(crate) file: RiskStateFile,
}

/// What a boot read found. `Corrupt` carries the parse error for the loud
/// error log — the fail-closed contract is that a file which exists and does
/// not parse never reads as "nothing was ever set".
pub(crate) enum RiskStateLoad {
    Missing,
    Ok(RiskStateFile),
    Corrupt(String),
}

pub(crate) fn load_risk_state_file(path: &Path) -> RiskStateLoad {
    let Some(text) = std::fs::read_to_string(path).ok() else {
        return RiskStateLoad::Missing;
    };
    // Issue #965 reconciliation: files written before the systemic bounds
    // moved to `.env` carry a `systemic` map this envelope no longer holds.
    // serde would drop it silently — the bounds' honest carrier is `.env` and
    // a silent drop reads as "nothing was ever set", so name the retirement
    // in the log instead.
    if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&text)
        && raw.get("systemic").is_some()
    {
        tracing::warn!(
            path = %path.display(),
            "risk-state file carries retired systemic cells — ignored; the bounds' \
             durable carrier is .env (BK_RISK_* keys, issue #966/#965)"
        );
    }
    match serde_json::from_str::<RiskStateFile>(&text) {
        Ok(file) => RiskStateLoad::Ok(file),
        Err(e) => RiskStateLoad::Corrupt(e.to_string()),
    }
}

impl RiskStateCell {
    /// Rewrite the whole file atomically (fsync + rename, the #463 discipline:
    /// this file IS part of the "a restart cannot launder the risk posture"
    /// guarantee — a truncate-in-place write is the crash window that defeats
    /// it). Best-effort like every sibling store: a failed write warns and
    /// memory stays authoritative; the caller surfaces the outcome in the
    /// reply's `persisted` field where one exists.
    fn save(&self) -> bool {
        match serde_json::to_string_pretty(&self.file) {
            Ok(text) => {
                if !crate::jsonl::rewrite_atomic(&self.path, text.as_bytes()) {
                    tracing::warn!(
                        path = %self.path.display(),
                        "risk-state persist failed — the next restart will not see this state"
                    );
                    return false;
                }
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "risk-state serialize failed");
                false
            }
        }
    }
}

/// Parse one decimal-string cell. A malformed or negative value is skipped
/// with a warning — a hand-edited file must not silently arm a nonsense cap,
/// and the boot-resolved value standing is the fail-closed outcome.
fn parse_decimal_cell(field: &str, raw: &str, path: &Path) -> Option<Decimal> {
    match raw.parse::<Decimal>() {
        Ok(v) if v >= Decimal::ZERO => Some(v),
        Ok(_) => {
            tracing::warn!(
                path = %path.display(),
                field,
                value = %raw,
                "risk-state file holds a negative {field} — ignored, the boot value stands"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                path = %path.display(),
                field,
                value = %raw,
                "risk-state file holds a malformed {field} — ignored, the boot value stands"
            );
            None
        }
    }
}

/// Apply the restored ENTRY caps onto the boot config, BEFORE it moves into
/// the core (the gate the boot builds then carries the restored values).
/// Called only for a file that parsed. Only fields the runtime actually
/// wrote are applied — everything else keeps the boot chain. The systemic
/// bounds are NOT restored from here any more (issue #965): their durable
/// carrier is `.env`, and every boot-armed bound names `env` straight from
/// `SystemicRiskLimits::from_env` — no file cell is consulted.
pub(crate) fn apply_restored_limits(config: &mut CoreConfig, file: &RiskStateFile, path: &Path) {
    for (field, raw) in &file.limits {
        let Some(v) = parse_decimal_cell(field, raw, path) else {
            continue;
        };
        match field.as_str() {
            "maxOrderNotional" => config.risk.max_order_notional = v,
            "maxOrderNotionalPct" => config.risk.max_order_notional_pct = v,
            "maxOpenNotionalUsd" => config.risk.max_open_notional_usd = v,
            "minShares" => config.min_shares = v,
            "maxShares" => config.max_shares = v,
            other => tracing::warn!(
                path = %path.display(),
                field = %other,
                "risk-state file holds an unknown limits field — ignored"
            ),
        }
    }
}

impl Core {
    /// Rewrite the risk-state file from the in-memory cell. `false` (no cell)
    /// is the honest "this kernel has no risk-state file" — the same posture
    /// the `persisted` reply field has always reported for hot updates.
    pub(crate) fn persist_risk_state(&mut self) -> bool {
        match &self.risk_state {
            Some(cell) => cell.save(),
            None => false,
        }
    }

    /// Record a `risk.setLimits` hot write (each changed field → its new
    /// value) and persist. Returns whether the write landed — the reply's
    /// `persisted` field.
    pub(crate) fn risk_state_record_limits(
        &mut self,
        changes: &[crate::schema::RiskLimitChange],
    ) -> bool {
        let Some(cell) = &mut self.risk_state else {
            return false;
        };
        for c in changes {
            cell.file
                .limits
                .insert(c.field.to_string(), c.to.to_string());
        }
        cell.file.saved_at_ms = now_ms();
        cell.save()
    }

    /// The boot half of #861, called once from [`Core::with_account_book`]
    /// after the struct is built: restore the kill switch and the freeze
    /// book from the loaded file, then persist the boot state.
    /// `state_file_present` is true only for a file that PARSED (the corrupt
    /// case restored nothing and gets the fail-closed freeze instead —
    /// `freeze_all_accounts_corrupt_boot`). A clean boot does NOT
    /// default-freeze accounts (「重启就不需要再维持冻结状态」operator
    /// ruling, issue #928): the freeze book on the disk is the only freeze
    /// that survives a restart.
    pub(crate) fn restore_risk_state_at_boot(&mut self, state_file_present: bool) {
        // Snapshot everything the restore needs out of the cell FIRST — the
        // restore itself runs the freeze paths, which take `&mut self`.
        let (kill, _path) = {
            let Some(cell) = &self.risk_state else {
                return;
            };
            if !state_file_present {
                return;
            }
            (cell.file.kill.clone(), cell.path.display().to_string())
        };
        // 1) the global kill switch: a kernel that went down killed stays
        //    killed — the reason on the file is the one the operator saw.
        if let Some(k) = kill {
            self.freeze_trading(format!("restored across restart: {}", k.reason), now_ms());
        }
        // #1045: account entry gates are pure in-memory and a restart starts
        // clean. An OLD risk-state file that still carries `account_freezes` /
        // `auto_frozen` keys is read without error (serde ignores unknown keys)
        // and those keys are deliberately NOT replayed: no freeze is restored,
        // no account is frozen at boot, and the file's stale rows are simply
        // ignored (the next `save` rewrites the file without them). The daily
        // loss breaker is re-derived from the persisted daily budget on the
        // first tick (`emit_daily_trip` re-lifts the `DailyLoss` gate for any
        // account whose realized loss still exceeds the cap)—"按账本推导",
        // 不是「回放冻结」。
    }

    /// The corrupt-boot fail-closed freeze (issue #861's surviving clause):
    /// every account freezes when the risk-state file exists but does not
    /// parse — a kernel that cannot read its own risk posture must not
    /// quietly resume trading. Called from [`Core::with_account_book`] ONLY
    /// for the `RiskStateLoad::Corrupt` case; a missing file and a clean
    /// parse freeze nobody (#928).
    pub(crate) fn freeze_all_accounts_corrupt_boot(&mut self) {
        let reason = "risk-state file unreadable — risk posture unverifiable until resumed \
                      (fail-closed, issue #861)"
            .to_string();
        let accounts: Vec<String> = self
            .accounts
            .account_list()
            .iter()
            .map(|a| a.id.as_str().to_string())
            .collect();
        for id in &accounts {
            self.freeze_account(id, reason.clone());
        }
        self.persist_risk_state();
    }
}

#[cfg(test)]
mod tests;
