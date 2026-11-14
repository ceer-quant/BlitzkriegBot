//! #1199 acceptance A — the backend-swap proof: one assembly line, three
//! backends, the SAME business path.
//!
//! The acceptance promise: "swap the backend = change ONE assembly line, the
//! business tests stay green" (issue #1199). This test is the engineering
//! evidence: ONE set of business-path operations, run against THREE StoreApi
//! implementations — `EnvStore` (the env backend: real `.env` files through
//! the kernel's own env-write faces the service layer calls), `MemoryStore`
//! (the in-memory backend) and `SqliteStore` (the real DB backend over a
//! tempdir database, PR-2) — with every observable outcome asserted
//! IDENTICAL fact-for-fact.
//!
//! SHAPE CONTRACT (the feat/1199-store-trait-env line): the real trait is
//! `blitzkrieg_core::store::StoreApi`, grouped by domain. The trait below is
//! the test-side comparison contract over the same domains, written to the
//! agreed grouping: account book, settings cell, risk/exit pair,
//! credentials. When the trait module grows read faces, this file's local
//! trait aliases/re-exports it and NOTHING else changes.
//!
//! The business paths are the ones the issue names as the env-write debt
//! (scripts/store-layer-baseline.txt carries the exact list): account
//! create / venue / rounds / status / trading / strategies / remove, the
//! runtime settings cell (round duration), and — since PR-2 — the risk/exit
//! pair (`save_risk_bounds` / `save_exit` + the load read-back) and the
//! credentials domain. For each path: write through the backend, read back
//! through the SAME backend's list/get face, and require the backends to
//! agree fact-for-fact — same rows, same values, same removals.
//!
//! LEGAL DIFFERENCES (enumerated, never silenced — issue #1199 r2 P0-2):
//!   * `market.plugin.set` is NOT a StoreApi domain (the runtime-config cell
//!     belongs to the launcher): EnvStore/MemoryStore carry it, SqliteStore
//!     has no face and refuses with a typed error — asserted per backend.
//!   * `market.credentials.set`: EnvStore writes `.env`, MemoryStore records,
//!     SqliteStore refuses with a typed error (the per-account var-name
//!     mapping is PR-3) — asserted per backend.
//!   * The SqliteStore materializes an `accounts` row on any account-book
//!     write (the FK precondition), so a GHOST write leaves a row in the DB
//!     the env backend's dead key does not — but neither materializes in the
//!     BOOK (the list face resolves from `ACCOUNT_LIST` / the row set the
//!     writer named), which is the invariant the issue's "no implicit
//!     ledger" discipline is about.
//!
//! Toolchain note: this is an integration test (tests/), so the library is
//! compiled WITHOUT cfg(test) — the env backend drives the REAL public
//! write faces (`env_file` + the account config env family + the production
//! `crate::store::EnvStore`), which is exactly the seam the gate
//! (scripts/store-layer-check.mjs) watches.

// TRAIT_SOURCE: the agreed shape. When the store module grows read faces,
// this becomes `use blitzkrieg_core::store::StoreApi;` and the local trait
// block is deleted — the test bodies do not change.
#![allow(dead_code)]

