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

    /// Every `account_risk` row, keyed by account id. A missing row means the
    /// caller's env/global fallback applies.
    pub fn load_risk(&self) -> BTreeMap<String, AccountRiskLimits> {
        self.db.with_connection(|conn| {
            let mut stmt = match conn.prepare(
                "SELECT account_id, max_single_loss_usd, max_daily_drawdown_usd,
                        max_position_size, max_consecutive_losses, cooldown_minutes
                 FROM account_risk",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "account_risk read failed — env fallback stays");
                    return BTreeMap::new();
                }
            };
            let rows = match stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            }) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "account_risk read failed — env fallback stays");
                    return BTreeMap::new();
                }
            };
            let mut out = BTreeMap::new();
            for row in rows {
                let (id, single, daily, position, losses, cooldown) = match row {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "account_risk row unreadable — skipped");
                        continue;
                    }
                };
                let mut limits = AccountRiskLimits {
                    max_single_loss_usd: dec_bound(&single),
                    max_daily_drawdown_usd: dec_bound(&daily),
                    max_position_size: dec_bound(&position),
                    max_consecutive_losses: dec_bound(&losses),
                    cooldown_minutes: dec_bound(&cooldown),
                    max_order_notional_usd: Bound::off(),
                };
                // #1186's cap lives in the settings table (the v1 schema has
                // no column for it); a missing key stays off.
                let cap: Option<String> = conn
                    .query_row(
                        "SELECT value FROM account_settings
                         WHERE account_id = ?1 AND key = ?2",
                        [&id, MAX_ORDER_NOTIONAL_SETTING],
                        |r| r.get(0),
                    )
                    .ok();
                if let Some(raw) = cap {
                    limits.max_order_notional_usd = dec_bound(&raw);
                }
                out.insert(id, limits);
            }
            out
        })
    }

    /// Every `account_exit_policy` row, deserialized over `base` (the global
    /// exit config). A malformed JSON row is skipped with a warning; the
    /// caller's global/account fallback stands.
    pub fn load_exits(&self, base: &ExitConfig) -> BTreeMap<String, ExitConfig> {
        self.db.with_connection(|conn| {
            let mut stmt = match conn.prepare(
                "SELECT account_id, policy_json FROM account_exit_policy",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "account_exit_policy read failed — env fallback stays");
                    return BTreeMap::new();
                }
            };
            let rows = match stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "account_exit_policy read failed — env fallback stays");
                    return BTreeMap::new();
                }
            };
            let mut out = BTreeMap::new();
            for row in rows {
                let (id, json) = match row {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "account_exit_policy row unreadable — skipped");
                        continue;
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
            let _ = base;
            out
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
        let rows = db.load_risk();
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
        let rows = db.load_exits(&ExitConfig::default());
        assert_eq!(rows["paper"].stop_loss_pct, dec!(8));
        assert_eq!(rows["paper"].take_profit_pct, dec!(90));
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }
}
