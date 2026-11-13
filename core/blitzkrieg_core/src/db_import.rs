//! One-shot legacy-data importer (issue #1189).
//!
//! The Node era left its history in plain files under `data/` — the order /
//! position / trade JSONL logs, the per-account ledger journals, a few state
//! snapshots — and its account / risk / exit / policy knobs in the `.env` the
//! kernel booted from. The SQLite database is the new home; this module is
//! the bridge that folds the old world into it, exactly once.
//!
//! Rules straight from the issue:
//!  - self-contained: the v1 DDL lives HERE ([`SCHEMA_V1`]) — the importer
//!    builds its database from nothing and depends on no db module;
//!  - idempotent: `kernel_meta.import_v1` is the done-marker — a second run
//!    inserts nothing and returns the all-zero report;
//!  - additive only: every insert is INSERT OR IGNORE, and NOTHING under
//!    `data/` is modified or deleted (backups are the caller's call);
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

/// The v1 schema — issue #1189 §2: fourteen tables plus the indexes the
/// per-account lookups need. Money columns are TEXT (the `decimal` wire
/// spelling); `IF NOT EXISTS` keeps the DDL re-runnable.
const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL DEFAULT '',
    venue           TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL DEFAULT 'active',
    trading_enabled INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT
);
CREATE TABLE IF NOT EXISTS account_credentials (
    account_id TEXT NOT NULL,
    env_key    TEXT NOT NULL,
    env_value  TEXT NOT NULL,
    PRIMARY KEY (account_id, env_key),
    FOREIGN KEY (account_id) REFERENCES accounts (id)
);
CREATE TABLE IF NOT EXISTS account_risk (
    account_id TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (account_id, key),
    FOREIGN KEY (account_id) REFERENCES accounts (id)
);
CREATE TABLE IF NOT EXISTS account_exit_policy (
    account_id  TEXT PRIMARY KEY,
    policy_json TEXT NOT NULL,
    FOREIGN KEY (account_id) REFERENCES accounts (id)
);
CREATE TABLE IF NOT EXISTS account_settings (
    account_id TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    PRIMARY KEY (account_id, key),
    FOREIGN KEY (account_id) REFERENCES accounts (id)
);
CREATE TABLE IF NOT EXISTS strategy_assignments (
    account_id  TEXT NOT NULL,
    strategy    TEXT NOT NULL,
    config_json TEXT,
    enabled     INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (account_id, strategy),
    FOREIGN KEY (account_id) REFERENCES accounts (id)
);
CREATE TABLE IF NOT EXISTS orders (
    id              TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL DEFAULT 'default',
    strategy        TEXT,
    asset           TEXT,
    direction       TEXT,
    side            TEXT,
    status          TEXT,
    price           TEXT,
    size            TEXT,
    filled_size     TEXT,
    payload_json    TEXT NOT NULL,
    submitted_at_ms INTEGER,
    updated_at_ms   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_orders_account ON orders (account_id);
CREATE TABLE IF NOT EXISTS fills (
    id           TEXT PRIMARY KEY,
    order_id     TEXT NOT NULL,
    account_id   TEXT NOT NULL DEFAULT 'default',
    price        TEXT NOT NULL,
    size         TEXT NOT NULL,
    fee_usd      TEXT,
    filled_at_ms INTEGER,
    payload_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_fills_order ON fills (order_id);
CREATE TABLE IF NOT EXISTS positions (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL DEFAULT 'default',
    strategy      TEXT,
    asset         TEXT,
    direction     TEXT,
    entry_price   TEXT,
    current_price TEXT,
    shares        TEXT,
    cost_usd      TEXT,
    entered_at_ms INTEGER,
    payload_json  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_positions_account ON positions (account_id);
CREATE TABLE IF NOT EXISTS trades (
    id            TEXT PRIMARY KEY,
    account_id    TEXT NOT NULL DEFAULT 'default',
    strategy      TEXT,
    asset         TEXT,
    direction     TEXT,
    entry_price   TEXT,
    exit_price    TEXT,
    shares        TEXT,
    cost_usd      TEXT,
    fees_usd      TEXT,
    gross_pnl_usd TEXT,
    net_pnl_usd   TEXT,
    payload_json  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_trades_account ON trades (account_id);
CREATE TABLE IF NOT EXISTS ledger_entries (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL,
    kind         TEXT NOT NULL,
    amount       TEXT NOT NULL,
    ts_ms        INTEGER,
    payload_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ledger_account ON ledger_entries (account_id);
CREATE TABLE IF NOT EXISTS audit_log (
    id     INTEGER PRIMARY KEY,
    ts     TEXT NOT NULL,
    event  TEXT NOT NULL,
    detail TEXT
);
CREATE TABLE IF NOT EXISTS kernel_meta (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT
);
CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);
";

/// What one import run folded in. Counts are rows actually INSERTED, so the
/// second (idempotent) run reports all zeros.
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

/// The cockpit risk family (`BK_RISK_*`, COCKPIT_KEYS minus the exit family)
/// — the `account_risk` row set, global value with a per-account `_SFX`
/// spelling on top.
const RISK_KEYS: &[&str] = &[
    "MAX_SINGLE_LOSS_USD",
    "MAX_DAILY_DRAWDOWN_USD",
    "MAX_POSITION_SIZE",
    "MAX_CONSECUTIVE_LOSSES",
    "COOLDOWN_MINUTES",
    "MAX_ORDER_NOTIONAL_USD",
];

/// The cockpit exit family (`BK_EXIT_*`) — folded per account into one
/// `policy_json` object.
const EXIT_KEYS: &[&str] = &[
    "STOP_LOSS_PCT",
    "TAKE_PROFIT_PCT",
    "FORCE_EXIT_SEC",
    "TRAILING_ENABLED",
    "STRATEGY_EXIT_MAKER_FIRST",
];

/// The `account_settings` key set: the execution-policy family (`POLICY_*`
/// wins over the bare alias), the pre-#751 bare names (`BUDGET_RATIO`,
/// `MAX_BUDGET_USD`), and the launcher's sizing / daily-loss / notional caps.
const SETTING_KEYS: &[&str] = &[
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

/// Import the legacy world into `db_path`, exactly once.
///
/// `env_lookup` is the `.env` read face (the kernel passes
/// `|k| std::env::var(k).ok()`); every account / risk / exit / policy knob
/// is read through it. Returns what was folded in — the second run sees the
/// `import_v1` marker and returns the all-zero report.
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
    let now = utc_now();
    let mut report = ImportReport::default();
    let mut skipped: Vec<String> = Vec::new();

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
    report.orders = fold_orders(&tx, &data_dir, &mut skipped)?;
    report.positions = fold_positions(&tx, &data_dir, &mut skipped)?;
    report.trades = fold_trades(&tx, &data_dir, &mut skipped)?;
    report.ledger_entries = fold_ledgers(&tx, &data_dir, &accounts, &mut skipped)?;
    import_state_files(&tx, &data_dir, &now, &mut skipped)?;
    tx.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (1, ?1)",
        params![now],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO kernel_meta (key, value, updated_at) \
         VALUES ('import_v1', '1', ?1)",
        params![now],
    )?;
    let detail = format!(
        "accounts={} risk_rows={} exit_rows={} settings={} orders={} positions={} \
         trades={} ledger_entries={} skipped={}",
        report.accounts,
        report.risk_rows,
        report.exit_rows,
        report.settings,
        report.orders,
        report.positions,
        report.trades,
        report.ledger_entries,
        skipped.len(),
    );
    report.audit = tx.execute(
        "INSERT INTO audit_log (ts, event, detail) VALUES (?1, 'db_import_v1', ?2)",
        params![now, detail],
    )?;
    tx.commit()?;
    report.skipped = skipped;
    Ok(report)
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
/// `ACCOUNT_<SFX>_*`, defaulting to the id / '' / active / 1.
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
        "INSERT OR IGNORE INTO accounts (id, name, venue, status, trading_enabled) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![account_id, name, venue, status, trading_enabled],
    )?)
}

/// One `account_risk` map per account: every global `BK_RISK_*` value, with
/// the `BK_RISK_*_<SFX>` spelling on top (per-account value wins).
fn insert_account_risk(
    tx: &Connection,
    account_id: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> anyhow::Result<usize> {
    let sfx = env_account_suffix(account_id);
    let mut rows: BTreeMap<&str, String> = BTreeMap::new();
    for key in RISK_KEYS {
        let value = env(&format!("BK_RISK_{key}_{sfx}")).or_else(|| env(&format!("BK_RISK_{key}")));
        if let Some(value) = value {
            rows.insert(key, value);
        }
    }
    let mut inserted = 0;
    for (key, value) in &rows {
        inserted += tx.execute(
            "INSERT OR IGNORE INTO account_risk (account_id, key, value) VALUES (?1, ?2, ?3)",
            params![account_id, key, value],
        )?;
    }
    Ok(inserted)
}

/// One `account_exit_policy` row per account (skipped when the env carries no
/// `BK_EXIT_*` value for it): the global exit knobs with the `_SFX` spelling
/// on top, rendered as one `policy_json` object.
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
            policy.insert((*key).to_string(), Value::String(value));
        }
    }
    if policy.is_empty() {
        return Ok(0);
    }
    let policy_json = serde_json::json!(policy).to_string();
    Ok(tx.execute(
        "INSERT OR IGNORE INTO account_exit_policy (account_id, policy_json) VALUES (?1, ?2)",
        params![account_id, policy_json],
    )?)
}