use blitzkrieg_core::account::config::{
    env_persist_account_create, env_persist_account_remove, env_persist_account_rounds,
    env_persist_account_status, env_persist_account_strategies, env_persist_account_trading,
    env_persist_account_venue, load_account_ledgers_single_source,
};
use blitzkrieg_core::account::{AccountStatus, CredentialKeys};
use blitzkrieg_core::exit_policy::ExitConfig;
use blitzkrieg_core::model::default_account_id;
use blitzkrieg_core::risk::limits::{AccountRiskLimits, Bound, LimitSource};
use blitzkrieg_core::store::StoreApi as _;
use rust_decimal_macros::dec;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ── the trait: StoreApi, grouped by domain (the agreed #1199 shape) ─────────

/// The store contract. Domain-grouped: each group owns its keys'
/// get/set/remove/list faces. An implementation is interchangeable with any
/// other — that is the whole acceptance.
pub trait StoreApi {
    /// Account book domain.
    fn accounts(&self) -> &dyn AccountStore;
    /// Runtime settings domain (the runtime config cell).
    fn settings(&self) -> &dyn SettingsStore;
    /// The risk/exit pair (`save_risk_bounds` / `save_exit` + load read-back).
    fn risk_exit(&self) -> &dyn RiskExitStore;
    /// The credentials domain.
    fn credentials(&self) -> &dyn CredentialsStore;
}

/// The account book: the ACCOUNT_* grammar's typed face.
pub trait AccountStore {
    /// Create (or refuse: `Err` when the id is already named).
    fn create(&self, id: &str, name: Option<&str>) -> Result<(), String>;
    /// Upsert the venue binding (`""` = unbind, the grammar spells unbound
    /// by absence).
    fn set_venue(&self, id: &str, venue: &str) -> Result<(), String>;
    /// Upsert the round-length family.
    fn set_rounds(&self, id: &str, rounds: &[i64]) -> Result<(), String>;
    /// Upsert the status word.
    fn set_status(&self, id: &str, status: &str) -> Result<(), String>;
    /// Upsert the trading switch.
    fn set_trading_enabled(&self, id: &str, on: bool) -> Result<(), String>;
    /// Upsert the per-account strategy enablement set.
    fn set_strategies(&self, id: &str, strategies: &[&str]) -> Result<(), String>;
    /// Remove the account and every key it owns.
    fn remove(&self, id: &str) -> Result<(), String>;
    /// The book as the BOOT path reads it (the list face).
    fn list(&self) -> Result<Vec<AccountRow>, String>;
}

/// One row of the book as the boot loader resolves it — the comparison
/// shape between backends (NOT a serde wire type: it is what the two
/// backends must agree on).
#[derive(Debug, Clone, PartialEq)]
pub struct AccountRow {
    pub id: String,
    pub name: String,
    pub venue: String,
    pub status: String,
    pub trading_enabled: bool,
    pub rounds: Vec<i64>,
    pub strategies: Vec<String>,
}

/// The one status spelling both backends report (the grammar's word, not the
/// enum's Debug shape — `Suspended { reason }` is not a word).
fn status_word(status: &AccountStatus) -> String {
    match status {
        AccountStatus::Active => "active".to_string(),
        AccountStatus::ReadOnly => "readonly".to_string(),
        AccountStatus::Frozen => "frozen".to_string(),
        AccountStatus::Suspended { .. } => "suspended".to_string(),
    }
}

/// The runtime settings cell: the round-duration / market-plugin pair.
pub trait SettingsStore {
    fn set_round_duration(&self, sec: Option<i64>) -> Result<(), String>;
    fn set_market_plugin(&self, plugin: Option<&str>) -> Result<(), String>;
    fn get_round_duration(&self) -> Option<i64>;
    fn get_market_plugin(&self) -> Option<String>;
}

/// The risk/exit pair's store face: the six systemic bounds / the five exit
/// cockpit fields, one account at a time, with the load read-back. The
/// read-back is the account's row as a sorted field → value list — the
/// carriers spell their rows in different orders (the file's insertion
/// order, the DB's column order, the map's key order), and the comparison
/// is the FACT (the same field → value map), not the carrier's spelling.
pub trait RiskExitStore {
    /// Upsert the named systemic fields for one account.
    fn set_risk(&self, account: &str, fields: &[(&str, &str)]) -> Result<(), String>;
    /// The account's systemic row, read back (field → value, sorted).
    fn get_risk(&self, account: &str) -> Vec<(String, String)>;
    /// Upsert the named exit-discipline fields for one account.
    fn set_exit(&self, account: &str, fields: &[(&str, &str)]) -> Result<(), String>;
    /// The account's exit row, read back (field → value, sorted).
    fn get_exit(&self, account: &str) -> Vec<(String, String)>;
}

/// Sort a read-back row into the comparison's canonical order (field name).
fn sorted_row(mut row: Vec<(String, String)>) -> Vec<(String, String)> {
    row.sort_by(|a, b| a.0.cmp(&b.0));
    row
}

/// Filter a read-back row to the fields the business path WROTE for the
/// account: the carriers' unwritten-field resolution is their own fallback
/// semantics (the env grammar inherits the globals; the DB row is
/// standalone), so the fact-for-fact comparison covers the written fields
/// (and the merge — the earlier values survive).
fn filter_row(row: Vec<(String, String)>, written: &[&str]) -> Vec<(String, String)> {
    sorted_row(
        row.into_iter()
            .filter(|(field, _)| written.contains(&field.as_str()))
            .collect(),
    )
}

/// The credentials domain's store face.
pub trait CredentialsStore {
    /// Upsert (`reset = false`) or remove (`reset = true`) the declared
    /// credential variable names.
    fn save_credentials(&self, vars: &[(&str, &str)], reset: bool) -> Result<(), String>;
}

// ── EnvStore: the env backend over the REAL kernel write faces ──────────────

/// The env backend: a scratch directory holding the `.env` (the kernel's
/// single config source) plus the runtime settings cell. Every write goes
/// through the kernel's own public env-write faces — the same functions the
/// service layer calls — and every read resolves through the boot loader.
/// The risk/exit and credentials domains delegate to the PRODUCTION
/// `blitzkrieg_core::store::EnvStore` (the PR-1 implementation), so the
/// parity exercises the shipped env backend, not a test double.
pub struct EnvStore {
    env_path: PathBuf,
    legacy_toml: PathBuf,
    state_path: PathBuf,
    inner: blitzkrieg_core::store::EnvStore,
}

impl EnvStore {
    /// A fresh scratch deployment. The legacy toml path is INSIDE the
    /// scratch (and never created): the write faces refuse while a legacy
    /// toml exists, and the parity fixture must not trip that refusal.
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bk-store-parity-env-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let env_path = dir.join(".env");
        let legacy_toml = dir.join("accounts.toml");
        let inner = blitzkrieg_core::store::EnvStore::new(
            Some((env_path.clone(), legacy_toml.clone())),
            Some(env_path.clone()),
            None,
        );
        Self {
            env_path,
            legacy_toml,
            state_path: dir.join("runtime-config.json"),
            inner,
        }
    }

    pub fn env_path(&self) -> &Path {
        &self.env_path
    }
}

