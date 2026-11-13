//! One-shot legacy-data importer (issue #1189).
//!
//! The Node era left its history in plain files under `data/` — the order /
//! position / trade JSONL logs, the per-account ledger journals, a few state
//! snapshots — and its account / risk / exit / policy knobs in the `.env` the
//! kernel booted from. The SQLite database is the new home; this module is
//! the bridge that folds the old world into it, exactly once.
//!
//! Rules straight from the issue:
//!  - self-contained: the v1 DDL lives HERE ([`SCHEMA_V1`]), a verbatim copy
//!    of the canonical store schema (`db::schema`, PR #1190) — the importer
//!    builds its database from nothing and depends on no db module;
//!  - idempotent: `kernel_meta.import_v1` is the done-marker — a second run
//!    inserts nothing and returns the all-zero report;
//!  - additive only: every insert is INSERT OR IGNORE, and every legacy file
//!    the run read is renamed to `<name>.migrated` AFTER the transaction
//!    commits (best effort — a rename failure only warns; an existing
//!    `.migrated` target is skipped, so re-runs never touch it again);
//!  - money is TEXT end to end (the `decimal` wire spelling), never a float;
//!  - `config.toml` / `*.toml` are retired — never read, never imported.
//!
//! The legacy tree is read relative to the DATABASE's parent directory: the
//! deployment points the importer at `data/bank.db` and it folds the
//! `orders/`, `positions/`, `trades/`, `ledger/` trees and the state
//! snapshots sitting next to it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};

/// The v1 schema — a verbatim copy of the canonical store DDL (`db::schema`,
/// main PR #1190): 18 tables plus the 8 indexes the per-account lookups
/// need. Money columns are TEXT (the `decimal` wire spelling); `IF NOT
/// EXISTS` keeps the DDL re-runnable. The SQL is byte-identical to the
/// canonical `SCHEMA_V1` so an imported database and a migrated one are the
/// same shape.
const SCHEMA_V1: &str = r#"
-- Schema version ledger. The migration runner owns this table's bootstrap;
-- it is repeated here so SCHEMA_V1 alone is a complete v1 database.
CREATE TABLE IF NOT EXISTS schema_migrations (
  version       INTEGER PRIMARY KEY,
  applied_at_ms INTEGER NOT NULL
);

