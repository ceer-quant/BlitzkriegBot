//! The per-account risk/exit rows' durable store (issue #1189).
//!
//! The SQLite migration names two tables: `account_risk` (one row per
//! account's systemic bounds) and `account_exit_policy` (one row per
//! account's exit discipline). This module is the repository seam the kernel
//! talks to. When no store is open (`None`), every caller falls back to the
//! env face (`BK_RISK_*` / `BK_EXIT_*` plus the per-account suffixed keys),
//! which is the historical behaviour the tests run under.
//!
//! The schema's `account_risk` table carries the six original columns; the
//! `max_order_notional_usd` bound added by #1186 lives in the adjacent
//! `account_settings` key (`risk.max_order_notional_usd`) so the v1 schema
//! stays unchanged. Both are written in one transaction, so a row is never
//! half-updated.

use crate::db::Database;
use crate::exit_policy::ExitConfig;
use crate::risk::limits::{AccountRiskLimits, Bound, LimitSource};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// The `account_settings` key carrying the per-account order-notional cap.
pub const MAX_ORDER_NOTIONAL_SETTING: &str = "risk.max_order_notional_usd";

/// The durable per-account risk/exit store.
#[derive(Clone)]
pub struct AccountRiskDb {
    db: Database,
}

impl AccountRiskDb {
    /// Open (creating/migrating if needed) the store at `path`.
    pub fn open(path: impl Into<std::path::PathBuf>) -> anyhow::Result<Self> {
        Ok(Self {
            db: Database::open(path.into())?,
        })
    }

    pub fn path(&self) -> &std::path::Path {
        self.db.path()
    }

    /// The underlying connection handle (issue #1199 PR-2): the SQLite
    /// store backend shares THIS connection — one file, one writer — instead
    /// of opening a second handle on the same database.
    pub(crate) fn db(&self) -> &crate::db::Database {
        &self.db
    }

