//! The SQLite-backed store (issue #1199 PR-2): the DB backend behind
//! [`StoreApi`](super::StoreApi).
//!
//! One connection: this store owns the same [`AccountRiskDb`] handle the
//! risk/exit DB-first branch uses — one file, one connection, the `db`
//! module's single-writer discipline (every statement serialised through the
//! connection's mutex). Opening the store opens (creating/migrating if
//! needed) the database; a failure is the caller's fail-closed boot refusal.
//!
//! Domain coverage (the DB-carried domains vs the pending ones):
//!
//!   * **risk / exit** — the DB-carried pair. The boot overlay
//!     (`Core::open_account_risk_db`) reads their rows onto the working
//!     copies, so a write here IS the restart-durable value: the merge
//!     (fields onto the stored row) mirrors the write path's in-memory merge.
//!     The production write port (`risk.setSystemic` / `risk.setExit`) keeps
//!     the whole-row `AccountRiskDb::save_risk` / `save_exit` write while the
//!     default assembly is EnvStore — the ExitConfig carries calibration
//!     fields the wire audit never names, so the full-row write is the exact
//!     pre-seam semantics (the five-field merge here is the carrier for the
//!     swapped backend, and it is parity-proven against the raw pair: the
//!     `account_risk` six-field row, the `account_settings`
//!     order-notional key, `source = 'panel'`, and the exit `policy_json`).
//!   * **settings family** (execution-policy sections, per-account round
//!     duration, the account-book knobs, the arb legs) — the rows land in
//!     the tables the importer fills (`account_settings`, `accounts`,
//!     `strategy_assignments`, `arb_pairs`, `kernel_meta`), which is the
//!     right durable shape. Their runtime READ side is still the env grammar
//!     (the account-book loader, the policy loader) — so this store is NOT
//!     the production backend yet: the default assembly keeps
//!     [`EnvStore`](super::EnvStore) until that read-side migration lands
//!     (issue #1199 PR-3). Installing this store as the production backend
//!     is gated on it; the write methods are implemented and tested so the
//!     gate is the only thing missing.
//!   * **credentials** — a typed refusal: the table stores per-account
//!     credential ENV VAR NAMES plus the AES-256-GCM sealed payload, and the
//!     var-name→account mapping is a PR-3 design decision (the current env
//!     grammar is global). Values never land in plaintext anywhere; a
//!     refusal is the honest face until the mapping exists.
//!
//! Install state (issue #1199 r2, the honest ledger): the default assembly
//! still installs [`EnvStore`](super::EnvStore) (`service/assembly.rs`, the
//! PR-1 line — unchanged here); this backend is constructed nowhere in
//! production yet. It has passed the three-backend parity proof
//! (`tests/store_backend_parity.rs`: one business path — account
//! create/venue/rounds/status/trading/strategies/remove, the settings cell,
//! the risk/exit round-trip — asserted fact-for-fact identical across
//! EnvStore / MemStore / SqliteStore, with the legitimate differences
//! enumerated, not silenced). The remaining gate is the READ-side loader
//! migration (PR-3): the account-book loader
//! (`account::config::load_account_ledgers_single_source` — the boot reads
//! `ACCOUNT_*` env only), the execution-policy loaders
//! (`execution_policy::Policy::load_single_source` / `load_for_reload` —
//! `POLICY_*` env only), the per-account round-duration boot resolution
//! (`ACCOUNT_<ID>_ROUND_DURATION_SEC` in `service/assembly.rs`), the
//! credentials status probe (process env, `market/mod.rs`), and the
//! market-plugin cell read (the launcher's `runtime_config` cell). Until
//! those read the DB, a settings-family write through this store would be
//! write-only: the install gate, not a missing write method.
//!
//! Fail-closed posture (issue #1199 acceptance): the store exists only with
//! a live database — construction IS the open, and an open failure is an
//! error. There is no store-without-a-database state, so no write can
//! silently fall back to a second carrier from inside this backend.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rust_decimal::Decimal;