/// The `account_settings` rows for every account: `POLICY_*` beats the bare
/// alias for every key that has both (the execution policy's env rule).
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
            "INSERT OR IGNORE INTO account_settings (account_id, key, value) VALUES (?1, ?2, ?3)",
            params![account_id, key, value],
        )?;
    }
    Ok(inserted)
}

/// `data/orders/orders.jsonl` → `orders`: the log is append-only CURRENT
/// snapshots, so fold to the LATEST line per order id (later lines win — the
/// `OrderDb::load` rule) and insert one row per id.
fn fold_orders(
    tx: &Connection,
    data_dir: &Path,
    skipped: &mut Vec<String>,
) -> anyhow::Result<usize> {
    const REL: &str = "orders/orders.jsonl";
    let path = data_dir.join(REL);
    if !path.exists() {
        skipped.push(REL.to_string());
        return Ok(0);
    }
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
            "INSERT OR IGNORE INTO orders \
             (id, account_id, strategy, asset, direction, side, status, \
              price, size, filled_size, payload_json, submitted_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id,
                account_of(row),
                str_field(row, &["strategy"]),
                str_field(row, &["asset"]),
                str_field(row, &["direction"]),
                str_field(row, &["side"]),
                str_field(row, &["status"]),
                str_field(row, &["price"]),
                str_field(row, &["size"]),
                str_field(row, &["filledSize", "filled_size"]),
                payload,
                i64_field(row, &["submittedAtMs", "submitted_at_ms"]),
                i64_field(row, &["updatedAtMs", "updated_at_ms"]),
            ],
        )?;
    }
    Ok(inserted)
}

