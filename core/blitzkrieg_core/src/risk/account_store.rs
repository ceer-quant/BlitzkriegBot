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

/// The `account_settings` key carrying an account's round-duration anchor in
/// seconds — the `ACCOUNT_<ID>_ROUND_DURATION_SEC` key's DB twin (same
/// spelling the SQLite store backend uses, one name for one fact).
pub const ROUND_DURATION_SETTING: &str = "round.duration_sec";

/// The `account_settings` key carrying an account's strategy ENABLEMENT set
/// as a JSON array of names — the `ACCOUNT_<ID>_STRATEGIES` key's DB twin.
pub const STRATEGIES_SETTING: &str = "strategies";

/// `kernel_meta` key: Shadow Evolution on/off (process-scoped, not per-account).
pub const SHADOW_ENABLED_META: &str = "shadow_enabled";
/// `kernel_meta` key: Shadow Evolution unattended application on/off.
pub const SHADOW_AUTO_EVOLVE_META: &str = "shadow_auto_evolve";

/// `kernel_meta` key prefix for UI-submitted credential values:
/// `credential.<ENV_VAR_NAME>` → AES-256-GCM sealed blob, base64-encoded
/// (nonce || ciphertext || tag). The master key is `BLITZKRIEG_DB_KEY`;
/// the plaintext value never lands anywhere on disk.
pub const CREDENTIAL_META_PREFIX: &str = "credential.";

/// One row of the `accounts` table — the durable account book's raw shape.
/// The boot parses these into `Account` values (fail-closed on malformed
/// cells: a book that is not what was written must not trade).
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub id: String,
    pub name: String,
    pub market_type: String,
    pub status: String,
    pub venue: String,
    pub trading_enabled: bool,
    pub rounds_json: String,
    pub opening_equity: Option<String>,
    pub updated_at_ms: i64,
}