    /// Every `account_risk` row, keyed by account id. A missing row means the
    /// caller's env/global fallback applies — that is `Ok` with an empty map.
    /// A *query* failure (the table is gone, the file unreadable) returns
    /// `Err`; it is never silent-grown into "no rows", which the boot chain
    /// would read as "use the env global" and keep trading with widened limits.
    pub fn load_risk(&self) -> Result<BTreeMap<String, AccountRiskLimits>, String> {
        self.db.with_connection(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT account_id, max_single_loss_usd, max_daily_drawdown_usd,
                        max_position_size, max_consecutive_losses, cooldown_minutes,
                        source
                 FROM account_risk",
                )
                .map_err(|e| format!("account_risk prepare failed: {e}"))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                })
                .map_err(|e| format!("account_risk query_map failed: {e}"))?;
            let mut out = BTreeMap::new();
            for row in rows {
                let (id, single, daily, position, losses, cooldown, source) = match row {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(format!("account_risk row unreadable — fail-closed: {e}"))
                    }
                };
                // The one-shot importer writes placeholder zeros
                // (`source = "imported_env"`) for every BK_RISK_* key that
                // was ABSENT — "we looked and found nothing", not a
                // configured bound. A placeholder is no fact: read it as
                // `Bound::off()` (factory default provenance), never as an
                // env-sourced 0. Overlaying it as env would make readouts
                // claim a provenance that never existed (E26 §4.1: the
                // factory posture is `default`), on the all-zero row for
                // the whole account and on the mixed row per key (issue
                // #1199 PR-2 CI red, risk-systemic E26). A row whose
                // values are ALL placeholders is skipped entirely — the
                // boot chain (env → factory) resolves that account.
                let imported_env = source == "imported_env";
                let dec = |raw: &str| {
                    if imported_env && raw.trim() == "0" {
                        Bound::off()
                    } else {
                        dec_bound(raw)
                    }
                };
                if imported_env
                    && [
                        single.as_str(),
                        daily.as_str(),
                        position.as_str(),
                        losses.as_str(),
                        cooldown.as_str(),
                    ]
                    .iter()
                    .all(|v| v.trim() == "0")
                {
                    continue;
                }
                let mut limits = AccountRiskLimits {
                    max_single_loss_usd: dec(&single),
                    max_daily_drawdown_usd: dec(&daily),
                    max_position_size: dec(&position),
                    max_consecutive_losses: dec(&losses),
                    cooldown_minutes: dec(&cooldown),
                    max_order_notional_usd: Bound::off(),
                };
                // #1186's cap lives in the settings table (the v1 schema has
                // Match on the Result directly to distinguish "key truly absent"
                // (QueryReturnedNoRows -> Bound::off()) from a genuine read error
                // (column gone, db locked/corrupt) which is fail-closed.
                match conn.query_row(
                    "SELECT value FROM account_settings
                         WHERE account_id = ?1 AND key = ?2",
                    [&id, MAX_ORDER_NOTIONAL_SETTING],
                    |r| -> Result<String, _> { r.get(0) },
                ) {
                    Ok(raw) => limits.max_order_notional_usd = dec_bound(&raw),
                    Err(rusqlite::Error::QueryReturnedNoRows) => {}
                    Err(e) => {
                        return Err(format!(
                            "account_settings max_order_notional read failed for {id}: {e}"
                        ))
                    }
                }
                out.insert(id, limits);
            }
            Ok(out)
        })
    }

    /// Every `account_exit_policy` row, deserialized as the account's exit
    /// config (stored rows are authoritative; nothing is layered over `base`
    /// here — the caller overlays the global fallback for missing accounts).
    /// A malformed JSON row is skipped with a warning; the caller's
    /// global/account fallback stands. Zero rows is `Ok` with an empty map
    /// ("this account has no DB row → use the env global") — but a *query*
    /// failure (the table is gone, the file unreadable) is `Err`: it is
    /// never silent-grown into "no rows", which the boot chain would read as
    /// "use the env global" and keep trading with a silently-defaulted exit
    /// discipline (issue #1199 fourth audit P0: the load_risk fix in #1214
    /// applied to the exit twin too).
    pub fn load_exits(&self) -> Result<BTreeMap<String, ExitConfig>, String> {
        self.db.with_connection(|conn| {
            let mut stmt = conn
                .prepare("SELECT account_id, policy_json FROM account_exit_policy")
                .map_err(|e| format!("account_exit_policy prepare failed: {e}"))?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(|e| format!("account_exit_policy query_map failed: {e}"))?;
            let mut out = BTreeMap::new();
            for row in rows {
                let (id, json) = match row {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(format!("account_exit_policy row unreadable — fail-closed: {e}"))
                    }
                };
                match serde_json::from_str::<ExitConfig>(&json) {
                    Ok(exit) => {
                        out.insert(id, exit);
                    }
                    Err(e) => tracing::warn!(
                        account = %id,
                    error = %e,
                    "account_exit_policy row malformed — global exit stays"
                ),
                }
            }
            Ok(out)
        })
    }

    /// Upsert one account's risk row and its order-notional setting in one
    /// transaction (the panel write path).
    pub fn save_risk(&self, account_id: &str, limits: &AccountRiskLimits) -> Result<(), String> {
        self.db.with_connection(|conn| {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| format!("account_risk transaction failed: {e}"))?;
            // The store may be opened before the account book is mirrored
            // into the DB (tests, embedders). `INSERT OR IGNORE` keeps the
            // foreign key satisfied without inventing a second account
            // source: a real row is never overwritten.
            tx.execute(
                "INSERT OR IGNORE INTO accounts
                        (id, name, market_type, status, trading_enabled, updated_at_ms)
                     VALUES (?1, ?1, 'prediction', 'active', 1, ?2)",
                rusqlite::params![account_id, crate::db::now_ms()],
            )
            .map_err(|e| format!("account_risk account ensure failed: {e}"))?;
            tx.execute(
                "INSERT INTO account_risk
                        (account_id, max_single_loss_usd, max_daily_drawdown_usd,
                         max_position_size, max_consecutive_losses, cooldown_minutes,
                         source, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'panel', ?7)
                     ON CONFLICT(account_id) DO UPDATE SET
                        max_single_loss_usd = excluded.max_single_loss_usd,
                        max_daily_drawdown_usd = excluded.max_daily_drawdown_usd,
                        max_position_size = excluded.max_position_size,
                        max_consecutive_losses = excluded.max_consecutive_losses,
                        cooldown_minutes = excluded.cooldown_minutes,
                        source = excluded.source,
                        updated_at_ms = excluded.updated_at_ms",
                rusqlite::params![
                    account_id,
                    limits.max_single_loss_usd.value().to_string(),
                    limits.max_daily_drawdown_usd.value().to_string(),
                    limits.max_position_size.value().to_string(),
                    limits.max_consecutive_losses.value().to_string(),
                    limits.cooldown_minutes.value().to_string(),
                    crate::db::now_ms(),
                ],
            )
            .map_err(|e| format!("account_risk write failed: {e}"))?;
            tx.execute(
                "INSERT INTO account_settings (account_id, key, value, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(account_id, key) DO UPDATE SET
                        value = excluded.value, updated_at_ms = excluded.updated_at_ms",
                rusqlite::params![
                    account_id,
                    MAX_ORDER_NOTIONAL_SETTING,
                    limits.max_order_notional_usd.value().to_string(),
                    crate::db::now_ms(),
                ],
            )
            .map_err(|e| format!("account_risk setting write failed: {e}"))?;
            tx.commit()
                .map_err(|e| format!("account_risk commit failed: {e}"))?;
            Ok(())
        })
    }

    /// Upsert one account's exit-policy row.
    pub fn save_exit(&self, account_id: &str, exit: &ExitConfig) -> Result<(), String> {
        let json = serde_json::to_string(exit)
            .map_err(|e| format!("account_exit_policy serialize failed: {e}"))?;
        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO accounts
                        (id, name, market_type, status, trading_enabled, updated_at_ms)
                     VALUES (?1, ?1, 'prediction', 'active', 1, ?2)",
                rusqlite::params![account_id, crate::db::now_ms()],
            )
            .map_err(|e| format!("account_exit_policy account ensure failed: {e}"))?;
            conn.execute(
                "INSERT INTO account_exit_policy (account_id, policy_json, source, updated_at_ms)
                     VALUES (?1, ?2, 'panel', ?3)
                     ON CONFLICT(account_id) DO UPDATE SET
                        policy_json = excluded.policy_json,
                        source = excluded.source,
                        updated_at_ms = excluded.updated_at_ms",
                rusqlite::params![account_id, json, crate::db::now_ms()],
            )
            .map_err(|e| format!("account_exit_policy write failed: {e}"))?;
            Ok(())
        })
    }
}

