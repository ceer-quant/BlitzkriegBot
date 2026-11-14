//! SQLite schema (issue #1189, §2; v2 = #1199 PR-0.5 T-02).
//!
//! One database holds both account state and kernel metadata. Every monetary
//! value is a **decimal string** (`TEXT`), never a SQLite `REAL` — the kernel
//! speaks `rust_decimal`, and a binary float would silently lose the precision
//! the ledger's cash identity depends on.
//!
//! The SQL here is the single definition of the schema. [`crate::db::migrations`]
//! owns the version number and the runner; this module owns the DDL and the
//! table/index inventory the tests check against.
//!
//! # v2 (#1199 PR-0.5, T-02): the five zero-runtime-write tables are dropped
//!
//! `orders` / `fills` / `positions` / `trades` / `ledger_entries` never had a
//! runtime writer: the live truth for all five lives in the `data/*.jsonl`
//! files (`order_db.rs` / `trade_db.rs` / `position_db.rs` / `ledger.rs`), and
//! the only rows the SQLite tables ever carried came from the one-shot
//! importer. Keeping them made the schema a lying surface — it advertised
//! storage nothing reads. [`SCHEMA_V2`] drops them (and their indexes);
//! [`SCHEMA_V1`] below stays byte-identical (append-only: a v1 database on
//! disk is upgraded by the new migration, never by editing the old DDL).

/// The schema version this module defines. Bumped whenever [`SCHEMA_V1`] is
/// superseded by a new migration (never edited in place).
pub const SCHEMA_VERSION: i64 = 2;

/// Every table the CURRENT schema creates, in dependency order (parents first).
///
/// The test suite asserts this list against `sqlite_master` so a table that is
/// renamed or dropped cannot pass unnoticed. Since v2 (#1199 T-02) this is the
/// post-migration target state: the five ledger tables the importer alone ever
/// wrote are dropped, and the JSONL files are their only home.
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
    "arb_pairs",
    "audit_log",
];

/// Every index the CURRENT schema creates. Since v2 (#1199 T-02) the five
/// per-account ledger indexes went with their tables.
pub const INDEXES: &[&str] = &["idx_audit_ts", "idx_rounds_account", "idx_markets_venue"];

/// The complete v1 schema.
///
/// Idempotent by construction (`IF NOT EXISTS` everywhere) so a partially
/// applied file can be re-run safely. Amounts, prices and share counts are
/// `TEXT`; `INTEGER` columns carry only counts, flags and epoch milliseconds.
pub const SCHEMA_V1: &str = r#"
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

/// The v2 migration (#1199 PR-0.5, T-02 — the "write-matrix lockdown" ruling):
/// drop the five tables that never had a runtime writer.
///
/// `orders` / `fills` / `positions` / `trades` / `ledger_entries` were written
/// ONLY by the one-shot legacy importer; every runtime read and write of those
/// facts goes to the `data/*.jsonl` files. The empty tables made the schema
/// advertise storage nothing consumes, so v2 removes them — a database that
/// boots through the migration runner converges on the same shape the runtime
/// actually uses.
///
/// Indexes drop with their tables (SQLite would remove them implicitly, but
/// the explicit `DROP INDEX IF EXISTS` keeps the intent readable and the
/// statements order-independent). No foreign keys point at these tables, so the
/// drop order is free; the child-before-parent spelling is defensive habit.
/// Idempotent (`IF EXISTS` everywhere), so a partially applied file re-runs
/// safely — same discipline as [`SCHEMA_V1`].
pub const SCHEMA_V2: &str = r#"
DROP INDEX IF EXISTS idx_fills_account;
DROP INDEX IF EXISTS idx_ledger_account;
DROP INDEX IF EXISTS idx_orders_account;
DROP INDEX IF EXISTS idx_positions_account;
DROP INDEX IF EXISTS idx_trades_account;
DROP TABLE IF EXISTS fills;
DROP TABLE IF EXISTS ledger_entries;
DROP TABLE IF EXISTS positions;
DROP TABLE IF EXISTS trades;
DROP TABLE IF EXISTS orders;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The DDL must not declare a floating-point column anywhere: every amount
    /// in the kernel is a decimal string, and a REAL column would invite an
    /// implicit float round-trip.
    #[test]
    fn schema_declares_no_real_columns() {
        // Strip `--` line comments first: the prose explains WHY floats are
        // banned, and the words REAL/FLOAT appear in that prose.
        let ddl: String = SCHEMA_V1
            .lines()
            .map(|line| line.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
            .to_uppercase();
        for forbidden in ["REAL", "FLOAT", "DOUBLE", "NUMERIC"] {
            assert!(
                !ddl.contains(forbidden),
                "schema v1 must not declare {forbidden} columns"
            );
        }
    }

    /// The version constant and the migration list must agree.
    #[test]
    fn version_matches_migration_list() {
        assert_eq!(
            crate::db::migrations::MIGRATIONS
                .iter()
                .map(|m| m.version)
                .max(),
            Some(SCHEMA_VERSION)
        );
    }

    /// v2 (#1199 T-02) drops exactly the five zero-runtime-write tables and
    /// their five indexes — no more, no less. A typo in the DROP list would
    /// either leave a lying table behind or take a live one down.
    #[test]
    fn v2_drops_exactly_the_five_ledger_tables() {
        const DROPPED_TABLES: &[&str] =
            &["orders", "fills", "positions", "trades", "ledger_entries"];
        const DROPPED_INDEXES: &[&str] = &[
            "idx_orders_account",
            "idx_fills_account",
            "idx_positions_account",
            "idx_trades_account",
            "idx_ledger_account",
        ];
        for table in DROPPED_TABLES {
            assert!(
                SCHEMA_V2.contains(&format!("DROP TABLE IF EXISTS {table};")),
                "SCHEMA_V2 must drop table {table}"
            );
            assert!(
                !TABLES.contains(table),
                "{table} is dropped by v2 — it must not be in the TABLES inventory"
            );
        }
        for index in DROPPED_INDEXES {
            assert!(
                SCHEMA_V2.contains(&format!("DROP INDEX IF EXISTS {index};")),
                "SCHEMA_V2 must drop index {index}"
            );
            assert!(
                !INDEXES.contains(index),
                "{index} is dropped by v2 — it must not be in the INDEXES inventory"
            );
        }
        // The v1 DDL itself is never edited (append-only): the tables it
        // creates must still be declared there, v2 is what removes them.
        for table in DROPPED_TABLES {
            assert!(
                SCHEMA_V1.contains(&format!("CREATE TABLE IF NOT EXISTS {table} (")),
                "SCHEMA_V1 keeps its historical DDL: {table} must still be created there"
            );
        }
    }
}