use crate::account::config::CrossVenueArbConfig;
use crate::account::{AccountStatus, CredentialKeys};
use crate::db::{account_setting_remove, account_setting_set};
use crate::model::{CoreError, CoreErrorCode};
use crate::risk::account_store::AccountRiskDb;
use crate::risk::limits::{AccountRiskLimits, Bound, LimitSource};
use crate::store::FieldChanges;

/// The `account_settings` key carrying the per-account round-length grid
/// (the account-level form of the env's `ACCOUNT_<ID>_ROUND_DURATION_SEC`;
/// the table is already account-keyed, so the key carries no id).
pub const ROUND_DURATION_SETTING: &str = "round.duration_sec";

/// The SQLite-backed [`StoreApi`](super::StoreApi) implementation.
///
/// See the module doc for the domain coverage and the install gate.
#[derive(Clone)]
pub struct SqliteStore {
    risk: AccountRiskDb,
}

impl SqliteStore {
    /// Open (creating/migrating if needed) the store at `path`.
    ///
    /// The open is the only constructor: a failure is an error (fail-closed
    /// — the caller refuses to boot rather than run without a store).
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        Ok(Self {
            risk: AccountRiskDb::open(path)?,
        })
    }

    /// The risk/exit store behind this backend (the same connection — the
    /// boot overlay and the DB-first write branch use it without opening a
    /// second handle on the file).
    pub fn risk_db(&self) -> &AccountRiskDb {
        &self.risk
    }

    /// The path this store's database was opened from.
    pub fn path(&self) -> &std::path::Path {
        self.risk.path()
    }
}

impl SqliteStore {
    /// Merge wire field changes onto an existing risk row (the stored row,
    /// or the factory posture when the account has none) — the DB-side twin
    /// of the service layer's in-memory merge (`install_account_risk_row`).
    /// An unknown field is skipped (a future wire field without a DB column
    /// must not crash the write — the pre-seam rule, kept).
    /// Read-modify-write base row for a risk patch. A store read failure is
    /// propagated (fail-closed): merging onto a silently-empty base could wipe
    /// a stored row the operator can't see.
    fn merge_risk_fields(
        &self,
        account_id: &str,
        fields: &FieldChanges,
    ) -> Result<AccountRiskLimits, String> {
        let mut row = self
            .risk
            .load_risk()
            .map_err(|e| format!("account_risk read failed: {e}"))?
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        for (field, value) in fields {
            let Ok(v) = value.trim().parse::<Decimal>() else {
                tracing::warn!(
                    account = %account_id,
                    field = %field,
                    raw = %value,
                    "risk bound value unparseable — field stays"
                );
                continue;
            };
            let bound = Bound::new(v, LimitSource::Flag);
            match field.as_str() {
                "maxSingleLossUsd" => row.max_single_loss_usd = bound,
                "maxDailyDrawdownUsd" => row.max_daily_drawdown_usd = bound,
                "maxPositionSize" => row.max_position_size = bound,
                "maxConsecutiveLosses" => row.max_consecutive_losses = bound,
                "cooldownMinutes" => row.cooldown_minutes = bound,
                "maxOrderNotionalUsd" => row.max_order_notional_usd = bound,
                other => {
                    tracing::warn!(
                        account = %account_id,
                        field = %other,
                        "unknown risk field — skipped (no DB column)"
                    );
                }
            }
        }
        Ok(row)
    }

    /// Merge wire field changes onto an existing exit-policy row (the stored
    /// JSON row, or the factory posture when the account has none) — the
    /// DB-side twin of `install_account_exit_row`'s merge. A store read
    /// failure is propagated (fail-closed): merging onto a silently-empty
    /// base could wipe a stored row the operator can't see (mirrors
    /// `merge_risk_fields`, issue #1199 fourth audit P0).
    fn merge_exit_fields(
        &self,
        account_id: &str,
        fields: &FieldChanges,
    ) -> Result<crate::exit_policy::ExitConfig, String> {
        let mut exit = self
            .risk
            .load_exits(&crate::exit_policy::ExitConfig::default())
            .map_err(|e| format!("account_exit_policy read failed: {e}"))?
            .get(account_id)
            .cloned()
            .unwrap_or_default();
        for (field, value) in fields {
            match field.as_str() {
                "stopLossPct" => {
                    if let Ok(v) = value.trim().parse::<Decimal>() {
                        exit.stop_loss_pct = v;
                    }
                }
                "takeProfitPct" => {
                    if let Ok(v) = value.trim().parse::<Decimal>() {
                        exit.take_profit_pct = v;
                    }
                }
                "forceExitSec" => {
                    if let Ok(v) = value.trim().parse::<i64>() {
                        exit.force_exit_sec = v;
                    }
                }
                "trailingEnabled" => {
                    if let Ok(v) = value.trim().parse::<bool>() {
                        exit.trailing_enabled = v;
                    }
                }
                "strategyExitMakerFirst" => {
                    if let Ok(v) = value.trim().parse::<bool>() {
                        exit.strategy_exit_maker_first = v;
                    }
                }
                other => {
                    tracing::warn!(
                        account = %account_id,
                        field = %other,
                        "unknown exit field — skipped (no DB column)"
                    );
                }
            }
        }
        Ok(exit)
    }

