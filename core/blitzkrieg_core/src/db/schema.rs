//! SQLite schema v1 (issue #1189, §2).
//!
//! One database holds both account state and kernel metadata. Every monetary
//! value is a **decimal string** (`TEXT`), never a SQLite `REAL` — the kernel
//! speaks `rust_decimal`, and a binary float would silently lose the precision
//! the ledger's cash identity depends on.
//!
//! The SQL here is the single definition of the schema. [`crate::db::migrations`]
//! owns the version number and the runner; this module owns the DDL and the
//! table/index inventory the tests check against.

/// The schema version this module defines. Bumped whenever [`SCHEMA_V1`] is
/// superseded by a new migration (never edited in place).
pub const SCHEMA_VERSION: i64 = 1;

/// Every table the v1 schema creates, in dependency order (parents first).
///
/// The test suite asserts this list against `sqlite_master` so a table that is
/// renamed or dropped cannot pass unnoticed.
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

/// Every index the v1 schema creates.
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
}