impl Drop for EnvStore {
    fn drop(&mut self) {
        if let Some(dir) = self.env_path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl AccountStore for EnvStore {
    fn create(&self, id: &str, name: Option<&str>) -> Result<(), String> {
        env_persist_account_create(
            &self.legacy_toml,
            &self.env_path,
            id,
            name,
            None,
            &CredentialKeys::default(),
        )
        .map_err(|e| e.to_string())
    }
    fn set_venue(&self, id: &str, venue: &str) -> Result<(), String> {
        env_persist_account_venue(&self.legacy_toml, &self.env_path, id, venue)
            .map_err(|e| e.to_string())
    }
    fn set_rounds(&self, id: &str, rounds: &[i64]) -> Result<(), String> {
        env_persist_account_rounds(&self.legacy_toml, &self.env_path, id, rounds)
            .map_err(|e| e.to_string())
    }
    fn set_status(&self, id: &str, status: &str) -> Result<(), String> {
        let parsed = match status {
            "active" => AccountStatus::Active,
            "readonly" => AccountStatus::ReadOnly,
            "frozen" => AccountStatus::Frozen,
            other => return Err(format!("unknown status `{other}`")),
        };
        env_persist_account_status(&self.legacy_toml, &self.env_path, id, &parsed)
            .map_err(|e| e.to_string())
    }
    fn set_trading_enabled(&self, id: &str, on: bool) -> Result<(), String> {
        env_persist_account_trading(&self.legacy_toml, &self.env_path, id, on)
            .map_err(|e| e.to_string())
    }
    fn set_strategies(&self, id: &str, strategies: &[&str]) -> Result<(), String> {
        let owned: Vec<String> = strategies.iter().map(|s| s.to_string()).collect();
        env_persist_account_strategies(&self.legacy_toml, &self.env_path, id, &owned)
            .map_err(|e| e.to_string())
    }
    fn remove(&self, id: &str) -> Result<(), String> {
        env_persist_account_remove(&self.legacy_toml, &self.env_path, id).map_err(|e| e.to_string())
    }
    fn list(&self) -> Result<Vec<AccountRow>, String> {
        let book = load_account_ledgers_single_source(
            &self.legacy_toml,
            Some(&self.env_path),
            0,
            // The boot resolution validates the declared rounds family
            // against the plugins' union — the parity fixture uses exactly
            // the durations the union declares (300/900/3600, the scanner's
            // own labels), so the env backend resolves the same book the
            // memory backend holds.
            &[300, 900, 3600],
        )
        .map_err(|e| e.to_string())?;
        Ok(book
            .account_list()
            .iter()
            .map(|a| AccountRow {
                id: a.id.as_str().to_string(),
                name: a.name.clone(),
                venue: a.venue.clone(),
                status: status_word(&a.status),
                trading_enabled: a.trading_enabled,
                rounds: a.rounds.clone(),
                strategies: a.strategies.clone(),
            })
            .collect())
    }
}

impl SettingsStore for EnvStore {
    fn set_round_duration(&self, sec: Option<i64>) -> Result<(), String> {
        blitzkrieg_core::runtime_config::persist_round_duration_to_with_env(
            &self.state_path,
            sec,
            "uid:parity",
            0,
            Some(&self.env_path),
        )
        .map_err(|e| e.to_string())
    }
    fn set_market_plugin(&self, plugin: Option<&str>) -> Result<(), String> {
        blitzkrieg_core::runtime_config::persist_market_plugin_to_with_env(
            &self.state_path,
            plugin,
            "uid:parity",
            0,
            Some(&self.env_path),
        )
        .map_err(|e| e.to_string())
    }
    fn get_round_duration(&self) -> Option<i64> {
        blitzkrieg_core::runtime_config::load_round_duration_from(
            &self.state_path,
            &[300, 900, 3600],
        )
        .map(|c| c.duration_sec)
    }
    fn get_market_plugin(&self) -> Option<String> {
        blitzkrieg_core::runtime_config::load_market_plugin_from(&self.state_path).map(|c| c.plugin)
    }
}

impl RiskExitStore for EnvStore {
    fn set_risk(&self, account: &str, fields: &[(&str, &str)]) -> Result<(), String> {
        let owned: Vec<(String, String)> = fields
            .iter()
            .map(|(f, v)| (f.to_string(), v.to_string()))
            .collect();
        self.inner
            .save_risk_bounds(Some(account), &owned)
            .map(|_| ())
            .map_err(|e| e.message)
    }
    fn get_risk(&self, account: &str) -> Vec<(String, String)> {
        let vars = blitzkrieg_core::env_file::read_env_vars(&self.env_path).unwrap_or_default();
        // The read face mirrors the write face's key grammar exactly: the
        // default account's row is the GLOBAL (unsuffixed) keys, a named
        // account's row is its suffixed keys — no cross-account fallback
        // (the DB read face is per-account, and the parity compares the
        // per-account row).
        let is_default = account == default_account_id().as_str();
        let mut out = Vec::new();
        for key in blitzkrieg_core::cockpit::COCKPIT_KEYS
            .iter()
            .filter(|k| k.family == blitzkrieg_core::cockpit::CockpitFamily::Systemic)
        {
            let candidate = if is_default {
                key.env_key.to_string()
            } else {
                blitzkrieg_core::config::account_env_key(key.env_key, account)
            };
            if let Some(raw) = vars.get(&candidate) {
                out.push((key.field.to_string(), raw.trim().to_string()));
            }
        }
        sorted_row(out)
    }
    fn set_exit(&self, account: &str, fields: &[(&str, &str)]) -> Result<(), String> {
        let owned: Vec<(String, String)> = fields
            .iter()
            .map(|(f, v)| (f.to_string(), v.to_string()))
            .collect();
        self.inner
            .save_exit(Some(account), &owned)
            .map(|_| ())
            .map_err(|e| e.message)
    }
    fn get_exit(&self, account: &str) -> Vec<(String, String)> {
        let vars = blitzkrieg_core::env_file::read_env_vars(&self.env_path).unwrap_or_default();
        // The read face mirrors the write face's key grammar exactly (see
        // `get_risk`): the default account's row is the GLOBAL keys, a
        // named account's row is its suffixed keys.
        let is_default = account == default_account_id().as_str();
        let mut out = Vec::new();
        for key in blitzkrieg_core::cockpit::COCKPIT_KEYS
            .iter()
            .filter(|k| k.family == blitzkrieg_core::cockpit::CockpitFamily::Exit)
        {
            let candidate = if is_default {
                key.env_key.to_string()
            } else {
                blitzkrieg_core::config::account_env_key(key.env_key, account)
            };
            if let Some(raw) = vars.get(&candidate) {
                out.push((key.field.to_string(), raw.trim().to_string()));
            }
        }
        sorted_row(out)
    }
}

impl CredentialsStore for EnvStore {
    fn save_credentials(&self, vars: &[(&str, &str)], reset: bool) -> Result<(), String> {
        let owned: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let reset_keys: Vec<&str> = if reset {
            vars.iter().map(|(k, _)| *k).collect()
        } else {
            Vec::new()
        };
        self.inner
            .save_credentials(&owned, &reset_keys)
            .map_err(|e| e.message)
    }
}

impl StoreApi for EnvStore {
    fn accounts(&self) -> &dyn AccountStore {
        self
    }
    fn settings(&self) -> &dyn SettingsStore {
        self
    }
    fn risk_exit(&self) -> &dyn RiskExitStore {
        self
    }
    fn credentials(&self) -> &dyn CredentialsStore {
        self
    }
}

// ── MemoryStore: the in-memory backend ──────────────────────────────────────

/// The in-memory backend: the same domain contract over plain maps. No
/// disk, no env, no .env — this is the backend the acceptance's "swap the
/// backend = change one assembly line" promises is interchangeable.
#[derive(Default)]
pub struct MemoryStore {
    accounts: std::sync::Mutex<BTreeMap<String, MemAccount>>,
    round_duration: std::sync::Mutex<Option<i64>>,
    market_plugin: std::sync::Mutex<Option<String>>,
    risk: std::sync::Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    exit: std::sync::Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    credentials: std::sync::Mutex<Vec<(String, String)>>,
}

#[derive(Clone, Default)]
struct MemAccount {
    name: Option<String>,
    venue: String,
    status: Option<String>,
    trading_enabled: Option<bool>,
    rounds: Option<Vec<i64>>,
    strategies: Option<Vec<String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AccountStore for MemoryStore {
    fn create(&self, id: &str, name: Option<&str>) -> Result<(), String> {
        let mut book = self.accounts.lock().unwrap();
        if book.contains_key(id) {
            return Err(format!("ACCOUNT_LIST already names `{id}`"));
        }
        book.insert(
            id.to_string(),
            MemAccount {
                name: name.map(str::to_string),
                ..Default::default()
            },
        );
        Ok(())
    }
    fn set_venue(&self, id: &str, venue: &str) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.venue = venue.to_string();
        })
    }
    fn set_rounds(&self, id: &str, rounds: &[i64]) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.rounds = Some(rounds.to_vec());
        })
    }
    fn set_status(&self, id: &str, status: &str) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.status = Some(status.to_string());
        })
    }
    fn set_trading_enabled(&self, id: &str, on: bool) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.trading_enabled = Some(on);
        })
    }
    fn set_strategies(&self, id: &str, strategies: &[&str]) -> Result<(), String> {
        self.mut_account(id, |a| {
            a.strategies = Some(strategies.iter().map(|s| s.to_string()).collect());
        })
    }
    fn remove(&self, id: &str) -> Result<(), String> {
        let mut book = self.accounts.lock().unwrap();
        if book.remove(id).is_none() {
            return Err(format!("account `{id}` is not configured"));
        }
        Ok(())
    }
    fn list(&self) -> Result<Vec<AccountRow>, String> {
        let book = self.accounts.lock().unwrap();
        Ok(book
            .iter()
            .map(|(id, a)| AccountRow {
                id: id.clone(),
                name: a.name.clone().unwrap_or_else(|| id.clone()),
                venue: a.venue.clone(),
                status: a
                    .status
                    .clone()
                    .unwrap_or_else(|| status_word(&AccountStatus::Active)),
                trading_enabled: a.trading_enabled.unwrap_or(true),
                rounds: a.rounds.clone().unwrap_or_default(),
                strategies: a.strategies.clone().unwrap_or_default(),
            })
            .collect())
    }
}