    /// The settings family's write tail shared by the policy-section and
    /// round-duration methods: each `(key, value)` lands as an
    /// `account_settings` row (a `None` value is the sweep — key gone =
    /// inherit the globals, the env grammar's spelling).
    fn write_settings(
        &self,
        account_id: &str,
        rows: &[(&str, Option<&str>)],
    ) -> Result<(), CoreError> {
        self.risk
            .db()
            .with_connection(|conn| -> anyhow::Result<()> {
                for (key, value) in rows {
                    match value {
                        Some(value) => account_setting_set(conn, account_id, key, value)?,
                        None => account_setting_remove(conn, account_id, key)?,
                    }
                }
                Ok(())
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account settings write failed: {e}"),
                )
            })
    }

    /// Ensure the account row exists (the FK precondition the settings and
    /// account-book writes share; INSERT OR IGNORE — a real row is never
    /// overwritten).
    fn ensure_account(&self, account_id: &str, name: Option<&str>) -> Result<(), CoreError> {
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO accounts
                            (id, name, market_type, status, trading_enabled, updated_at_ms)
                         VALUES (?1, ?2, 'prediction', 'active', 1, ?3)",
                    rusqlite::params![account_id, name.unwrap_or(account_id), crate::db::now_ms(),],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account ensure failed: {e}"),
                )
            })?;
        Ok(())
    }
}

impl super::StoreApi for SqliteStore {
    fn account_writes_enabled(&self) -> bool {
        // The DB is always live here (the open is the constructor); account
        // writes are accepted.
        true
    }

