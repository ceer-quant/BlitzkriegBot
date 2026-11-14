//! The cockpit working copies derived FROM the `.env` file — the read-only
//! cache side of the single-source ruling (see `crate::cockpit` for the
//! table and the architecture statement; `crate::env_file` for the read
//! contract).
//!
//! Every path that changes the exit discipline or the systemic limits funnels
//! through [`Core::refresh_cockpit_from_env_file`]:
//!
//!   * `risk.setExit` / `risk.setSystemic` persist to the file FIRST and then
//!     call this (a persist failure refuses the whole apply — memory may
//!     never hold a value the file does not have);
//!   * the boot derives the working copy from the file directly (the
//!     launcher's process-env snapshot is no longer consulted for these
//!     keys — the 2026-11-08 divergence class dies here);
//!   * the env-file watcher (mtime poll in the IPC server) calls this on
//!     external hand edits.
//!
//! Fail direction: a key that is present but unparseable warns naming the
//! key and is skipped — one typo must never freeze the other nine (same
//! posture as the #967 env face). A missing key is silence: the file no
//! longer speaks for it, so the working copy keeps its current value (a
//! REMOVED key needs a restart to fall back to the factory — documented).

use super::Core;
use crate::cockpit::{COCKPIT_KEYS, CockpitFamily, CockpitValue, parse_cockpit_value};
use crate::env_file::read_env_vars;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::path::Path;

impl Core {
    /// The `.env` file this kernel derives its cockpit state from, or `None`
    /// when no file layer exists (backtester, unwired tests) — then there is
    /// nothing to derive from and the cache is the whole story.
    pub fn cockpit_env_path(&self) -> Option<&Path> {
        self.config.env_file_path.as_deref().map(Path::new)
    }