impl MemoryStore {
    fn mut_account(&self, id: &str, f: impl FnOnce(&mut MemAccount)) -> Result<(), String> {
        let mut book = self.accounts.lock().unwrap();
        let a = book
            .get_mut(id)
            .ok_or_else(|| format!("account `{id}` is not configured"))?;
        f(a);
        Ok(())
    }
}

impl SettingsStore for MemoryStore {
    fn set_round_duration(&self, sec: Option<i64>) -> Result<(), String> {
        *self.round_duration.lock().unwrap() = sec;
        Ok(())
    }
    fn set_market_plugin(&self, plugin: Option<&str>) -> Result<(), String> {
        *self.market_plugin.lock().unwrap() = plugin.map(str::to_string);
        Ok(())
    }
    fn get_round_duration(&self) -> Option<i64> {
        *self.round_duration.lock().unwrap()
    }
    fn get_market_plugin(&self) -> Option<String> {
        self.market_plugin.lock().unwrap().clone()
    }
}

impl RiskExitStore for MemoryStore {
    fn set_risk(&self, account: &str, fields: &[(&str, &str)]) -> Result<(), String> {
        let mut risk = self.risk.lock().unwrap();
        let row = risk.entry(account.to_string()).or_default();
        for (field, value) in fields {
            row.insert(field.to_string(), value.to_string());
        }
        Ok(())
    }
    fn get_risk(&self, account: &str) -> Vec<(String, String)> {
        sorted_row(
            self.risk
                .lock()
                .unwrap()
                .get(account)
                .map(|row| row.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
        )
    }
    fn set_exit(&self, account: &str, fields: &[(&str, &str)]) -> Result<(), String> {
        let mut exit = self.exit.lock().unwrap();
        let row = exit.entry(account.to_string()).or_default();
        for (field, value) in fields {
            row.insert(field.to_string(), value.to_string());
        }
        Ok(())
    }
    fn get_exit(&self, account: &str) -> Vec<(String, String)> {
        sorted_row(
            self.exit
                .lock()
                .unwrap()
                .get(account)
                .map(|row| row.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
        )
    }
}

impl CredentialsStore for MemoryStore {
    fn save_credentials(&self, vars: &[(&str, &str)], reset: bool) -> Result<(), String> {
        let mut creds = self.credentials.lock().unwrap();
        for (key, value) in vars {
            if reset {
                creds.retain(|(k, _)| k != key);
            } else {
                creds.push((key.to_string(), value.to_string()));
            }
        }
        Ok(())
    }
}

impl StoreApi for MemoryStore {
    fn accounts(&self) -> &dyn AccountStore {
        self
    }
    fn settings(&self) -> &dyn SettingsStore {
        self
    }
    fn risk_exit(&self) -> &dyn RiskExitStore {
        self
    }
    fn credentials(&self) -> &dyn CredentialsStore {
        self
    }
}

// ── SqliteStore: the real DB backend over a tempdir database ────────────────

/// The SQLite backend: the PRODUCTION `blitzkrieg_core::store::SqliteStore`
/// (PR-2) over a private tempdir database, plus a read-back connection the
/// test opens on the same file (the store's write side has no read faces;
/// the boot overlay's read is the AccountRiskDb's `load_risk` / `load_exits`
/// and the account/settings rows come from the test's own connection — the
/// same database, the same committed facts).
pub struct SqliteStore {
    store: blitzkrieg_core::store::SqliteStore,
    dir: PathBuf,
    readback: blitzkrieg_core::db::Database,
}

impl SqliteStore {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bk-store-parity-sqlite-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let store = blitzkrieg_core::store::SqliteStore::open(dir.join("store.sqlite"))
            .expect("temp store opens");
        let readback =
            blitzkrieg_core::db::Database::open(dir.join("store.sqlite")).expect("readback opens");
        Self {
            store,
            dir,
            readback,
        }
    }
}

impl Drop for SqliteStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl AccountStore for SqliteStore {
    fn create(&self, id: &str, name: Option<&str>) -> Result<(), String> {
        self.store
            .save_account_create(id, name, None, &CredentialKeys::default())
            .map_err(|e| e.message)
    }
    fn set_venue(&self, id: &str, venue: &str) -> Result<(), String> {
        self.store
            .save_account_venue(id, venue)
            .map_err(|e| e.message)
    }
    fn set_rounds(&self, id: &str, rounds: &[i64]) -> Result<(), String> {
        self.store
            .save_account_rounds(id, rounds)
            .map_err(|e| e.message)
    }
    fn set_status(&self, id: &str, status: &str) -> Result<(), String> {
        let parsed = match status {
            "active" => AccountStatus::Active,
            "readonly" => AccountStatus::ReadOnly,
            "frozen" => AccountStatus::Frozen,
            other => return Err(format!("unknown status `{other}`")),
        };
        self.store
            .save_account_status(id, &parsed)
            .map_err(|e| e.message)
    }
    fn set_trading_enabled(&self, id: &str, on: bool) -> Result<(), String> {
        self.store
            .save_account_trading(id, on)
            .map_err(|e| e.message)
    }
    fn set_strategies(&self, id: &str, strategies: &[&str]) -> Result<(), String> {
        let owned: Vec<String> = strategies.iter().map(|s| s.to_string()).collect();
        self.store
            .save_account_strategies(id, &owned)
            .map_err(|e| e.message)
    }
    fn remove(&self, id: &str) -> Result<(), String> {
        self.store.remove_account(id).map_err(|e| e.message)
    }
    fn list(&self) -> Result<Vec<AccountRow>, String> {
        let rows: Vec<(String, String, String, String, i64, String)> = self
            .readback
            .with_connection(|conn| -> anyhow::Result<Vec<_>> {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, name, venue, status, trading_enabled, rounds_json
                         FROM accounts",
                    )
                    .expect("prepare");
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, String>(5)?,
                        ))
                    })
                    .expect("query");
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (id, name, venue, status, trading, rounds_json) in rows {
            let rounds: Vec<i64> = serde_json::from_str(&rounds_json).unwrap_or_default();
            let strategies: Vec<String> = self
                .readback
                .with_connection(|conn| -> anyhow::Result<Vec<String>> {
                    let mut stmt = conn
                        .prepare(
                            "SELECT strategy FROM strategy_assignments
                             WHERE account_id = ?1",
                        )
                        .expect("prepare");
                    let rows = stmt
                        .query_map([&id], |r| r.get::<_, String>(0))
                        .expect("query");
                    Ok(rows.collect::<Result<Vec<_>, _>>()?)
                })
                .map_err(|e| e.to_string())?;
            out.push(AccountRow {
                id,
                name,
                venue,
                status,
                trading_enabled: trading != 0,
                rounds,
                strategies,
            });
        }
        Ok(out)
    }
}