/// `data/positions/positions.dry.jsonl` then `positions.jsonl` →
/// `positions`: the dry snapshot loads first, the live one overwrites — same
/// id, live wins.
fn fold_positions(
    tx: &Connection,
    data_dir: &Path,
    skipped: &mut Vec<String>,
) -> anyhow::Result<usize> {
    let mut folded: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for rel in ["positions/positions.dry.jsonl", "positions/positions.jsonl"] {
        let path = data_dir.join(rel);
        if !path.exists() {
            skipped.push(rel.to_string());
            continue;
        }
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
            "INSERT OR IGNORE INTO positions \
             (id, account_id, strategy, asset, direction, entry_price, current_price, \
              shares, cost_usd, entered_at_ms, payload_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                account_of(row),
                str_field(row, &["strategy"]),
                str_field(row, &["asset"]),
                str_field(row, &["direction"]),
                str_field(row, &["entryPrice", "entry_price"]),
                str_field(row, &["currentPrice", "current_price"]),
                str_field(row, &["shares"]),
                str_field(row, &["costUsd", "cost_usd"]),
                i64_field(row, &["enteredAtMs", "entered_at_ms"]),
                payload,
            ],
        )?;
    }
    Ok(inserted)
}

/// `data/trades/trades.jsonl` then `trades-live.jsonl` → `trades`: dry log
/// first, live log overwrites — same id, live wins.
fn fold_trades(
    tx: &Connection,
    data_dir: &Path,
    skipped: &mut Vec<String>,
) -> anyhow::Result<usize> {
    let mut folded: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for rel in ["trades/trades.jsonl", "trades/trades-live.jsonl"] {
        let path = data_dir.join(rel);
        if !path.exists() {
            skipped.push(rel.to_string());
            continue;
        }
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
            "INSERT OR IGNORE INTO trades \
             (id, account_id, strategy, asset, direction, entry_price, exit_price, shares, \
              cost_usd, fees_usd, gross_pnl_usd, net_pnl_usd, payload_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id,
                account_of(row),
                str_field(row, &["strategy"]),
                str_field(row, &["asset"]),
                str_field(row, &["direction"]),
                str_field(row, &["entryPrice", "entry_price"]),
                str_field(row, &["exitPrice", "exit_price"]),
                str_field(row, &["shares"]),
                str_field(row, &["costUsd", "cost_usd"]),
                str_field(row, &["feesUsd", "fees_usd"]),
                str_field(row, &["grossPnlUsd", "gross_pnl_usd"]),
                str_field(row, &["netPnlUsd", "net_pnl_usd"]),
                payload,
            ],
        )?;
    }
    Ok(inserted)
}