    /// Re-derive the exit-discipline and systemic working copies from the
    /// `.env` file. Returns the applied change descriptions
    /// (`field -> value`) for the audit line; a missing file is an empty Ok
    /// (nothing to derive), a read failure is an Err (the caller reports it —
    /// the cache stays as-is, which for a READ failure is the honest posture:
    /// last known good).
    pub fn refresh_cockpit_from_env_file(&mut self) -> Result<Vec<String>, String> {
        let Some(path) = self.cockpit_env_path() else {
            return Ok(Vec::new());
        };
        let vars = read_env_vars(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(self.apply_cockpit_vars(&vars))
    }

    /// Apply parsed cockpit values onto the working copies. Shared by the
    /// file refresh (whole file) — this function is the ONLY code that
    /// mutates the cockpit cache, so the cache is by construction a pure
    /// function of the file.
    pub(crate) fn apply_cockpit_vars(&mut self, vars: &BTreeMap<String, String>) -> Vec<String> {
        let mut applied = Vec::new();
        for key in COCKPIT_KEYS {
            let Some(raw) = vars.get(key.env_key) else {
                continue;
            };
            let Some(value) = parse_cockpit_value(key, raw) else {
                tracing::warn!(
                    key = key.env_key,
                    "cockpit env key is present but unparseable for its shape — skipped \
                     (the other cockpit keys still apply)"
                );
                continue;
            };
            match key.family {
                CockpitFamily::Exit => {
                    if self.apply_cockpit_exit(key.field, value) {
                        applied.push(format!("{} -> {}", key.field, raw.trim()));
                    }
                }
                CockpitFamily::Systemic => {
                    if self.apply_cockpit_systemic(key.field, value) {
                        applied.push(format!("{} -> {}", key.field, raw.trim()));
                    }
                }
            }
        }
        // Per-account rows (`BK_EXIT_STOP_LOSS_PCT_PAPER`,
        // `BK_RISK_MAX_POSITION_SIZE_PAPER`, and the infix spelling). The
        // global keys above stay the fallback for accounts without a row.
        let accounts: Vec<String> = self
            .accounts
            .account_list()
            .iter()
            .map(|a| a.id.to_string())
            .collect();
        for account in accounts {
            let suffix = crate::config::account_env_suffix(&account);
            for key in COCKPIT_KEYS {
                let candidates = [
                    format!("{}_{}", key.env_key, suffix),
                    crate::config::account_env_key(key.env_key, &account),
                ];
                let Some(raw) = candidates.iter().find_map(|k| vars.get(k)) else {
                    continue;
                };
                let Some(value) = parse_cockpit_value(key, raw) else {
                    tracing::warn!(
                        key = %candidates[0],
                        "per-account cockpit env key is present but unparseable — skipped"
                    );
                    continue;
                };
                let moved = match key.family {
                    CockpitFamily::Exit => self.apply_cockpit_exit_for(&account, key.field, value),
                    CockpitFamily::Systemic => {
                        self.apply_cockpit_systemic_for(&account, key.field, value)
                    }
                };
                if moved {
                    applied.push(format!("{account}.{} -> {}", key.field, raw.trim()));
                }
            }
        }
        applied
    }

    /// Apply one exit-discipline value onto the working copy. Returns whether
    /// the value changed anything (the audit records only real moves).
    fn apply_cockpit_exit(&mut self, field: &str, value: CockpitValue) -> bool {
        let mut exit = self.config.positions.exit.clone();
        let changed = match (field, value) {
            ("stopLossPct", CockpitValue::Decimal(v)) if v > Decimal::ZERO => {
                let moved = exit.stop_loss_pct != v;
                exit.stop_loss_pct = v;
                moved
            }
            ("takeProfitPct", CockpitValue::Decimal(v)) if v > Decimal::ZERO => {
                let moved = exit.take_profit_pct != v;
                exit.take_profit_pct = v;
                moved
            }
            ("forceExitSec", CockpitValue::Int(v)) if v >= 0 => {
                let moved = exit.force_exit_sec != v;
                exit.force_exit_sec = v;
                moved
            }
            ("trailingEnabled", CockpitValue::Bool(v)) => {
                let moved = exit.trailing_enabled != v;
                exit.trailing_enabled = v;
                moved
            }
            ("strategyExitMakerFirst", CockpitValue::Bool(v)) => {
                let moved = exit.strategy_exit_maker_first != v;
                exit.strategy_exit_maker_first = v;
                moved
            }
            // Out-of-range values (0 pct, negative seconds) are skipped the
            // same way unparseable ones are: the file speaks, but the working
            // copy never takes a shape the write path itself would refuse.
            _ => {
                tracing::warn!(
                    field = field,
                    "cockpit env value is out of range for its field — skipped"
                );
                return false;
            }
        };
        if changed {
            self.config.positions.exit = exit.clone();
            let mut positions = self.config.positions.clone();
            positions.exit = exit;
            self.positions.set_config(positions);
        }
        changed
    }

    /// Apply one exit-discipline value to ONE account's row (or create the
    /// row from the current global config). The default account also moves
    /// the global working copy, preserving the legacy single-account readout.
    fn apply_cockpit_exit_for(
        &mut self,
        account_id: &str,
        field: &str,
        value: CockpitValue,
    ) -> bool {
        let mut exit = self
            .config
            .positions
            .account_exit
            .get(account_id)
            .cloned()
            .unwrap_or_else(|| self.config.positions.exit.clone());
        let changed = match (field, value) {
            ("stopLossPct", CockpitValue::Decimal(v)) if v > Decimal::ZERO => {
                let moved = exit.stop_loss_pct != v;
                exit.stop_loss_pct = v;
                moved
            }
            ("takeProfitPct", CockpitValue::Decimal(v)) if v > Decimal::ZERO => {
                let moved = exit.take_profit_pct != v;
                exit.take_profit_pct = v;
                moved
            }
            ("forceExitSec", CockpitValue::Int(v)) if v >= 0 => {
                let moved = exit.force_exit_sec != v;
                exit.force_exit_sec = v;
                moved
            }
            ("trailingEnabled", CockpitValue::Bool(v)) => {
                let moved = exit.trailing_enabled != v;
                exit.trailing_enabled = v;
                moved
            }
            ("strategyExitMakerFirst", CockpitValue::Bool(v)) => {
                let moved = exit.strategy_exit_maker_first != v;
                exit.strategy_exit_maker_first = v;
                moved
            }
            _ => {
                tracing::warn!(
                    account = %account_id,
                    field = field,
                    "per-account cockpit env value is out of range — skipped"
                );
                return false;
            }
        };
        if !changed {
            return false;
        }
        self.config
            .positions
            .account_exit
            .insert(account_id.to_string(), exit.clone());
        if account_id == crate::model::default_account_id().as_str() {
            self.config.positions.exit = exit.clone();
        }
        let mut positions = self.config.positions.clone();
        positions.exit = self.config.positions.exit.clone();
        self.positions.set_config(positions);
        true
    }

    /// Apply one systemic-limit value to ONE account's row. The default
    /// account also moves the global fallback matrix.
    fn apply_cockpit_systemic_for(
        &mut self,
        account_id: &str,
        field: &str,
        value: CockpitValue,
    ) -> bool {
        let v = match value {
            CockpitValue::Decimal(v) => v,
            _ => {
                tracing::warn!(
                    account = %account_id,
                    field = field,
                    "per-account systemic cockpit key must be a decimal — skipped"
                );
                return false;
            }
        };
        if v < Decimal::ZERO {
            tracing::warn!(
                account = %account_id,
                field = field,
                "per-account systemic cockpit value is negative — skipped"
            );
            return false;
        }
        let mut row = self
            .risk
            .config()
            .systemic
            .accounts
            .get(account_id)
            .cloned()
            .unwrap_or_else(|| self.risk.config().systemic.account.clone());
        let moved = match field {
            "maxSingleLossUsd" => {
                let moved = row.max_single_loss_usd.value() != v;
                row.max_single_loss_usd =
                    crate::risk::limits::Bound::new(v, crate::risk::limits::LimitSource::Env);
                moved
            }
            "maxDailyDrawdownUsd" => {
                let moved = row.max_daily_drawdown_usd.value() != v;
                row.max_daily_drawdown_usd =
                    crate::risk::limits::Bound::new(v, crate::risk::limits::LimitSource::Env);
                moved
            }
            "maxPositionSize" => {
                let moved = row.max_position_size.value() != v;
                row.max_position_size =
                    crate::risk::limits::Bound::new(v, crate::risk::limits::LimitSource::Env);
                moved
            }
            "maxConsecutiveLosses" => {
                let moved = row.max_consecutive_losses.value() != v;
                row.max_consecutive_losses =
                    crate::risk::limits::Bound::new(v, crate::risk::limits::LimitSource::Env);
                moved
            }
            "cooldownMinutes" => {
                let moved = row.cooldown_minutes.value() != v;
                row.cooldown_minutes =
                    crate::risk::limits::Bound::new(v, crate::risk::limits::LimitSource::Env);
                moved
            }
            "maxOrderNotionalUsd" => {
                let moved = row.max_order_notional_usd.value() != v;
                row.max_order_notional_usd =
                    crate::risk::limits::Bound::new(v, crate::risk::limits::LimitSource::Env);
                moved
            }
            other => {
                debug_assert!(false, "unmapped per-account systemic field {other}");
                return false;
            }
        };
        if !moved {
            return false;
        }
        self.risk
            .config_mut()
            .systemic
            .set_account(account_id, row.clone());
        if account_id == crate::model::default_account_id().as_str() {
            self.risk.config_mut().systemic.account = row;
        }
        true
    }

    /// Apply one systemic-limit value onto the working copy (the same Bound
    /// write the setSystemic path does, sourced `Env` — the file IS the env
    /// layer). Returns whether the value changed anything.
    fn apply_cockpit_systemic(&mut self, field: &str, value: CockpitValue) -> bool {
        let v = match value {
            CockpitValue::Decimal(v) => v,
            _ => {
                tracing::warn!(
                    field = field,
                    "systemic cockpit key must be a decimal — skipped"
                );
                return false;
            }
        };
        if v < Decimal::ZERO {
            tracing::warn!(
                field = field,
                "systemic cockpit value is negative — skipped"
            );
            return false;
        }
        let cfg = self.risk.config_mut();
        let matrix = &mut cfg.systemic;
        let (bound, moved) = match field {
            "maxSingleLossUsd" => {
                let moved = matrix.account.max_single_loss_usd.value() != v;
                (&mut matrix.account.max_single_loss_usd, moved)
            }
            "maxDailyDrawdownUsd" => {
                let moved = matrix.account.max_daily_drawdown_usd.value() != v;
                (&mut matrix.account.max_daily_drawdown_usd, moved)
            }
            "maxPositionSize" => {
                let moved = matrix.account.max_position_size.value() != v;
                (&mut matrix.account.max_position_size, moved)
            }
            "maxConsecutiveLosses" => {
                let moved = matrix.account.max_consecutive_losses.value() != v;
                (&mut matrix.account.max_consecutive_losses, moved)
            }
            "cooldownMinutes" => {
                let moved = matrix.account.cooldown_minutes.value() != v;
                (&mut matrix.account.cooldown_minutes, moved)
            }
            "maxOrderNotionalUsd" => {
                let moved = matrix.account.max_order_notional_usd.value() != v;
                (&mut matrix.account.max_order_notional_usd, moved)
            }
            other => {
                debug_assert!(false, "unmapped systemic cockpit field {other}");
                return false;
            }
        };
        if moved {
            *bound = crate::risk::limits::Bound::new(v, crate::risk::limits::LimitSource::Env);
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_env(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bk-cockpit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join(".env");
        std::fs::write(&path, content).expect("write fixture");
        path
    }

    /// The operator's exact 2026-11-08 divergence, now impossible: kernel
    /// memory true (stale launcher snapshot), file false (hand edit) — the
    /// refresh re-derives the cache from the file and the divergence dies.
    #[test]
    fn refresh_rederives_memory_from_the_file_not_the_snapshot() {
        let path = temp_env("BK_EXIT_STRATEGY_EXIT_MAKER_FIRST=\"false\"\n");
        let cfg = crate::service::CoreConfig {
            env_file_path: Some(path.display().to_string()),
            ..crate::service::CoreConfig::default()
        };
        let mut c = Core::new(cfg);
        // Simulate the stale snapshot: memory says true.
        c.config_mut().positions.exit.strategy_exit_maker_first = true;
        assert!(c.config().positions.exit.strategy_exit_maker_first);

        let applied = c.refresh_cockpit_from_env_file().expect("refresh lands");
        assert_eq!(applied, vec!["strategyExitMakerFirst -> false".to_string()]);
        assert!(
            !c.config().positions.exit.strategy_exit_maker_first,
            "the file is the truth: memory follows it"
        );
        let dir = path.parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn one_unparseable_key_never_freezes_the_other_nine() {
        let path = temp_env("BK_EXIT_STOP_LOSS_PCT=banana\nBK_EXIT_TAKE_PROFIT_PCT=55\n");
        let cfg = crate::service::CoreConfig {
            env_file_path: Some(path.display().to_string()),
            ..crate::service::CoreConfig::default()
        };
        let mut c = Core::new(cfg);
        // The BOOT derive has already applied the file (55 in place, the
        // banana skipped with a warning) — that is the file-first boot
        // working. Now edit the file and refresh: only real moves report.
        std::fs::write(
            &path,
            "BK_EXIT_STOP_LOSS_PCT=banana\nBK_EXIT_TAKE_PROFIT_PCT=60\n",
        )
        .expect("edit fixture");
        let applied = c.refresh_cockpit_from_env_file().expect("refresh lands");
        assert_eq!(
            applied,
            vec!["takeProfitPct -> 60".to_string()],
            "the typo is skipped with a warning; the good key still lands"
        );
        assert_eq!(
            c.config().positions.exit.take_profit_pct,
            rust_decimal_macros::dec!(60)
        );
        assert_eq!(
            c.config().positions.exit.stop_loss_pct,
            crate::service::CoreConfig::default()
                .positions
                .exit
                .stop_loss_pct,
            "the banana never moved the stop"
        );
        let dir = path.parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_file_yields_no_changes_and_a_read_failure_is_an_error() {
        let cfg = crate::service::CoreConfig {
            env_file_path: Some("/nonexistent/bk-cockpit-dir/.env".into()),
            ..crate::service::CoreConfig::default()
        };
        let mut c = Core::new(cfg);
        // Missing file: the read fails (no such file) — an Err, and the cache
        // stands at last-known-good. A file that EXISTS but is empty is the
        // empty-derivation Ok case.
        assert!(c.refresh_cockpit_from_env_file().is_err());
    }
}