impl SettingsStore for SqliteStore {
    fn set_round_duration(&self, sec: Option<i64>) -> Result<(), String> {
        self.store
            .save_account_round_duration("default", sec)
            .map_err(|e| e.message)
    }
    fn set_market_plugin(&self, _plugin: Option<&str>) -> Result<(), String> {
        // LEGAL DIFFERENCE (enumerated): `market.plugin.set` is NOT a
        // StoreApi domain — the runtime-config cell belongs to the launcher.
        // The SqliteStore has no face for it; the refusal is the honest
        // answer (asserted per backend in the business path).
        Err("no store face: market.plugin is the launcher's runtime-config cell, not a StoreApi domain".to_string())
    }
    fn get_round_duration(&self) -> Option<i64> {
        self.readback
            .with_connection(|conn| {
                blitzkrieg_core::db::account_setting_get(conn, "default", "round.duration_sec")
                    .ok()
                    .flatten()
            })
            .and_then(|raw| raw.trim().parse::<i64>().ok())
    }
    fn get_market_plugin(&self) -> Option<String> {
        None
    }
}

impl RiskExitStore for SqliteStore {
    fn set_risk(&self, account: &str, fields: &[(&str, &str)]) -> Result<(), String> {
        let owned: Vec<(String, String)> = fields
            .iter()
            .map(|(f, v)| (f.to_string(), v.to_string()))
            .collect();
        self.store
            .save_risk_bounds(Some(account), &owned)
            .map(|_| ())
            .map_err(|e| e.message)
    }
    fn get_risk(&self, account: &str) -> Vec<(String, String)> {
        let rows = self.store.risk_db().load_risk();
        let Some(row) = rows.get(account) else {
            return Vec::new();
        };
        sorted_row(vec![
            (
                "maxSingleLossUsd".to_string(),
                row.max_single_loss_usd.value().to_string(),
            ),
            (
                "maxDailyDrawdownUsd".to_string(),
                row.max_daily_drawdown_usd.value().to_string(),
            ),
            (
                "maxPositionSize".to_string(),
                row.max_position_size.value().to_string(),
            ),
            (
                "maxConsecutiveLosses".to_string(),
                row.max_consecutive_losses.value().to_string(),
            ),
            (
                "cooldownMinutes".to_string(),
                row.cooldown_minutes.value().to_string(),
            ),
            (
                "maxOrderNotionalUsd".to_string(),
                row.max_order_notional_usd.value().to_string(),
            ),
        ])
    }
    fn set_exit(&self, account: &str, fields: &[(&str, &str)]) -> Result<(), String> {
        let owned: Vec<(String, String)> = fields
            .iter()
            .map(|(f, v)| (f.to_string(), v.to_string()))
            .collect();
        self.store
            .save_exit(Some(account), &owned)
            .map(|_| ())
            .map_err(|e| e.message)
    }
    fn get_exit(&self, account: &str) -> Vec<(String, String)> {
        let rows = self.store.risk_db().load_exits(&ExitConfig::default());
        let Some(exit) = rows.get(account) else {
            return Vec::new();
        };
        sorted_row(vec![
            ("stopLossPct".to_string(), exit.stop_loss_pct.to_string()),
            (
                "takeProfitPct".to_string(),
                exit.take_profit_pct.to_string(),
            ),
            ("forceExitSec".to_string(), exit.force_exit_sec.to_string()),
            (
                "trailingEnabled".to_string(),
                exit.trailing_enabled.to_string(),
            ),
            (
                "strategyExitMakerFirst".to_string(),
                exit.strategy_exit_maker_first.to_string(),
            ),
        ])
    }
}

impl CredentialsStore for SqliteStore {
    fn save_credentials(&self, _vars: &[(&str, &str)], _reset: bool) -> Result<(), String> {
        // LEGAL DIFFERENCE (enumerated): the per-account var-name→account
        // mapping is a PR-3 design decision; the refusal is the honest
        // face (asserted per backend in the business path).
        Err("the sqlite store does not carry credentials yet (issue #1199 PR-3)".to_string())
    }
}

impl StoreApi for SqliteStore {
    fn accounts(&self) -> &dyn AccountStore {
        self
    }
    fn settings(&self) -> &dyn SettingsStore {
        self
    }
    fn risk_exit(&self) -> &dyn RiskExitStore {
        self
    }
    fn credentials(&self) -> &dyn CredentialsStore {
        self
    }
}

// ── the business path: ONE set of operations, run on ALL THREE backends ─────