/// `data/ledger/<account>.journal.dry.jsonl` / `.live.jsonl` → `ledger_entries`:
/// one row per journal line, keyed `<account>:<mode>:<seq>`; `kind` / `amount`
/// are read from the row and the amount stays the TEXT it was serialized as.
fn fold_ledgers(
    tx: &Connection,
    data_dir: &Path,
    accounts: &[String],
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
            for (seq, (line, row)) in read_jsonl(&path).into_iter().enumerate() {
                let kind = str_field(&row, &["kind"]).unwrap_or_default();
                let amount = str_field(&row, &["amount"]).unwrap_or_default();
                let at_ms = i64_field(&row, &["atMs", "at_ms"]);
                inserted += tx.execute(
                    "INSERT OR IGNORE INTO ledger_entries \
                     (id, account_id, kind, amount, ts_ms, payload_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        format!("{account}:{mode}:{}", seq + 1),
                        account,
                        kind,
                        amount,
                        at_ms,
                        line,
                    ],
                )?;
            }
        }
    }
    Ok(inserted)
}

/// The state snapshots → `kernel_meta` (raw file contents, no re-encoding):
/// `data/state.json` → `legacy_state_json`, `data/runtime/config.json` →
/// `runtime_config_json`, `data/update/state.json` → `update_state_json`. A
/// missing file is skipped-and-noted, never fatal.
fn import_state_files(
    tx: &Connection,
    data_dir: &Path,
    now: &str,
    skipped: &mut Vec<String>,
) -> anyhow::Result<()> {
    for (rel, key) in [
        ("state.json", "legacy_state_json"),
        ("runtime/config.json", "runtime_config_json"),
        ("update/state.json", "update_state_json"),
    ] {
        let path = data_dir.join(rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            skipped.push(rel.to_string());
            continue;
        };
        tx.execute(
            "INSERT OR IGNORE INTO kernel_meta (key, value, updated_at) VALUES (?1, ?2, ?3)",
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

/// The account a legacy row settled in — absent `account_id` reads as the
/// `default` book (the serde default the live loaders use).
fn account_of(row: &Value) -> String {
    str_field(row, &["account_id", "accountId"]).unwrap_or_else(|| DEFAULT_ACCOUNT.to_string())
}

/// RFC 3339 UTC timestamp, no date dependency: civil-from-days over
/// `SystemTime`.
fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let (h, m, s) = (secs % 86_400 / 3_600, secs % 3_600 / 60, secs % 60);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// (year, month, day) for days since 1970-01-01 — Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = yoe as i64 + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
#[path = "db_import_tests.rs"]
mod tests;