/// One row of the `account_credentials` table: the credential ENV VAR
/// NAMES an account binds (values live in the encrypted `kernel_meta`
/// credential rows or the process env — never here).
#[derive(Debug, Clone)]
pub struct CredentialKeyNames {
    pub api_key_env: Option<String>,
    pub secret_env: Option<String>,
    pub passphrase_env: Option<String>,
    pub private_key_env: Option<String>,
}

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
                        tracing::warn!(error = %e, "account_risk row unreadable — skipped");
                        continue;
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
            Ok(out)
        })
    }

    /// Every `account_exit_policy` row, deserialized over `base` (the global
    /// exit config). A malformed JSON row is skipped with a warning; the
    /// caller's global/account fallback stands. Zero rows is `Ok` with an
    /// empty map ("this account has no DB row → use the env global") — but a
    /// *query* failure (the table is gone, the file unreadable) is `Err`: it
    /// is never silent-grown into "no rows", which the boot chain would read
    /// as "use the env global" and keep trading with a silently-defaulted
    /// exit discipline (issue #1199 fourth audit P0: the load_risk fix in
    /// #1214 applied to the exit twin too).
    pub fn load_exits(&self, base: &ExitConfig) -> Result<BTreeMap<String, ExitConfig>, String> {
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

    // ── account_settings / kernel_meta ──────────────────────────────────────
    // The account-scoped setting families that used to live in `.env` as
    // `ACCOUNT_<ID>_*` keys: the round-duration anchor, the strategy
    // enablement set, and the two Shadow-Evolution switches. Their durable
    // home is this database — the same file, the same connection the rest of
    // the account rows use, so "the account's data" is one write surface and
    // `.env` stops being a second truth for runtime settings.

    /// Every `account_settings` row, grouped by account then key. A query
    /// failure is an `Err` (never silently "no rows", which the boot would
    /// read as "fall back to the env default" and quietly change the grid).
    pub fn load_settings(&self) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
        self.db.with_connection(|conn| {
            let mut stmt = conn
                .prepare("SELECT account_id, key, value FROM account_settings")
                .map_err(|e| format!("account_settings prepare failed: {e}"))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| format!("account_settings query failed: {e}"))?;
            let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
            for row in rows {
                let (account, key, value) =
                    row.map_err(|e| format!("account_settings row failed: {e}"))?;
                out.entry(account).or_default().insert(key, value);
            }
            Ok(out)
        })
    }

    /// Upsert (`Some(value)`) or delete (`None`) one account-scoped setting.
    /// The account row is ensured first — same seam as [`Self::save_risk`]:
    /// the store may be opened before the account book is mirrored.
    pub fn save_setting(
        &self,
        account_id: &str,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), String> {
        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO accounts
                        (id, name, market_type, status, trading_enabled, updated_at_ms)
                     VALUES (?1, ?1, 'prediction', 'active', 1, ?2)",
                rusqlite::params![account_id, crate::db::now_ms()],
            )
            .map_err(|e| format!("account_settings account ensure failed: {e}"))?;
            match value {
                Some(v) => crate::db::account_setting_set(conn, account_id, key, v)
                    .map_err(|e| format!("account_settings write failed: {e}")),
                None => crate::db::account_setting_remove(conn, account_id, key)
                    .map_err(|e| format!("account_settings delete failed: {e}")),
            }
        })
    }

    /// One `kernel_meta` value (`None` = never written).
    pub fn load_kernel_meta(&self, key: &str) -> Result<Option<String>, String> {
        self.db.with_connection(|conn| {
            crate::db::kernel_meta_get(conn, key)
                .map_err(|e| format!("kernel_meta read ({key}) failed: {e}"))
        })
    }

    /// Upsert one `kernel_meta` value (process-scoped switches: the Shadow
    /// Evolution toggles are not per-account facts).
    pub fn save_kernel_meta(&self, key: &str, value: &str) -> Result<(), String> {
        self.db.with_connection(|conn| {
            crate::db::kernel_meta_set(conn, key, value)
                .map_err(|e| format!("kernel_meta write ({key}) failed: {e}"))
        })
    }

    /// Delete one `kernel_meta` row (the credential reset face: the row is
    /// gone, not blanked — absence is "no credential", never "empty value").
    pub fn delete_kernel_meta(&self, key: &str) -> Result<(), String> {
        self.db.with_connection(|conn| {
            conn.execute("DELETE FROM kernel_meta WHERE key = ?1", [key])
                .map(|_| ())
                .map_err(|e| format!("kernel_meta delete ({key}) failed: {e}"))
        })
    }

    /// Every `kernel_meta` row whose key starts with `prefix` (used by the
    /// credential store: `credential.<ENV_VAR_NAME>` → sealed blob). A query
    /// failure is an `Err` — the boot injection must never read a partial
    /// credential set as "none".
    pub fn load_kernel_meta_by_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, String> {
        self.db.with_connection(|conn| {
            let mut stmt = conn
                .prepare("SELECT key, value FROM kernel_meta WHERE key LIKE ?1 || '%'")
                .map_err(|e| format!("kernel_meta prefix prepare failed: {e}"))?;
            let rows = stmt
                .query_map([prefix], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| format!("kernel_meta prefix query failed: {e}"))?;
            let mut out = Vec::new();
            for row in rows {
                let (k, v) = row.map_err(|e| format!("kernel_meta prefix row failed: {e}"))?;
                out.push((k, v));
            }
            Ok(out)
        })
    }

    /// Full-row upsert — the boot's env-book materialization (issue #1199
    /// 第六轮, 操作者裁决 2026-11-14): when the `accounts` table is empty and
    /// the book came from the read-only `.env` fallback, the boot adopts the
    /// book into SQL so the durable home exists from that boot on. INSERT
    /// OR IGNORE is not enough here (a row must carry every field), but a
    /// concurrent/runtime write must never be clobbered either — hence the
    /// column-wise ON CONFLICT upsert, which only fires while the table is
    /// still empty in practice.
    pub fn save_account_row(&self, row: &AccountRow) -> Result<(), String> {
        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO accounts
                        (id, name, market_type, status, venue, trading_enabled,
                         rounds_json, opening_equity, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(id) DO UPDATE SET
                        name = excluded.name,
                        market_type = excluded.market_type,
                        status = excluded.status,
                        venue = excluded.venue,
                        trading_enabled = excluded.trading_enabled,
                        rounds_json = excluded.rounds_json,
                        opening_equity = excluded.opening_equity,
                        updated_at_ms = excluded.updated_at_ms",
                rusqlite::params![
                    row.id,
                    row.name,
                    row.market_type,
                    row.status,
                    row.venue,
                    row.trading_enabled,
                    row.rounds_json,
                    row.opening_equity,
                    row.updated_at_ms,
                ],
            )
            .map(|_| ())
            .map_err(|e| format!("account row write ({}) failed: {e}", row.id))
        })
    }

    /// The credential ENV VAR NAMES half of the materialization (names only,
    /// never values — the sealed values live in `kernel_meta.credential.*`).
    pub fn save_credential_names(
        &self,
        account_id: &str,
        keys: &crate::account::CredentialKeys,
    ) -> Result<(), String> {
        self.db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO account_credentials
                        (account_id, api_key_env, secret_env, passphrase_env,
                         private_key_env, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(account_id) DO UPDATE SET
                        api_key_env = excluded.api_key_env,
                        secret_env = excluded.secret_env,
                        passphrase_env = excluded.passphrase_env,
                        private_key_env = excluded.private_key_env,
                        updated_at_ms = excluded.updated_at_ms",
                rusqlite::params![
                    account_id,
                    keys.api_key_env,
                    keys.secret_env,
                    keys.passphrase_env,
                    keys.private_key_env,
                    crate::db::now_ms(),
                ],
            )
            .map(|_| ())
            .map_err(|e| format!("credential names write ({account_id}) failed: {e}"))
        })
    }

    /// The durable account book: every row of the `accounts` table, in id
    /// order. This is the SQL face of what the env `ACCOUNT_*` grammar
    /// spells — the boot overlays the book from here when the table holds
    /// rows (SQL is the authority; `.env` stays read-only fallback).
    pub fn load_accounts(&self) -> Result<Vec<AccountRow>, String> {
        self.db.with_connection(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, market_type, status, venue, trading_enabled,
                            rounds_json, opening_equity, updated_at_ms
                     FROM accounts ORDER BY id",
                )
                .map_err(|e| format!("accounts prepare failed: {e}"))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(AccountRow {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        market_type: r.get(2)?,
                        status: r.get(3)?,
                        venue: r.get(4)?,
                        trading_enabled: r.get::<_, i64>(5)? != 0,
                        rounds_json: r.get(6)?,
                        opening_equity: r.get(7)?,
                        updated_at_ms: r.get(8)?,
                    })
                })
                .map_err(|e| format!("accounts query failed: {e}"))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| format!("accounts row failed: {e}"))?);
            }
            Ok(out)
        })
    }

    /// One account row's credential ENV VAR NAMES (the `account_credentials`
    /// table — names only, never values).
    pub fn load_account_credential_keys(
        &self,
        account_id: &str,
    ) -> Result<Option<CredentialKeyNames>, String> {
        self.db.with_connection(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT api_key_env, secret_env, passphrase_env, private_key_env
                     FROM account_credentials WHERE account_id = ?1",
                )
                .map_err(|e| format!("account_credentials prepare failed: {e}"))?;
            let mut rows = stmt
                .query_map([account_id], |r| {
                    Ok(CredentialKeyNames {
                        api_key_env: r.get(0)?,
                        secret_env: r.get(1)?,
                        passphrase_env: r.get(2)?,
                        private_key_env: r.get(3)?,
                    })
                })
                .map_err(|e| format!("account_credentials query failed: {e}"))?;
            match rows.next() {
                Some(row) => row.map(Some).map_err(|e| format!("account_credentials row failed: {e}")),
                None => Ok(None),
            }
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
        let rows = db.load_exits(&ExitConfig::default()).unwrap();
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
        let rows = db.load_exits(&ExitConfig::default()).unwrap();
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
        let err = db.load_exits(&ExitConfig::default()).unwrap_err();
        assert!(
            err.contains("account_exit_policy"),
            "the error must name the failing query: {err}"
        );
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }
}