/// Run the full business path against one backend and return every
/// observable outcome. The three backends' reports must be equal.
///
/// Report discipline: writes report their OUTCOME (Ok, or Err with the
/// refusal's KIND — the env backend's refusals carry an error-code prefix
/// the memory backend's plain strings do not, and that text is an
/// implementation freedom, not a behavior). The LIST face is the full
/// fact-for-fact comparison: every row, every field. Refusals additionally
/// report whether the account was NAMED in the message (both backends
/// refuse by name — the no-implicit-ledger discipline).
///
/// The ENUMERATED store-level differences are kept OUT of this shared
/// report (asserted per backend in
/// `the_sqlite_backend_enumerates_its_legal_differences`): the duplicate
/// create (the env store refuses its own ACCOUNT_LIST duplicate; the
/// SqliteStore's create is INSERT OR IGNORE — idempotent, the service
/// layer's duplicate refusal is the production gate for both), the
/// market-plugin cell (not a StoreApi domain), and the credentials domain
/// (the SqliteStore's typed PR-3 refusal). The shared report is the
/// fact-for-fact body: the account book, the settings cell, the risk/exit
/// round-trip.
fn run_business_path(store: &dyn StoreApi) -> Vec<String> {
    let mut out = Vec::new();
    let accounts = store.accounts();

    // 1. create two accounts (one with a name). (The duplicate-create
    // refusal is an enumerated store-level difference — see the doc.)
    out.push(format!(
        "create a: {}",
        outcome(accounts.create("alpha", Some("Alpha")))
    ));
    out.push(format!(
        "create b: {}",
        outcome(accounts.create("beta", None))
    ));

    // 2. venue bindings (one bound, one explicitly unbound by "").
    out.push(format!(
        "venue alpha->polymarket: {}",
        outcome(accounts.set_venue("alpha", "polymarket"))
    ));
    out.push(format!(
        "venue beta->empty: {}",
        outcome(accounts.set_venue("beta", ""))
    ));

    // 3. rounds families.
    out.push(format!(
        "rounds alpha [900,3600]: {}",
        outcome(accounts.set_rounds("alpha", &[900, 3600]))
    ));
    out.push(format!(
        "rounds beta [300]: {}",
        outcome(accounts.set_rounds("beta", &[300]))
    ));

    // 4. status words.
    out.push(format!(
        "status alpha frozen: {}",
        outcome(accounts.set_status("alpha", "frozen"))
    ));

    // 5. the trading switch.
    out.push(format!(
        "trading beta off: {}",
        outcome(accounts.set_trading_enabled("beta", false))
    ));

    // 6. per-account strategy enablement.
    out.push(format!(
        "strategies alpha [s1,s2]: {}",
        outcome(accounts.set_strategies("alpha", &["s1", "s2"]))
    ));
    out.push(format!(
        "strategies beta []: {}",
        outcome(accounts.set_strategies("beta", &[]))
    ));

    // 7. the list face after every write — the boot resolution.
    out.push(format!("list: {:?}", accounts.list()));

    // 8. remove beta, then the list again (removal must take every key).
    out.push(format!("remove beta: {}", outcome(accounts.remove("beta"))));
    out.push(format!(
        "remove beta again: {}",
        outcome(accounts.remove("beta"))
    ));
    out.push(format!("list after remove: {:?}", accounts.list()));

    // 9. the settings domain: write, read back, clear, read again. (The
    // market-plugin cell is an enumerated store-level difference — not a
    // StoreApi domain — asserted per backend in the legal-differences test.)
    let settings = store.settings();
    out.push(format!(
        "set round 300: {}",
        outcome(settings.set_round_duration(Some(300)))
    ));
    out.push(format!("get round: {:?}", settings.get_round_duration()));
    out.push(format!(
        "clear round: {}",
        outcome(settings.set_round_duration(None))
    ));
    out.push(format!(
        "get round after clear: {:?}",
        settings.get_round_duration()
    ));

    // 10. the risk/exit pair: a partial set, a merging second set, the
    // read-back (filtered to the fields this path WROTE for the account —
    // the carriers' unwritten-field resolution is their own fallback
    // semantics, the env grammar's "inherit the globals" vs the DB row's
    // standalone shape, enumerated in the legal-differences test), and the
    // default account's own row.
    let risk_exit = store.risk_exit();
    out.push(format!(
        "risk alpha maxPositionSize 5: {}",
        outcome(risk_exit.set_risk("alpha", &[("maxPositionSize", "5")]))
    ));
    out.push(format!(
        "risk alpha cooldownMinutes 30: {}",
        outcome(risk_exit.set_risk("alpha", &[("cooldownMinutes", "30")]))
    ));
    out.push(format!(
        "get risk alpha: {:?}",
        filter_row(
            risk_exit.get_risk("alpha"),
            &["maxPositionSize", "cooldownMinutes"]
        )
    ));
    out.push(format!(
        "exit alpha stopLossPct 8: {}",
        outcome(risk_exit.set_exit("alpha", &[("stopLossPct", "8")]))
    ));
    out.push(format!(
        "exit alpha takeProfitPct 90 + trailing: {}",
        outcome(risk_exit.set_exit(
            "alpha",
            &[("takeProfitPct", "90"), ("trailingEnabled", "true")]
        ))
    ));
    out.push(format!(
        "get exit alpha: {:?}",
        filter_row(
            risk_exit.get_exit("alpha"),
            &["stopLossPct", "takeProfitPct", "trailingEnabled"]
        )
    ));
    // The default account's own row (absent account = the process default).
    let default_id = default_account_id();
    out.push(format!(
        "risk default maxSingleLossUsd 25: {}",
        outcome(risk_exit.set_risk(default_id.as_str(), &[("maxSingleLossUsd", "25")]))
    ));
    out.push(format!(
        "get risk default: {:?}",
        filter_row(
            risk_exit.get_risk(default_id.as_str()),
            &["maxSingleLossUsd"]
        )
    ));
    // The sibling isolation: beta's row is untouched by alpha's writes.
    out.push(format!("get risk beta: {:?}", risk_exit.get_risk("beta")));
    out.push(format!("get exit beta: {:?}", risk_exit.get_exit("beta")));

    out
}

/// The normalized outcome of one write: `Ok`, or `Err(kind)` where kind is
/// the refusal's SHAPE (does it name the account?), not its text.
fn outcome(r: Result<(), String>) -> String {
    match r {
        Ok(()) => "Ok".to_string(),
        Err(e) => {
            // the account id this operation addressed, for the naming probe
            let _ = &e;
            format!("Err({})", refusal_kind(&e))
        }
    }
}

/// Classify a refusal's text into its behavioral kind. Both backends refuse
/// a duplicate create and an unknown account BY NAME; the env backend's
/// refusals carry the CoreError code prefix (`InvalidParams: …`) — strip it
/// and compare the shape.
fn refusal_kind(e: &str) -> String {
    let stripped = e
        .strip_prefix("InvalidParams: ")
        .or_else(|| e.strip_prefix("InvalidParams:"))
        .unwrap_or(e);
    // does the refusal name an account (backticks around an id)?
    if stripped.contains('`') {
        "names-the-account".to_string()
    } else {
        "un-named".to_string()
    }
}