impl crate::service::Core {
    /// The budget-ratio conflict warn after a `risk.setLimits` that touched
    /// `maxOrderNotionalPct` (issue #1189 review note 5): a no-op unless
    /// the policy actually conflicts, so the call site stays one line.
    pub(crate) fn warn_budget_conflict_if_needed(
        &self,
        changes: &[crate::schema::RiskLimitChange],
    ) {
        if changes.iter().any(|c| c.field == "maxOrderNotionalPct") {
            let account = self.accounts.active_id().to_string();
            self.warn_if_budget_ratio_conflicts(&account);
        }
    }

    /// Resolve the account a panel patch targets (issue #1189 per-account):
    /// the named account, or the process default when absent. Refuses an
    /// unknown account (fail-closed: never silently a sibling's row).
    pub(crate) fn resolve_set_target(
        &self,
        account_id: Option<&str>,
        op: &str,
    ) -> Result<String, crate::model::CoreError> {
        let target = account_id
            .map(str::to_string)
            .unwrap_or_else(|| self.accounts.active_id().to_string());
        if self
            .accounts
            .account(&crate::account::AccountId::from(target.as_str()))
            .is_none()
        {
            return Err(crate::model::CoreError::new(
                crate::model::CoreErrorCode::InvalidParams,
                format!("{op}: unknown account `{target}`; nothing was applied"),
            ));
        }
        Ok(target)
    }

    /// The systemic write phase (issue #1189 per-account): apply the named
    /// fields onto the target account's row, install it, and mirror it into
    /// the global fallback for the default account so the legacy readout
    /// keeps working. Returns the installed row for the persist phase.
    pub(crate) fn install_account_risk_row(
        &mut self,
        target: &str,
        base: &AccountRiskLimits,
        patch: &crate::schema::SetSystemicLimitsParams,
    ) -> AccountRiskLimits {
        let mut row = base.clone();
        let set = |bound: &mut Bound, to: Option<Decimal>| {
            if let Some(v) = to {
                *bound = Bound::new(v, LimitSource::Flag);
            }
        };
        set(&mut row.max_single_loss_usd, patch.max_single_loss_usd);
        set(
            &mut row.max_daily_drawdown_usd,
            patch.max_daily_drawdown_usd,
        );
        set(&mut row.max_position_size, patch.max_position_size);
        set(
            &mut row.max_consecutive_losses,
            patch.max_consecutive_losses,
        );
        set(&mut row.cooldown_minutes, patch.cooldown_minutes);
        set(
            &mut row.max_order_notional_usd,
            patch.max_order_notional_usd,
        );
        let is_default = target == crate::model::default_account_id().as_str();
        self.risk
            .config_mut()
            .systemic
            .set_account(target, row.clone());
        if is_default {
            self.risk.config_mut().systemic.account = row.clone();
        }
        row
    }

    /// The exit-discipline write phase (issue #1189 per-account): apply the
    /// patch onto the current effective exit config, install it as the
    /// account's row (mirroring the default account into the global), and
    /// hand the row back for the persist phase.
    pub(crate) fn install_account_exit_row(
        &mut self,
        target: &str,
        current: &ExitConfig,
        patch: &crate::schema::SetExitParams,
    ) -> ExitConfig {
        let mut exit = current.clone();
        if let Some(v) = patch.stop_loss_pct {
            exit.stop_loss_pct = v;
        }
        if let Some(v) = patch.take_profit_pct {
            exit.take_profit_pct = v;
        }
        if let Some(sec) = patch.force_exit_sec {
            exit.force_exit_sec = sec;
        }
        if let Some(on) = patch.trailing_enabled {
            exit.trailing_enabled = on;
        }
        if let Some(on) = patch.strategy_exit_maker_first {
            exit.strategy_exit_maker_first = on;
        }
        self.config
            .positions
            .account_exit
            .insert(target.to_string(), exit.clone());
        if target == crate::model::default_account_id().as_str() {
            self.config.positions.exit = exit.clone();
        }
        let mut positions = self.config.positions.clone();
        positions.exit = exit.clone();
        self.positions.set_config(positions);
        exit
    }
}