-- Kernel metadata (import marker, active/display account, update + runtime
-- cells). `dry_run` is deliberately NOT storable here: whether the process
-- simulates is decided by the environment alone (issue #1189 §2/§4).
CREATE TABLE IF NOT EXISTS kernel_meta (
  key           TEXT PRIMARY KEY,
  value         TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  CHECK (lower(key) <> 'dry_run')
);

CREATE TABLE IF NOT EXISTS accounts (
  id              TEXT PRIMARY KEY,
  name            TEXT NOT NULL,
  market_type     TEXT NOT NULL,          -- prediction / spot / ...
  status          TEXT NOT NULL,          -- active / read_only / frozen / suspended
  status_reason   TEXT,
  venue           TEXT NOT NULL DEFAULT '',
  trading_enabled INTEGER NOT NULL,
  rounds_json     TEXT NOT NULL DEFAULT '[]',
  opening_equity  TEXT,                   -- decimal string, never REAL
  updated_at_ms   INTEGER NOT NULL
);

-- Credential ENV VAR NAMES only, plus the AES-256-GCM sealed payload
-- (nonce || ciphertext || tag) and the name of the env var holding the
-- master key. The plaintext value never lands in this table.
CREATE TABLE IF NOT EXISTS account_credentials (
  account_id         TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  api_key_env        TEXT,
  secret_env         TEXT,
  passphrase_env     TEXT,
  private_key_env    TEXT,
  encrypted_payload  BLOB,
  encryption_key_env TEXT,
  updated_at_ms      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS account_risk (
  account_id             TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  max_single_loss_usd    TEXT NOT NULL,
  max_daily_drawdown_usd TEXT NOT NULL,
  max_position_size      TEXT NOT NULL,
  max_consecutive_losses TEXT NOT NULL,
  cooldown_minutes       TEXT NOT NULL,
  source                 TEXT NOT NULL,   -- default / imported_env / imported_toml / panel / flag
  updated_at_ms          INTEGER NOT NULL
);

-- The full ExitConfig as JSON; `source` as in account_risk.
CREATE TABLE IF NOT EXISTS account_exit_policy (
  account_id    TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
  policy_json   TEXT NOT NULL,
  source        TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

-- Execution policy, sizing ratios, daily loss, round anchor, hot caps, the
-- shadow-evolution copy. Money and ratios are decimal strings; POLICY_RULES
-- values are JSON text.
CREATE TABLE IF NOT EXISTS account_settings (
  account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  key           TEXT NOT NULL,
  value         TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (account_id, key)
);

CREATE TABLE IF NOT EXISTS strategy_assignments (
  account_id         TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  strategy           TEXT NOT NULL,
  enabled            INTEGER NOT NULL,
  params_json        TEXT,
  round_duration_sec INTEGER,             -- STRATEGY_<NAME>_ROUND_DURATION_SEC
  updated_at_ms      INTEGER NOT NULL,
  PRIMARY KEY (account_id, strategy)
);

CREATE TABLE IF NOT EXISTS venues (
  id          TEXT PRIMARY KEY,
  kind        TEXT NOT NULL DEFAULT '',
  config_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS markets (
  id         TEXT PRIMARY KEY,
  venue_id   TEXT NOT NULL,
  account_id TEXT,
  meta_json  TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS rounds (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id    TEXT NOT NULL,
  venue_id      TEXT,
  market_id     TEXT,
  duration_sec  INTEGER,
  started_at_ms INTEGER,
  ends_at_ms    INTEGER,
  state_json    TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS orders (
  account_id    TEXT NOT NULL,
  order_id      TEXT NOT NULL,
  payload_json  TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (account_id, order_id)
);

CREATE TABLE IF NOT EXISTS fills (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   TEXT NOT NULL,
  order_id     TEXT,
  payload_json TEXT NOT NULL,
  ts_ms        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS positions (
  account_id    TEXT NOT NULL,
  position_id   TEXT NOT NULL,
  payload_json  TEXT NOT NULL,
  open          INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (account_id, position_id)
);

CREATE TABLE IF NOT EXISTS trades (
  id           TEXT PRIMARY KEY,
  account_id   TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  exit_time    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS ledger_entries (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   TEXT NOT NULL,
  kind         TEXT NOT NULL,
  amount       TEXT NOT NULL,             -- decimal string
  payload_json TEXT NOT NULL,
  ts_ms        INTEGER NOT NULL
);

-- Cross-account arbitrage legs. v1 holds exactly one row (id = 1); it belongs
-- to two accounts, so it is NOT folded into a single account's settings.
CREATE TABLE IF NOT EXISTS arb_pairs (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  leg_a_venue   TEXT NOT NULL,
  leg_a_account TEXT NOT NULL,
  leg_b_venue   TEXT NOT NULL,
  leg_b_account TEXT NOT NULL,
  pairs_json    TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts_ms       INTEGER NOT NULL,
  account_id  TEXT,
  actor       TEXT NOT NULL,
  action      TEXT NOT NULL,
  detail_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_fills_account     ON fills(account_id, ts_ms);
CREATE INDEX IF NOT EXISTS idx_ledger_account    ON ledger_entries(account_id, ts_ms);
CREATE INDEX IF NOT EXISTS idx_audit_ts          ON audit_log(ts_ms);
CREATE INDEX IF NOT EXISTS idx_rounds_account    ON rounds(account_id);
CREATE INDEX IF NOT EXISTS idx_orders_account    ON orders(account_id, updated_at_ms);
CREATE INDEX IF NOT EXISTS idx_positions_account ON positions(account_id, open);
CREATE INDEX IF NOT EXISTS idx_trades_account    ON trades(account_id, exit_time);
CREATE INDEX IF NOT EXISTS idx_markets_venue     ON markets(venue_id);
"#;

/// Every table the v1 schema creates, in dependency order (parents first) —
/// the canonical inventory, asserted against `sqlite_master` by the tests.
pub const TABLES: &[&str] = &[
    "schema_migrations",
    "kernel_meta",
    "accounts",
    "account_credentials",
    "account_risk",
    "account_exit_policy",
    "account_settings",
    "strategy_assignments",
    "venues",
    "markets",
    "rounds",
    "orders",
    "fills",
    "positions",
    "trades",
    "ledger_entries",
    "arb_pairs",
    "audit_log",
];

/// Every index the v1 schema creates — the canonical inventory.
pub const INDEXES: &[&str] = &[
    "idx_fills_account",
    "idx_ledger_account",
    "idx_audit_ts",
    "idx_rounds_account",
    "idx_orders_account",
    "idx_positions_account",
    "idx_trades_account",
    "idx_markets_venue",
];

/// What one import run folded in. Counts are rows actually INSERTED, so the
/// second (idempotent) run reports all zeros. `skipped` lists every legacy
/// input that was absent (missing files) or already carried a `.migrated`
/// backup (renamed in an earlier run).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub accounts: usize,
    pub risk_rows: usize,
    pub exit_rows: usize,
    pub settings: usize,
    pub orders: usize,
    pub positions: usize,
    pub trades: usize,
    pub ledger_entries: usize,
    pub audit: usize,
    pub skipped: Vec<String>,
}

/// The account a pre-E28 row reads as (the serde default the live loaders
/// use): `account_id` absent = the `default` book.
const DEFAULT_ACCOUNT: &str = "default";

/// The cockpit systemic-risk family (`BK_RISK_*`, COCKPIT_KEYS minus the
/// exit family) — the `account_risk` row set, global value with a
/// per-account `_SFX` spelling on top. These five land in the fixed
/// `account_risk` columns; `MAX_ORDER_NOTIONAL_USD` is a settings key, not
/// a risk-matrix column (see [`SETTING_KEYS`]).
const RISK_KEYS: &[&str] = &[
    "MAX_SINGLE_LOSS_USD",
    "MAX_DAILY_DRAWDOWN_USD",
    "MAX_POSITION_SIZE",
    "MAX_CONSECUTIVE_LOSSES",
    "COOLDOWN_MINUTES",
];

/// The cockpit exit family (`BK_EXIT_*`) — folded per account into one
/// `policy_json` object (the `ExitConfig` JSON projection).
const EXIT_KEYS: &[&str] = &[
    "STOP_LOSS_PCT",
    "TAKE_PROFIT_PCT",
    "FORCE_EXIT_SEC",
    "TRAILING_ENABLED",
    "STRATEGY_EXIT_MAKER_FIRST",
];

/// The `account_settings` key set: the cockpit systemic cap the risk matrix
/// has no column for (`MAX_ORDER_NOTIONAL_USD`), the execution-policy family
/// (`POLICY_*` wins over the bare alias), the pre-#751 bare names
/// (`BUDGET_RATIO`, `MAX_BUDGET_USD`), and the launcher's sizing /
/// daily-loss / notional caps.
const SETTING_KEYS: &[&str] = &[
    "BK_RISK_MAX_ORDER_NOTIONAL_USD",
    "BUDGET_RATIO",
    "MIN_BUDGET_USD",
    "MAX_BUDGET_USD",
    "MIN_EQUITY_USD",
    "MAX_POSITIONS_PER_ASSET",
    "RULES",
    "BK_SIZE_PCT",
    "BK_MAX_DAILY_LOSS",
    "BK_MAX_DAILY_LOSS_PCT",
    "BK_MAX_ORDER_NOTIONAL_PCT",
];

/// The audit actor a legacy import lands under.
const IMPORT_ACTOR: &str = "db_import_v1";

/// Import the legacy world into `db_path`, exactly once.
///
/// `env_lookup` is the `.env` read face (the kernel passes
/// `|k| std::env::var(k).ok()`); every account / risk / exit / policy knob
/// is read through it. Returns what was folded in — the second run sees the
/// `import_v1` marker and returns the all-zero report.
///
/// After the transaction commits, every legacy file the run actually read
/// is renamed to `<name>.migrated` (best effort: a rename failure only
/// warns; an existing `.migrated` target is skipped — the file was already
/// folded by an earlier run and must not be read twice).
pub fn import_once(
    db_path: &Path,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<ImportReport> {
    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create database dir {}", parent.display()))?;
    }
    let mut conn = Connection::open(db_path)
        .with_context(|| format!("open import database {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA_V1)?;

    // The done-marker: a present import_v1 makes this run a no-op.
    let marker: Option<()> = conn
        .query_row(
            "SELECT 1 FROM kernel_meta WHERE key = 'import_v1'",
            [],
            |_| Ok(()),
        )
        .optional()?;
    if marker.is_some() {
        return Ok(ImportReport::default());
    }

    let data_dir = legacy_data_dir(db_path);
    let accounts = account_ids(&env_lookup);
    let now = now_ms();
    let mut report = ImportReport::default();
    let mut skipped: Vec<String> = Vec::new();
    // The files this run actually read (present, non-`.migrated`): the ones
    // the post-commit backup phase renames. Ledger journals are per-account
    // and get their own entries appended as they are folded.
    let mut imported: Vec<PathBuf> = Vec::new();

    // One transaction: the whole import lands together with its marker,
    // migration row and audit row — a crash mid-import leaves NO
    // half-imported database behind.
    let tx = conn.transaction()?;
    for id in &accounts {
        report.accounts += insert_account(&tx, id, &env_lookup)?;
        report.risk_rows += insert_account_risk(&tx, id, &env_lookup)?;
        report.exit_rows += insert_account_exit_policy(&tx, id, &env_lookup)?;
        report.settings += insert_account_settings(&tx, id, &env_lookup)?;
    }
    report.orders = fold_orders(&tx, &data_dir, &mut imported, &mut skipped)?;
    report.positions = fold_positions(&tx, &data_dir, &mut imported, &mut skipped)?;
    report.trades = fold_trades(&tx, &data_dir, &mut imported, &mut skipped)?;
    report.ledger_entries = fold_ledgers(&tx, &data_dir, &accounts, &mut imported, &mut skipped)?;
    import_state_files(&tx, &data_dir, now, &mut imported, &mut skipped)?;
    tx.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at_ms) VALUES (1, ?1)",
        params![now],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO kernel_meta (key, value, updated_at_ms) \
         VALUES ('import_v1', '1', ?1)",
        params![now],
    )?;
    let detail = serde_json::json!({
        "accounts": report.accounts,
        "risk_rows": report.risk_rows,
        "exit_rows": report.exit_rows,
        "settings": report.settings,
        "orders": report.orders,
        "positions": report.positions,
        "trades": report.trades,
        "ledger_entries": report.ledger_entries,
        "skipped": skipped.len(),
    })
    .to_string();
    report.audit = tx.execute(
        "INSERT INTO audit_log (ts_ms, account_id, actor, action, detail_json) \
         VALUES (?1, NULL, ?2, 'db_import_v1', ?3)",
        params![now, IMPORT_ACTOR, detail],
    )?;
    tx.commit()?;

    // Post-commit backup: rename every file the run read. Best effort —
    // failures warn and never fail the import (the database is the source
    // of truth now; the rename only stops a second reader from double-
    // counting the file).
    for path in &imported {
        backup_as_migrated(path, &mut skipped);
    }
    report.skipped = skipped;
    Ok(report)
}

/// Rename `path` to `<path>.migrated` (i.e. `orders.jsonl` →
/// `orders.jsonl.migrated`). Skips (records in `skipped`) when the target
/// already exists — the file was folded by an earlier run. A rename failure
/// only warns.
fn backup_as_migrated(path: &Path, skipped: &mut Vec<String>) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let target = path.with_extension(format!(
        "{}.migrated",
        path.extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    if target.exists() {
        skipped.push(format!("{name} (already migrated)"));
        return;
    }
    if let Err(e) = std::fs::rename(path, &target) {
        eprintln!(
            "warn: db_import: backup rename {} -> {} failed: {e}",
            path.display(),
            target.display()
        );
        skipped.push(format!("{name} (backup failed)"));
        return;
    }
    skipped.push(format!("{name} (migrated)"));
}

/// The legacy `data/` tree: the database's own directory (the deployment
/// keeps `bank.db` inside `data/`); a bare filename falls back to `.`.
fn legacy_data_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `ACCOUNT_LIST` (comma-separated, default `["default"]`), de-duplicated in
/// first-spelling order.
fn account_ids(env_lookup: &impl Fn(&str) -> Option<String>) -> Vec<String> {
    let raw = env_lookup("ACCOUNT_LIST").unwrap_or_default();
    let mut ids: Vec<String> = Vec::new();
    for id in raw
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
    {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        ids.push(DEFAULT_ACCOUNT.to_string());
    }
    ids
}

/// The env-key suffix of an account id: uppercase, non-alphanumeric folded to
/// `_` — the same spelling the execution policy's per-account keys use.
fn env_account_suffix(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_ascii_uppercase()
}

/// One `accounts` row per id: name / venue / status / trading_enabled from
/// `ACCOUNT_<SFX>_*`, defaulting to the id / '' / active / 1. `market_type`
/// is a placeholder (`prediction`) — the canonical schema requires the
/// column and the legacy `.env` carries no market-type knob.
fn insert_account(
    tx: &Connection,
    account_id: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<usize> {
    let sfx = env_account_suffix(account_id);
    let name = env(&format!("ACCOUNT_{sfx}_NAME")).unwrap_or_else(|| account_id.to_string());
    let venue = env(&format!("ACCOUNT_{sfx}_VENUE")).unwrap_or_default();
    let status = env(&format!("ACCOUNT_{sfx}_STATUS")).unwrap_or_else(|| "active".to_string());
    let trading_enabled = env(&format!("ACCOUNT_{sfx}_TRADING_ENABLED"))
        .map(|raw| !matches!(raw.trim(), "false" | "0" | "no" | "off"))
        .unwrap_or(true);
    Ok(tx.execute(
        "INSERT OR IGNORE INTO accounts \
         (id, name, market_type, status, venue, trading_enabled, updated_at_ms) \
         VALUES (?1, ?2, 'prediction', ?3, ?4, ?5, ?6)",
        params![account_id, name, status, venue, trading_enabled, now_ms()],
    )?)
}

/// One `account_risk` row per account: the five cockpit systemic limits,
/// global `BK_RISK_*` value with the `BK_RISK_*_<SFX>` spelling on top
/// (per-account value wins). Fixed columns, not EAV — `source` records where
/// the row came from (`imported_env`). Every column is NOT NULL, so a
/// missing env value falls back to the shipped `'0'` (the factory posture:
/// bound off).
fn insert_account_risk(
    tx: &Connection,
    account_id: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<usize> {
    let sfx = env_account_suffix(account_id);
    let mut values: Vec<String> = Vec::with_capacity(RISK_KEYS.len());
    for key in RISK_KEYS {
        let value = env(&format!("BK_RISK_{key}_{sfx}")).or_else(|| env(&format!("BK_RISK_{key}")));
        values.push(value.unwrap_or_else(|| "0".to_string()));
    }
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(RISK_KEYS.len() + 3);
    params.push(&account_id);
    for value in &values {
        params.push(value);
    }
    params.push(&"imported_env");
    let now = now_ms();
    params.push(&now);
    Ok(tx.execute(
        "INSERT OR IGNORE INTO account_risk \
         (account_id, max_single_loss_usd, max_daily_drawdown_usd, max_position_size, \
          max_consecutive_losses, cooldown_minutes, source, updated_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params_from_iter(params),
    )?)
}

/// One `account_exit_policy` row per account (skipped when the env carries no
/// `BK_EXIT_*` value for it): the global exit knobs with the `_SFX` spelling
/// on top, rendered as one `policy_json` object. Percentages and seconds are
/// JSON NUMBERS (the `policy_json` contract: `{"stop_loss_pct":12,
/// "take_profit_pct":100}` — numeric, not stringly).
fn insert_account_exit_policy(
    tx: &Connection,
    account_id: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<usize> {
    let sfx = env_account_suffix(account_id);
    let mut policy: Map<String, Value> = Map::new();
    for key in EXIT_KEYS {
        let value = env(&format!("BK_EXIT_{key}_{sfx}")).or_else(|| env(&format!("BK_EXIT_{key}")));
        if let Some(value) = value {
            // Numbers stay numbers; booleans parse; anything else is kept
            // as the raw string (the panel's unparseable-value rule).
            let parsed = if matches!(key, &"TRAILING_ENABLED" | &"STRATEGY_EXIT_MAKER_FIRST") {
                match value.trim() {
                    "true" => Some(Value::Bool(true)),
                    "false" => Some(Value::Bool(false)),
                    _ => None,
                }
            } else {
                value
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .map(Value::from)
                    .or_else(|| {
                        value
                            .trim()
                            .parse::<f64>()
                            .ok()
                            .and_then(|n| serde_json::Number::from_f64(n).map(Value::Number))
                    })
            };
            policy.insert((*key).to_string(), parsed.unwrap_or(Value::String(value)));
        }
    }
    if policy.is_empty() {
        return Ok(0);
    }
    let policy_json = serde_json::json!(policy).to_string();
    Ok(tx.execute(
        "INSERT OR IGNORE INTO account_exit_policy (account_id, policy_json, source, updated_at_ms) \
         VALUES (?1, ?2, 'imported_env', ?3)",
        params![account_id, policy_json, now_ms()],
    )?)
}

/// The `account_settings` rows for every account: `POLICY_*` beats the bare
/// alias for every key that has both (the execution policy's env rule), and
/// `BK_RISK_MAX_ORDER_NOTIONAL_USD` (the systemic cap with no risk-matrix
/// column) lands here as a plain setting.
fn insert_account_settings(
    tx: &Connection,
    account_id: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<usize> {
    let mut rows: BTreeMap<&str, String> = BTreeMap::new();
    for key in SETTING_KEYS {
        let value = env(&format!("POLICY_{key}")).or_else(|| env(key));
        if let Some(value) = value {
            rows.insert(key, value);
        }
    }
    let mut inserted = 0;
    for (key, value) in &rows {
        inserted += tx.execute(
            "INSERT OR IGNORE INTO account_settings (account_id, key, value, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4)",
            params![account_id, key, value, now_ms()],
        )?;
    }
    Ok(inserted)
}

/// `data/orders/orders.jsonl` → `orders`: the log is append-only CURRENT
/// snapshots, so fold to the LATEST line per order id (later lines win — the
/// `OrderDb::load` rule) and insert one row per id. The whole original line
/// is the `payload_json` (no business-column split); `order_id` comes from
/// the row's id fields; `updated_at_ms` from the row (fallback: now).
fn fold_orders(
    tx: &Connection,
    data_dir: &Path,
    imported: &mut Vec<PathBuf>,
    skipped: &mut Vec<String>,
) -> anyhow::Result<usize> {
    const REL: &str = "orders/orders.jsonl";
    let path = data_dir.join(REL);
    if !path.exists() {
        skipped.push(REL.to_string());
        return Ok(0);
    }
    if path.with_extension("jsonl.migrated").exists() {
        // Already folded by an earlier run; the marker makes the whole
        // import a no-op anyway, but skip the rename dance explicitly.
        skipped.push(REL.to_string());
        return Ok(0);
    }
    imported.push(path.clone());
    let mut latest: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for (line, row) in read_jsonl(&path) {
        if let Some(id) = str_field(&row, &["orderId", "order_id", "id"])
            && !id.is_empty()
        {
            latest.insert(id, (line, row));
        }
    }
    let mut inserted = 0;
    for (id, (payload, row)) in &latest {
        inserted += tx.execute(
            "INSERT OR IGNORE INTO orders (account_id, order_id, payload_json, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                account_of(row),
                id,
                payload,
                i64_field(
                    row,
                    &[
                        "updatedAtMs",
                        "updated_at_ms",
                        "submittedAtMs",
                        "submitted_at_ms"
                    ]
                )
                .unwrap_or_else(now_ms),
            ],
        )?;
    }
    Ok(inserted)
}

/// `data/positions/positions.dry.jsonl` then `positions.jsonl` →
/// `positions`: the dry snapshot loads first, the live one overwrites — same
/// id, live wins. The whole line is `payload_json`; the `open` flag comes
/// from the row (absent = still open — the live book's default).
fn fold_positions(
    tx: &Connection,
    data_dir: &Path,
    imported: &mut Vec<PathBuf>,
    skipped: &mut Vec<String>,
) -> anyhow::Result<usize> {
    let mut folded: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for rel in ["positions/positions.dry.jsonl", "positions/positions.jsonl"] {
        let path = data_dir.join(rel);
        if !path.exists() {
            skipped.push(rel.to_string());
            continue;
        }
        if path.with_extension("jsonl.migrated").exists() {
            skipped.push(rel.to_string());
            continue;
        }
        imported.push(path.clone());
        for (line, row) in read_jsonl(&path) {
            if let Some(id) = str_field(&row, &["id", "position_id"])
                && !id.is_empty()
            {
                folded.insert(id, (line, row));
            }
        }
    }
    let mut inserted = 0;
    for (id, (payload, row)) in &folded {
        inserted += tx.execute(
            "INSERT OR IGNORE INTO positions (account_id, position_id, payload_json, open, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                account_of(row),
                id,
                payload,
                open_flag(row),
                i64_field(row, &["updatedAtMs", "updated_at_ms", "enteredAtMs", "entered_at_ms"])
                    .unwrap_or_else(now_ms),
            ],
        )?;
    }
    Ok(inserted)
}

/// The `open` flag of a legacy position row: an explicit `open` / `isOpen`
/// field wins, a status field is the fallback (`Closed` / `closed` = 0).
/// Absent both = still open (the live book's default).
fn open_flag(row: &Value) -> bool {
    if let Some(flag) = bool_field(row, &["open", "isOpen"]) {
        return flag;
    }
    if let Some(status) = str_field(row, &["status"]) {
        return !matches!(status.trim(), "closed" | "Closed" | "settled" | "Settled");
    }
    true
}

/// `data/trades/trades.jsonl` then `trades-live.jsonl` → `trades`: dry log
/// first, live log overwrites — same id, live wins. The whole line is
/// `payload_json`; `exit_time` from the row (fallback: now).
fn fold_trades(
    tx: &Connection,
    data_dir: &Path,
    imported: &mut Vec<PathBuf>,
    skipped: &mut Vec<String>,
) -> anyhow::Result<usize> {
    let mut folded: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for rel in ["trades/trades.jsonl", "trades/trades-live.jsonl"] {
        let path = data_dir.join(rel);
        if !path.exists() {
            skipped.push(rel.to_string());
            continue;
        }
        if path.with_extension("jsonl.migrated").exists() {
            skipped.push(rel.to_string());
            continue;
        }
        imported.push(path.clone());
        for (line, row) in read_jsonl(&path) {
            if let Some(id) = str_field(&row, &["id", "trade_id"])
                && !id.is_empty()
            {
                folded.insert(id, (line, row));
            }
        }
    }
    let mut inserted = 0;
    for (id, (payload, row)) in &folded {
        inserted += tx.execute(
            "INSERT OR IGNORE INTO trades (id, account_id, payload_json, exit_time) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                id,
                account_of(row),
                payload,
                i64_field(
                    row,
                    &["exitTime", "exit_time", "closedAtMs", "closed_at_ms"]
                )
                .unwrap_or_else(now_ms),
            ],
        )?;
    }
    Ok(inserted)
}

/// `data/ledger/<account>.journal.dry.jsonl` / `.live.jsonl` → `ledger_entries`:
/// one row per journal line; `kind` / `amount` are read from the row (the
/// amount stays the TEXT it was serialized as), the whole line is
/// `payload_json`, `ts_ms` from the row (fallback: now). The autoincrement
/// `id` is the database's business, not the import's.
fn fold_ledgers(
    tx: &Connection,
    data_dir: &Path,
    accounts: &[String],
    imported: &mut Vec<PathBuf>,
    skipped: &mut Vec<String>,
) -> anyhow::Result<usize> {
    let mut inserted = 0;
    for account in accounts {
        for mode in ["dry", "live"] {
            let rel = format!("ledger/{account}.journal.{mode}.jsonl");
            let path = data_dir.join(&rel);
            if !path.exists() {
                skipped.push(rel);
                continue;
            }
            if path.with_extension("jsonl.migrated").exists() {
                skipped.push(rel);
                continue;
            }
            imported.push(path.clone());
            for (line, row) in read_jsonl(&path) {
                let kind = str_field(&row, &["kind"]).unwrap_or_default();
                let amount = str_field(&row, &["amount"]).unwrap_or_default();
                let at_ms = i64_field(&row, &["atMs", "at_ms", "ts", "tsMs", "ts_ms"]);
                inserted += tx.execute(
                    "INSERT INTO ledger_entries (account_id, kind, amount, payload_json, ts_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![account, kind, amount, line, at_ms.unwrap_or_else(now_ms),],
                )?;
            }
        }
    }
    Ok(inserted)
}

/// The state snapshots → `kernel_meta` (raw file contents, no re-encoding):
/// `data/state.json` → `legacy_state_json`, `data/runtime/config.json` →
/// `runtime_config_json`, `data/update/state.json` → `update_state_json`. A
/// missing file is skipped-and-noted, never fatal. Files the run actually
/// read are queued for the `.migrated` backup.
fn import_state_files(
    tx: &Connection,
    data_dir: &Path,
    now: i64,
    imported: &mut Vec<PathBuf>,
    skipped: &mut Vec<String>,
) -> anyhow::Result<()> {
    for (rel, key) in [
        ("state.json", "legacy_state_json"),
        ("runtime/config.json", "runtime_config_json"),
        ("update/state.json", "update_state_json"),
    ] {
        let path = data_dir.join(rel);
        if path.with_extension("json.migrated").exists() {
            skipped.push(rel.to_string());
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            skipped.push(rel.to_string());
            continue;
        };
        imported.push(path);
        tx.execute(
            "INSERT OR IGNORE INTO kernel_meta (key, value, updated_at_ms) VALUES (?1, ?2, ?3)",
            params![key, text, now],
        )?;
    }
    Ok(())
}

/// One JSON object per line; blank and unparseable lines are skipped, never
/// fatal (the crate-wide JSONL tolerance rule — a corrupt tail must not fail
/// an import). Returns the RAW line text alongside the parsed value: the
/// payload columns keep the file's own bytes, not a re-serialization.
fn read_jsonl(path: &Path) -> Vec<(String, Value)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let row: Value = serde_json::from_str(line).ok()?;
            row.as_object()?;
            Some((line.to_string(), row))
        })
        .collect()
}

/// The first present field as a string; numbers stringify (money stays the
/// TEXT it was born as).
fn str_field(row: &Value, names: &[&str]) -> Option<String> {
    let obj = row.as_object()?;
    names.iter().find_map(|name| match obj.get(*name)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// The first present field as an i64 (a numeric string parses too — legacy
/// lines wrote some numbers as strings).
fn i64_field(row: &Value, names: &[&str]) -> Option<i64> {
    let obj = row.as_object()?;
    names.iter().find_map(|name| {
        let value = obj.get(*name)?;
        value
            .as_i64()
            .or_else(|| value.as_str()?.trim().parse().ok())
    })
}

/// The first present field as a bool (a `0`/`1` numeric or a `"true"`/
/// `"false"` string both parse).
fn bool_field(row: &Value, names: &[&str]) -> Option<bool> {
    let obj = row.as_object()?;
    names.iter().find_map(|name| {
        let value = obj.get(*name)?;
        match value {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => n.as_i64().map(|i| i != 0),
            Value::String(s) => match s.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            _ => None,
        }
    })
}

/// The account a legacy row settled in — absent `account_id` reads as the
/// `default` book (the serde default the live loaders use).
fn account_of(row: &Value) -> String {
    str_field(row, &["account_id", "accountId"]).unwrap_or_else(|| DEFAULT_ACCOUNT.to_string())
}

/// Milliseconds since the Unix epoch, or 0 if the clock is before it.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "db_import_tests.rs"]
mod tests;