/// THE acceptance: the same business path produces the same outcomes on the
/// env backend, the in-memory backend and the SQLite backend.
#[test]
fn all_three_backends_agree_on_the_business_path() {
    let env = EnvStore::new("main");
    let mem = MemoryStore::new();
    let sqlite = SqliteStore::new("main");

    let env_report = run_business_path(&env);
    let mem_report = run_business_path(&mem);
    let sqlite_report = run_business_path(&sqlite);

    // Every observable — every write's refusal/success, every list row, the
    // removal's effect, every settings read, the risk/exit round-trip —
    // must be identical across the three backends.
    assert_eq!(
        env_report, mem_report,
        "the env backend and the in-memory backend disagree on the business path:\n\
         env: {env_report:#?}\nmem: {mem_report:#?}"
    );
    assert_eq!(
        env_report, sqlite_report,
        "the env backend and the SQLite backend disagree on the business path:\n\
         env: {env_report:#?}\nsqlite: {sqlite_report:#?}"
    );

    // The report must be non-trivial: a path where every operation failed
    // identically on all three backends would also "agree" — pin that the book
    // actually got built (two creates, then one row after the removal), the
    // risk/exit round-trip actually landed, and the settings cell actually
    // moved.
    assert!(
        env_report.iter().any(|l| l.starts_with("create a: Ok")),
        "alpha must be created on the env backend: {env_report:#?}"
    );
    assert!(
        env_report.iter().any(|l| l.contains("alpha")
            && l.contains("polymarket")
            && l.contains("frozen")
            && l.contains("[900, 3600]")),
        "the list row must carry every write: {env_report:#?}"
    );
    assert!(
        env_report.iter().any(|l| l
            == "list after remove: Ok([AccountRow { id: \"alpha\", \
             name: \"Alpha\", venue: \"polymarket\", status: \"frozen\", trading_enabled: true, \
             rounds: [900, 3600], strategies: [\"s1\", \"s2\"] }])"),
        "removal must take beta and only beta: {env_report:#?}"
    );
    assert!(
        env_report
            .iter()
            .any(|l| l.contains("get round: Some(300)")),
        "the settings cell must carry the round duration: {env_report:#?}"
    );
    assert!(
        env_report.iter().any(|l| l
            == "get risk alpha: [(\"cooldownMinutes\", \"30\"), (\"maxPositionSize\", \"5\")]"),
        "the risk round-trip must land both writes (the merge, not the replace): {env_report:#?}"
    );
    assert!(
        env_report.iter().any(|l| l
            == "get exit alpha: [(\"stopLossPct\", \"8\"), (\"takeProfitPct\", \"90\"), \
             (\"trailingEnabled\", \"true\")]"),
        "the exit round-trip must land all three fields: {env_report:#?}"
    );
}

/// The legal differences, enumerated (issue #1199 r2 P0-2 — the parity
/// proof's honest ledger): where the SqliteStore's semantics differ from
/// the env backend's, the difference is ASSERTED per backend, never
/// silenced.
#[test]
fn the_sqlite_backend_enumerates_its_legal_differences() {
    let sqlite = SqliteStore::new("diffs");

    // 1. `market.plugin.set` is not a StoreApi domain (the runtime-config
    // cell belongs to the launcher): the SQLite backend refuses.
    let plugin = sqlite.settings().set_market_plugin(Some("polymarket"));
    assert!(
        plugin.is_err(),
        "the sqlite backend must refuse market.plugin (no store face)"
    );
    assert!(
        plugin.unwrap_err().contains("no store face"),
        "the refusal must name the missing face"
    );
    // The env backend CARRIES the cell (the runtime-config cell + the env
    // key — the launcher's domain, the env carrier's write).
    let env = EnvStore::new("diffs-env");
    assert!(
        env.settings().set_market_plugin(Some("polymarket")).is_ok(),
        "the env backend carries the market-plugin cell (the launcher's domain)"
    );
    assert_eq!(
        env.settings().get_market_plugin().as_deref(),
        Some("polymarket"),
        "the env backend's cell reads back"
    );
    // The memory backend carries it too (the map).
    let mem = MemoryStore::new();
    assert!(mem.settings().set_market_plugin(Some("polymarket")).is_ok());
    assert_eq!(
        mem.settings().get_market_plugin().as_deref(),
        Some("polymarket")
    );

    // 2. `market.credentials.set`: the typed refusal (the per-account
    // var-name→account mapping is PR-3). The env backend writes `.env`, the
    // memory backend records — the SqliteStore refuses.
    let creds = sqlite
        .credentials()
        .save_credentials(&[("KALSHI_API_KEY", "k")], false);
    assert!(creds.is_err(), "the sqlite backend must refuse credentials");
    assert!(
        creds.unwrap_err().contains("PR-3"),
        "the refusal must name the PR-3 gate"
    );
    assert!(
        env.credentials()
            .save_credentials(&[("KALSHI_API_KEY", "k")], false)
            .is_ok(),
        "the env backend carries credentials (.env is the carrier)"
    );
    assert!(
        std::fs::read_to_string(env.env_path())
            .unwrap_or_default()
            .lines()
            .any(|l| {
                blitzkrieg_core::env_file::parse_line(l)
                    .map(|(k, v)| k == "KALSHI_API_KEY" && v == "k")
                    .unwrap_or(false)
            }),
        "the env backend's credential write landed on file"
    );
    assert!(
        mem.credentials()
            .save_credentials(&[("KALSHI_API_KEY", "k")], false)
            .is_ok(),
        "the memory backend records credentials"
    );

    // 3. The duplicate create: the env store refuses its own ACCOUNT_LIST
    // duplicate; the SqliteStore's create is INSERT OR IGNORE (idempotent).
    // The service layer's duplicate refusal (the book check) is the
    // production gate for both — the store-level difference is named.
    assert!(
        env.accounts().create("alpha", Some("Alpha")).is_ok(),
        "the env create lands the first time"
    );
    assert!(
        env.accounts().create("alpha", None).is_err(),
        "the env store refuses its own duplicate (the ACCOUNT_LIST check)"
    );
    assert!(
        sqlite.accounts().create("alpha", Some("Alpha")).is_ok(),
        "the sqlite create lands the first time"
    );
    assert!(
        sqlite.accounts().create("alpha", None).is_ok(),
        "the sqlite create is INSERT OR IGNORE — idempotent on the duplicate \
         (the named, legal store-level difference; the service layer refuses \
         the duplicate before the store in production)"
    );
    // Neither duplicate changed the BOOK the boot resolves (alpha exists
    // once, with its first-write name).
    assert_eq!(
        env.list()
            .unwrap()
            .iter()
            .filter(|r| r.id == "alpha")
            .count(),
        1,
        "the env book holds alpha once"
    );
    assert_eq!(
        sqlite
            .list()
            .unwrap()
            .iter()
            .filter(|r| r.id == "alpha")
            .count(),
        1,
        "the DB holds alpha once (INSERT OR IGNORE)"
    );

    // 4. A ghost account-book write materializes an `accounts` row in the
    // DB (the FK precondition) that the env backend's dead key does not —
    // and the sqlite list face, reading the accounts table, SHOWS the ghost
    // row where the env list face (the ACCOUNT_LIST book) does not. The
    // store-level difference is named and asserted, not silenced.
    let _ = sqlite.accounts().set_venue("ghost", "polymarket");
    let ghost_row = sqlite.list().unwrap().iter().any(|r| r.id == "ghost");
    assert!(
        ghost_row,
        "the sqlite list face shows the FK-materialized ghost row (the named, \
         legal store-level difference): {:?}",
        sqlite.list().unwrap()
    );
    // The row exists in the accounts table (the FK materialization).
    let ghost_accounts: i64 = sqlite.readback.with_connection(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM accounts WHERE id = 'ghost'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
    });
    assert_eq!(
        ghost_accounts, 1,
        "the DB materializes the ghost row (the FK precondition) — the named, \
         legal store-level difference"
    );
    // And the env backend's ghost write leaves a dead key, not a book row
    // (the env binding above already carries the market-plugin/credentials
    // writes; the ghost uses the same scratch).
    let _ = env.accounts().set_venue("ghost", "polymarket");
    assert!(
        !env.list().unwrap().iter().any(|r| r.id == "ghost"),
        "the env backend's ghost is a dead key, never a book row"
    );
    assert!(
        std::fs::read_to_string(env.env_path())
            .unwrap_or_default()
            .contains("ACCOUNT_GHOST_VENUE"),
        "the env backend's ghost write left its dead key on file (the named, \
         legal store-level difference)"
    );
}