fn dec_bound(raw: &str) -> Bound {
    match raw.trim().parse::<Decimal>() {
        Ok(v) => Bound::new(v, LimitSource::Env),
        Err(_) => {
            tracing::warn!(raw = %raw, "account risk value unparseable — limit stays off");
            Bound::off()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn temp_db() -> (AccountRiskDb, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bk-account-risk-test-{}-{n}", std::process::id()));
        let path = dir.join("risk.sqlite");
        let db = AccountRiskDb::open(&path).expect("temp db opens");
        (db, dir)
    }

    /// issue #1189: the `account_risk` row round-trips through SQLite, and
    /// two accounts can hold different caps.
    #[test]
    fn account_risk_rows_round_trip_independently() {
        let (db, dir) = temp_db();
        let small = AccountRiskLimits {
            max_position_size: Bound::new(dec!(5), LimitSource::Env),
            ..Default::default()
        };
        let large = AccountRiskLimits {
            max_position_size: Bound::new(dec!(50), LimitSource::Env),
            ..Default::default()
        };
        db.save_risk("small", &small).unwrap();
        db.save_risk("large", &large).unwrap();
        let rows = db.load_risk().unwrap();
        assert_eq!(rows["small"].max_position_size.value(), dec!(5));
        assert_eq!(rows["large"].max_position_size.value(), dec!(50));
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The exit policy row round-trips the five cockpit fields.
    #[test]
    fn account_exit_policy_rows_round_trip() {
        let (db, dir) = temp_db();
        let exit = ExitConfig {
            stop_loss_pct: dec!(8),
            take_profit_pct: dec!(90),
            ..Default::default()
        };
        db.save_exit("paper", &exit).unwrap();
        let rows = db.load_exits().unwrap();
        assert_eq!(rows["paper"].stop_loss_pct, dec!(8));
        assert_eq!(rows["paper"].take_profit_pct, dec!(90));
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A freshly opened store has zero rows: `load_risk` answers `Ok` with an
    /// empty map — absence is not an error. Only a *query* failure is (issue
    /// #1199 P1-1: an empty map reads as "no row → use the env global", so it
    /// must never mask a read failure).
    #[test]
    fn fresh_db_load_risk_is_ok_and_empty() {
        let (db, dir) = temp_db();
        let rows = db.load_risk().unwrap();
        assert!(rows.is_empty(), "zero rows is Ok, not an error: {rows:?}");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A query failure (the `account_risk` table is gone) surfaces as `Err` —
    /// never silent-grown into an empty map the boot chain would misread as
    /// "use the env global" and keep trading with widened limits (issue
    /// #1199 P1-1 red line).
    #[test]
    fn load_risk_query_failure_returns_err() {
        let (db, dir) = temp_db();
        db.db()
            .with_connection(|conn| conn.execute("DROP TABLE account_risk", []))
            .expect("drop the account_risk table");
        let err = db.load_risk().unwrap_err();
        assert!(
            err.contains("account_risk"),
            "the error must name the failing query: {err}"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A freshly opened store has zero exit rows: `load_exits` answers `Ok`
    /// with an empty map — absence is not an error ("no row → use the env
    /// global" is the intended semantics). Only a *query* failure is (issue
    /// #1199 fourth audit P0: an empty map must never mask a read failure,
    /// or the boot chain silently trades the env global's exit discipline).
    #[test]
    fn fresh_db_load_exits_is_ok_and_empty() {
        let (db, dir) = temp_db();
        let rows = db.load_exits().unwrap();
        assert!(rows.is_empty(), "zero rows is Ok, not an error: {rows:?}");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A query failure (the `account_exit_policy` table is gone) surfaces as
    /// `Err` — never silent-grown into an empty map the boot chain would
    /// misread as "use the env global" and keep trading with a
    /// silently-defaulted exit discipline (issue #1199 fourth audit P0 red
    /// line, the exit twin of `load_risk_query_failure_returns_err`).
    #[test]
    fn load_exits_query_failure_returns_err() {
        let (db, dir) = temp_db();
        db.db()
            .with_connection(|conn| conn.execute("DROP TABLE account_exit_policy", []))
            .expect("drop the account_exit_policy table");
        let err = db.load_exits().unwrap_err();
        assert!(
            err.contains("account_exit_policy"),
            "the error must name the failing query: {err}"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }
}