    fn save_account_venue(&self, account_id: &str, venue: &str) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET venue = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    rusqlite::params![account_id, venue, crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account venue write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_create(
        &self,
        account_id: &str,
        name: Option<&str>,
        venue: Option<&str>,
        credential_keys: &CredentialKeys,
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, name)?;
        let venue = venue.unwrap_or("");
        if !venue.is_empty() {
            self.save_account_venue(account_id, venue)?;
        }
        // The credential ENV VAR NAMES ride the account row's credential
        // table (values belong to the credentials domain, never here).
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO account_credentials
                            (account_id, api_key_env, secret_env, passphrase_env,
                             private_key_env, encryption_key_env, updated_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(account_id) DO UPDATE SET
                            api_key_env = excluded.api_key_env,
                            secret_env = excluded.secret_env,
                            passphrase_env = excluded.passphrase_env,
                            private_key_env = excluded.private_key_env,
                            encryption_key_env = excluded.encryption_key_env,
                            updated_at_ms = excluded.updated_at_ms",
                    rusqlite::params![
                        account_id,
                        credential_keys.api_key_env,
                        credential_keys.secret_env,
                        credential_keys.passphrase_env,
                        credential_keys.private_key_env,
                        Option::<&str>::None,
                        crate::db::now_ms(),
                    ],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account credentials names write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn remove_account(&self, account_id: &str) -> Result<(), CoreError> {
        // The FK cascades sweep every child row (risk / exit / settings /
        // strategies / credentials) in the same statement.
        let removed = self
            .risk
            .db()
            .with_connection(|conn| {
                conn.execute("DELETE FROM accounts WHERE id = ?1", [account_id])
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account remove failed: {e}"),
                )
            })?;
        if removed == 0 {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!("the store names no account `{account_id}`"),
            ));
        }
        Ok(())
    }

    fn save_account_trading(&self, account_id: &str, enabled: bool) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET trading_enabled = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    rusqlite::params![account_id, enabled as i64, crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account trading write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_status(
        &self,
        account_id: &str,
        status: &AccountStatus,
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET status = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    rusqlite::params![account_id, status.to_string(), crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account status write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_active(&self, account_id: &str) -> Result<(), CoreError> {
        // ONE write: Active status AND trading-on are both carried by the
        // same row (no half-state is expressible — the pre-seam two-removal
        // window cannot exist here at all).
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET status = 'active', trading_enabled = 1,
                            updated_at_ms = ?2
                     WHERE id = ?1",
                    rusqlite::params![account_id, crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account unfreeze write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_display_account(&self, account_id: &str) -> Result<(), CoreError> {
        // The display pointer is pure presentation — a kernel_meta cell, not
        // an accounting axis (the same split the env grammar spells with
        // ACCOUNT_DISPLAY).
        self.risk
            .db()
            .with_connection(|conn| {
                crate::db::kernel_meta_set(conn, crate::db::META_DISPLAY_ACCOUNT, account_id)
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("display account write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_strategies(
        &self,
        account_id: &str,
        strategies: &[String],
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        self.risk
            .db()
            .with_connection(|conn| {
                // Sweep-then-write: the enablement set is the row set; an
                // empty list clears every assignment (absence = no
                // restriction, the env grammar's spelling).
                conn.execute(
                    "DELETE FROM strategy_assignments WHERE account_id = ?1",
                    [account_id],
                )?;
                for strategy in strategies {
                    conn.execute(
                        "INSERT INTO strategy_assignments
                                (account_id, strategy, enabled, updated_at_ms)
                             VALUES (?1, ?2, 1, ?3)
                             ON CONFLICT(account_id, strategy) DO UPDATE SET
                                enabled = excluded.enabled,
                                updated_at_ms = excluded.updated_at_ms",
                        rusqlite::params![account_id, strategy, crate::db::now_ms()],
                    )?;
                }
                Ok::<(), rusqlite::Error>(())
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account strategies write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_rounds(&self, account_id: &str, rounds: &[i64]) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        let rounds_json = serde_json::to_string(rounds).unwrap_or_else(|_| "[]".to_string());
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET rounds_json = ?2, updated_at_ms = ?3 WHERE id = ?1",
                    rusqlite::params![account_id, rounds_json, crate::db::now_ms()],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account rounds write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_account_opening_equity(
        &self,
        account_id: &str,
        equity: Decimal,
        at_ms: i64,
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        // Write-once: an already-recorded principal is authoritative and is
        // never revised by a later venue report (the env face's rule, kept —
        // the ledger's cash identity depends on it).
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE accounts SET opening_equity = ?2, updated_at_ms = ?3
                     WHERE id = ?1 AND opening_equity IS NULL",
                    rusqlite::params![account_id, equity.to_string(), at_ms],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("account opening equity write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_risk_bounds(
        &self,
        account_id: Option<&str>,
        fields: &FieldChanges,
    ) -> Result<bool, CoreError> {
        let target = account_id
            .map(str::to_string)
            .unwrap_or_else(|| crate::model::default_account_id().as_str().to_string());
        let row = self.merge_risk_fields(&target, fields).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("the account_risk store read failed ({e})"),
            )
        })?;
        self.risk.save_risk(&target, &row).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("the account_risk store write failed ({e})"),
            )
        })?;
        Ok(true)
    }

    fn save_exit(
        &self,
        account_id: Option<&str>,
        fields: &FieldChanges,
    ) -> Result<bool, CoreError> {
        let target = account_id
            .map(str::to_string)
            .unwrap_or_else(|| crate::model::default_account_id().as_str().to_string());
        let exit = self.merge_exit_fields(&target, fields).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("the account_exit_policy store read failed ({e})"),
            )
        })?;
        self.risk.save_exit(&target, &exit).map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("the account_exit_policy store write failed ({e})"),
            )
        })?;
        Ok(true)
    }

    fn save_arb_config(&self, cfg: &CrossVenueArbConfig) -> Result<(), CoreError> {
        let pairs_json = serde_json::to_string(&cfg.pairs).unwrap_or_else(|_| "[]".to_string());
        self.risk
            .db()
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO arb_pairs
                            (id, leg_a_venue, leg_a_account, leg_b_venue, leg_b_account,
                             pairs_json, updated_at_ms)
                     VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(id) DO UPDATE SET
                        leg_a_venue = excluded.leg_a_venue,
                        leg_a_account = excluded.leg_a_account,
                        leg_b_venue = excluded.leg_b_venue,
                        leg_b_account = excluded.leg_b_account,
                        pairs_json = excluded.pairs_json,
                        updated_at_ms = excluded.updated_at_ms",
                    rusqlite::params![
                        cfg.leg_a_venue,
                        cfg.leg_a_account,
                        cfg.leg_b_venue,
                        cfg.leg_b_account,
                        pairs_json,
                        crate::db::now_ms(),
                    ],
                )
            })
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("arb config write failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn clear_arb_config(&self) -> Result<(), CoreError> {
        self.risk
            .db()
            .with_connection(|conn| conn.execute("DELETE FROM arb_pairs WHERE id = 1", []))
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::Internal,
                    format!("arb config clear failed: {e}"),
                )
            })?;
        Ok(())
    }

    fn save_policy_section(
        &self,
        account_id: &str,
        state: &BTreeMap<String, Option<String>>,
    ) -> Result<(), String> {
        // The section's whole footprint, spelled by the env grammar the
        // importer and the policy loader share (the key set is the contract;
        // a `None` value is the sweep — key gone = inherit the globals).
        self.ensure_account(account_id, None)
            .map_err(|e| e.message)?;
        let rows: Vec<(&str, Option<&str>)> = state
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_deref()))
            .collect();
        self.write_settings(account_id, &rows)
            .map_err(|e| e.message)
    }

    fn save_account_round_duration(
        &self,
        account_id: &str,
        seconds: Option<i64>,
    ) -> Result<(), CoreError> {
        self.ensure_account(account_id, None)?;
        match seconds {
            Some(sec) => self.write_settings(
                account_id,
                &[(ROUND_DURATION_SETTING, Some(sec.to_string().as_str()))],
            ),
            None => self.write_settings(account_id, &[(ROUND_DURATION_SETTING, None)]),
        }
    }

    fn save_credentials(
        &self,
        vars: &BTreeMap<String, String>,
        reset_keys: &[&str],
    ) -> Result<(), CoreError> {
        // The account_credentials table stores per-account credential ENV VAR
        // NAMES plus the AES-256-GCM sealed payload — the var-name→account
        // mapping is a PR-3 design decision (the current env grammar is
        // global). A refusal is the honest face: no plaintext value ever
        // lands in the DB, and no half-mapping is invented here.
        let _ = (vars, reset_keys);
        Err(CoreError::new(
            CoreErrorCode::Internal,
            "the sqlite store does not carry credentials yet (issue #1199 PR-3: \
             the per-account var-name mapping and the AES-256-GCM sealing land \
             there); the env carrier still owns this domain",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::StoreApi;
    use super::*;
    use crate::exit_policy::ExitConfig;
    use crate::risk::limits::LimitSource;
    use rust_decimal_macros::dec;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A private on-disk store (the WAL/migration posture is part of the
    /// contract under test — a `:memory:` file would skip both).
    fn temp_store() -> (SqliteStore, std::path::PathBuf) {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bk-sqlite-store-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SqliteStore::open(dir.join("store.sqlite")).expect("temp store opens");
        (store, dir)
    }

    fn fields(pairs: &[(&str, &str)]) -> FieldChanges {
        pairs
            .iter()
            .map(|(f, v)| (f.to_string(), v.to_string()))
            .collect()
    }

    /// The risk-bounds round-trip: a field set merges onto the stored row,
    /// a second set upserts (no duplicate row), and a sibling account never
    /// sees the other's row.
    #[test]
    fn risk_bounds_round_trip_and_isolate_siblings() {
        let (store, dir) = temp_store();
        assert!(
            store
                .save_risk_bounds(Some("paper"), &fields(&[("maxPositionSize", "5")]))
                .unwrap()
        );
        let rows = store.risk.load_risk().unwrap();
        assert_eq!(rows["paper"].max_position_size.value(), dec!(5));
        assert_eq!(
            rows["paper"].max_position_size.source(),
            LimitSource::Env,
            "load_risk spells a stored row's source as env (the read face's \
             provenance label — the row has no source column)"
        );
        // A partial second write merges onto the stored row (the first value
        // survives — the merge is read-modify-write, not replace).
        store
            .save_risk_bounds(Some("paper"), &fields(&[("cooldownMinutes", "30")]))
            .unwrap();
        let rows = store.risk.load_risk().unwrap();
        assert_eq!(rows["paper"].max_position_size.value(), dec!(5));
        assert_eq!(rows["paper"].cooldown_minutes.value(), dec!(30));
        // The default-account face (None) is its own row.
        store
            .save_risk_bounds(None, &fields(&[("maxSingleLossUsd", "25")]))
            .unwrap();
        let rows = store.risk.load_risk().unwrap();
        assert_eq!(
            rows["default"].max_single_loss_usd.value(),
            dec!(25),
            "absent account = the process default"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The exit round-trip: the five cockpit fields land in the stored JSON
    /// row, and a partial second write merges (the first values survive).
    #[test]
    fn exit_fields_round_trip_and_merge() {
        let (store, dir) = temp_store();
        store
            .save_exit(Some("paper"), &fields(&[("stopLossPct", "8")]))
            .unwrap();
        let rows = store.risk.load_exits(&ExitConfig::default()).unwrap();
        assert_eq!(rows["paper"].stop_loss_pct, dec!(8));
        store
            .save_exit(
                Some("paper"),
                &fields(&[("takeProfitPct", "90"), ("trailingEnabled", "true")]),
            )
            .unwrap();
        let rows = store.risk.load_exits(&ExitConfig::default()).unwrap();
        assert_eq!(rows["paper"].stop_loss_pct, dec!(8));
        assert_eq!(rows["paper"].take_profit_pct, dec!(90));
        assert!(rows["paper"].trailing_enabled);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The settings family: policy sections sweep-then-write (a reset field
    /// removes the row), the round duration upserts/clears, and both live in
    /// `account_settings` under the env spelling the importer uses.
    #[test]
    fn policy_section_and_round_duration_land_in_settings() {
        let (store, dir) = temp_store();
        let state = BTreeMap::from([
            (
                "POLICY_BUDGET_RATIO_PAPER".to_string(),
                Some("0.25".to_string()),
            ),
            ("POLICY_MIN_BUDGET_USD_PAPER".to_string(), None),
        ]);
        store.save_policy_section("paper", &state).unwrap();
        let budget = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "paper", "POLICY_BUDGET_RATIO_PAPER")
            })
            .unwrap();
        assert_eq!(budget.as_deref(), Some("0.25"));
        // A None value is a sweep: the key is gone (inherit the globals).
        let swept = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "paper", "POLICY_MIN_BUDGET_USD_PAPER")
            })
            .unwrap();
        assert_eq!(swept, None);

        store
            .save_account_round_duration("paper", Some(300))
            .unwrap();
        let dur = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "paper", ROUND_DURATION_SETTING)
            })
            .unwrap();
        assert_eq!(dur.as_deref(), Some("300"));
        store.save_account_round_duration("paper", None).unwrap();
        let dur = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::account_setting_get(conn, "paper", ROUND_DURATION_SETTING)
            })
            .unwrap();
        assert_eq!(dur, None, "the reset removes the key");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The account-book family: the row updates carry the env grammar's
    /// meaning (trading/status as columns, the strategies set as its row
    /// set, rounds as the JSON array, the opening equity write-once), and
    /// the remove sweep cascades.
    #[test]
    fn account_book_writes_round_trip_and_cascade() {
        let (store, dir) = temp_store();
        store.save_account_venue("paper", "kalshi").unwrap();
        store.save_account_trading("paper", false).unwrap();
        store
            .save_account_status("paper", &AccountStatus::Frozen)
            .unwrap();
        store
            .save_account_strategies("paper", &["spread_arb".to_string()])
            .unwrap();
        store.save_account_rounds("paper", &[900, 3600]).unwrap();
        store
            .save_account_opening_equity("paper", dec!(1234.5), 7)
            .unwrap();
        // Write-once: the second report is ignored.
        store
            .save_account_opening_equity("paper", dec!(999), 8)
            .unwrap();
        store.save_display_account("paper").unwrap();

        let (venue, trading, status, rounds_json, equity) =
            store.risk.db().with_connection(|conn| {
                let venue: String = conn
                    .query_row("SELECT venue FROM accounts WHERE id = 'paper'", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                let trading: i64 = conn
                    .query_row(
                        "SELECT trading_enabled FROM accounts WHERE id = 'paper'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let status: String = conn
                    .query_row("SELECT status FROM accounts WHERE id = 'paper'", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                let rounds_json: String = conn
                    .query_row(
                        "SELECT rounds_json FROM accounts WHERE id = 'paper'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let equity: Option<String> = conn
                    .query_row(
                        "SELECT opening_equity FROM accounts WHERE id = 'paper'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                (venue, trading, status, rounds_json, equity)
            });
        assert_eq!(venue, "kalshi");
        assert_eq!(trading, 0, "trading=false is a column value");
        assert_eq!(status, "frozen");
        assert_eq!(rounds_json, "[900,3600]");
        assert_eq!(
            equity.as_deref(),
            Some("1234.5"),
            "write-once kept the first"
        );

        let display: String = store
            .risk
            .db()
            .with_connection(|conn| {
                crate::db::kernel_meta_get(conn, crate::db::META_DISPLAY_ACCOUNT)
            })
            .unwrap()
            .unwrap();
        assert_eq!(display, "paper");

        let strategies: i64 = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM strategy_assignments WHERE account_id = 'paper'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(strategies, 1);

        // The remove sweep cascades (risk / exit / settings / strategies /
        // credentials all ride the FK).
        store.remove_account("paper").unwrap();
        let left: i64 = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM accounts WHERE id = 'paper'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(left, 0);
        // An unknown remove refuses by name.
        let err = store.remove_account("paper").unwrap_err();
        assert_eq!(err.code, CoreErrorCode::InvalidParams);
        assert!(err.message.contains("paper"), "{err}");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The arb legs: the single-row upsert and the clear.
    #[test]
    fn arb_config_upserts_and_clears() {
        let (store, dir) = temp_store();
        store
            .save_arb_config(&CrossVenueArbConfig {
                leg_a_venue: "kalshi".into(),
                leg_a_account: "paper".into(),
                leg_b_venue: "polymarket".into(),
                leg_b_account: "paper".into(),
                pairs: vec!["BTC".into()],
            })
            .unwrap();
        let pairs_json: String = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row("SELECT pairs_json FROM arb_pairs WHERE id = 1", [], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        assert_eq!(pairs_json, "[\"BTC\"]");
        store.clear_arb_config().unwrap();
        let left: i64 = store
            .risk
            .db()
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM arb_pairs", [], |r| r.get(0))
            })
            .unwrap();
        assert_eq!(left, 0);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The credentials domain refuses with a typed error (no plaintext, no
    /// invented mapping — PR-3 owns it).
    #[test]
    fn credentials_refuse_with_a_typed_error() {
        let (store, dir) = temp_store();
        let err = store
            .save_credentials(
                &BTreeMap::from([("KALSHI_API_KEY".to_string(), "k".to_string())]),
                &[],
            )
            .unwrap_err();
        assert_eq!(err.code, CoreErrorCode::Internal);
        assert!(err.message.contains("PR-3"), "{err}");
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The open is the constructor: an unopenable path is an error (the
    /// fail-closed posture — no store-without-a-database state exists).
    #[test]
    fn open_failure_is_an_error() {
        // A path whose parent is a regular FILE can never be created.
        let dir =
            std::env::temp_dir().join(format!("bk-sqlite-store-open-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        assert!(SqliteStore::open(blocker.join("store.sqlite")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