/// The refusal discipline must agree on the OBSERVABLE face: an unknown
/// account is refused (or, on the env backend, writes a key the book never
/// resolves — the write-side existence check is the service layer's, per
/// the grammar) — and on the env/memory backends the ghost never
/// materializes in the book. The issue's "no implicit ledger" discipline is
/// about the BOOK the BOOT resolves (ACCOUNT_LIST), and the env/memory
/// backends keep it; the SQLite list face reads the accounts table, where
/// the FK precondition materializes the row (the named, legal store-level
/// difference asserted in `the_sqlite_backend_enumerates_its_legal_differences`).
#[test]
fn unknown_account_never_materializes_in_the_book() {
    let env = EnvStore::new("ghost");
    let mem = MemoryStore::new();

    for store in [&env as &dyn StoreApi, &mem as &dyn StoreApi] {
        let a = store.accounts();
        // every write face against a ghost id: refused by name (memory) or
        // a dead key (env) — both are legal backend semantics, and the
        // refusal text itself is not the contract.
        let _ = a.set_venue("ghost", "polymarket");
        let _ = a.set_rounds("ghost", &[900]);
        let _ = a.set_status("ghost", "frozen");
        let _ = a.set_trading_enabled("ghost", false);
        let _ = a.set_strategies("ghost", &["s1"]);
        let _ = a.remove("ghost");
        // THE invariant: no ghost row in the book.
        assert!(
            !a.list().unwrap().iter().any(|r| r.id == "ghost"),
            "a ghost account must never materialize in the book: {:?}",
            a.list().unwrap()
        );
    }

    // The memory backend's stronger face (write-side refusal, by name) is
    // the one the service layer's parity contract will standardize on:
    // pin it so a future change keeps refusing rather than degrading to
    // the env backend's dead-key shape.
    let mem_refused = {
        let a = mem.accounts();
        (
            a.set_venue("ghost2", "polymarket").is_err(),
            a.remove("ghost2").is_err(),
        )
    };
    assert!(
        mem_refused.0 && mem_refused.1,
        "memory backend must refuse unknown ids by name"
    );
}

/// The SqliteStore's risk/exit write is the store-seam twin of the raw
/// `AccountRiskDb::save_risk` / `save_exit` pair (issue #1199 r2 P1-1's
/// verification, the task's point 2): the same durable facts — the
/// `account_risk` row + the `account_settings` order-notional key
/// (`source = 'panel'`), the `account_exit_policy` `policy_json` — with
/// the read-modify-write merge over the STORED row. The parity against the
/// raw pair pins the merge base: a partial second write must MERGE (the
/// first values survive), not replace.
#[test]
fn sqlite_risk_exit_match_the_raw_account_risk_db_pair() {
    let dir = std::env::temp_dir().join(format!(
        "bk-parity-raw-pair-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    // The raw pair: an AccountRiskDb on its own file.
    let raw = blitzkrieg_core::risk::account_store::AccountRiskDb::open(dir.join("raw.sqlite"))
        .expect("raw db opens");
    let raw_row = AccountRiskLimits {
        max_position_size: Bound::new(dec!(5), LimitSource::Flag),
        ..Default::default()
    };
    raw.save_risk("alpha", &raw_row).unwrap();
    let raw_exit = ExitConfig {
        stop_loss_pct: dec!(8),
        ..Default::default()
    };
    raw.save_exit("alpha", &raw_exit).unwrap();

    // The store: the same writes through the store seam.
    let store =
        blitzkrieg_core::store::SqliteStore::open(dir.join("store.sqlite")).expect("store opens");
    let fields: Vec<(String, String)> = vec![
        ("maxPositionSize".to_string(), "5".to_string()),
        ("maxOrderNotionalUsd".to_string(), "0".to_string()),
    ];
    store
        .save_risk_bounds(Some("alpha"), &fields)
        .expect("store risk write");
    let exit_fields: Vec<(String, String)> = vec![
        ("stopLossPct".to_string(), "8".to_string()),
        ("forceExitSec".to_string(), "0".to_string()),
    ];
    store
        .save_exit(Some("alpha"), &exit_fields)
        .expect("store exit write");

    // The durable facts agree (the values; the source column is 'panel' on
    // both — the row's provenance label, the load face spells the values
    // as env, the read face's label).
    let raw_rows = raw.load_risk();
    let store_rows = store.risk_db().load_risk();
    assert_eq!(
        raw_rows["alpha"].max_position_size.value(),
        store_rows["alpha"].max_position_size.value(),
        "the risk row's value must match the raw pair's"
    );
    assert_eq!(
        raw_rows["alpha"].max_order_notional_usd.value(),
        store_rows["alpha"].max_order_notional_usd.value(),
        "the settings-key cap must match the raw pair's"
    );
    let raw_exits = raw.load_exits(&ExitConfig::default());
    let store_exits = store.risk_db().load_exits(&ExitConfig::default());
    assert_eq!(
        raw_exits["alpha"].stop_loss_pct, store_exits["alpha"].stop_loss_pct,
        "the exit row's value must match the raw pair's"
    );

    // The read-modify-write merge: a partial second write MERGES (the first
    // values survive — the parity test's step 10 pins the same at the
    // store face).
    let merge: Vec<(String, String)> = vec![("cooldownMinutes".to_string(), "30".to_string())];
    store
        .save_risk_bounds(Some("alpha"), &merge)
        .expect("merge write");
    let merged = store.risk_db().load_risk();
    assert_eq!(
        merged["alpha"].max_position_size.value(),
        dec!(5),
        "the merge must keep the first value"
    );
    assert_eq!(
        merged["alpha"].cooldown_minutes.value(),
        dec!(30),
        "the merge must land the second value"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
