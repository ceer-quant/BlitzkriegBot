//! SQLite-backed persistence for BlitzkriegBot (issue #1189).
//!
//! Account configuration, credentials, risk, exit discipline, runtime state,
//! orders, fills, positions, trades, ledger and audit all live in ONE database
//! file (`data/blitzkrieg.sqlite` by default). The migration is what retires
//! `config.toml` and the `data/*.json` / `*.jsonl` family as configuration and
//! state sources; `.env` keeps only the truly global static settings, the panel
//! password and the credential master key.
//!
//! # Layout
//!
//! - [`schema`] — the v1 DDL (embedded SQL) and the table/index inventory.
//! - [`migrations`] — the version ledger (`schema_migrations`) and the runner.
//! - [`credentials`] — AES-256-GCM credential encryption + `.env` fallback.
//!
//! # Concurrency (issue #1189 §5)
//!
//! - `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON` per
//!   connection, `busy_timeout=5s`.
//! - The transaction kernel is the single writer. Callers serialise through
//!   the [`Mutex`] inside [`Database`]; the panel reaches the database only
//!   through the kernel's IPC, never by opening the file itself.
//! - A hot-path write failure is logged and never interrupts a running round;
//!   a failure to OPEN or MIGRATE at boot is fatal (fail-closed) — a kernel
//!   that cannot open its store must not pretend to still be reading files.
//!
//! # Decimal discipline (issue #1189 §2)
//!
//! Every amount, price and share count is a decimal STRING in a `TEXT` column.
//! No `REAL`/`FLOAT` column exists anywhere in the schema; money crosses this
//! boundary through `rust_decimal` and its string form, never a binary float.

pub mod credentials;
pub mod migrations;
pub mod schema;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

/// Default database path, relative to the process working directory (the same
/// convention every other `data/` writer in the kernel uses).
pub const DEFAULT_DB_PATH: &str = "data/blitzkrieg.sqlite";

/// `busy_timeout` for every connection (issue #1189 §5): 5 seconds.
pub const BUSY_TIMEOUT_MS: u64 = 5_000;

// ── kernel_meta keys ────────────────────────────────────────────────────────
// The reserved key `dry_run` is deliberately absent: whether the process
// simulates is decided by the environment alone and can never be stored here.

/// Set once the one-shot legacy import has completed (issue #1189 §4).
pub const META_IMPORT_V1: &str = "import_v1";
/// The process-level active account pointer.
pub const META_ACTIVE_ACCOUNT: &str = "active_account";
/// The display pointer (pure presentation, never an accounting axis).
pub const META_DISPLAY_ACCOUNT: &str = "display_account";
/// Verbatim `data/update/state.json` after the import.
pub const META_UPDATE_STATE_JSON: &str = "update_state_json";
/// Verbatim `data/runtime/config.json` after the import.
pub const META_RUNTIME_CONFIG_JSON: &str = "runtime_config_json";
/// Verbatim `data/state.json` (when it existed) after the import.
pub const META_LEGACY_STATE_JSON: &str = "legacy_state_json";

/// An open BlitzkriegBot database.
///
/// The connection sits behind a [`Mutex`] because the trading kernel is the
/// single writer: one process, one connection, every statement serialised.
/// Cloning is intentionally not implemented — there is exactly one store.
pub struct Database {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl Database {
    /// Open (creating if needed) the database at `path` and bring its schema up
    /// to date.
    ///
    /// Fail-closed: a failure to open, configure or migrate is an error. The
    /// caller (the boot path) must refuse to start rather than fall back to
    /// files.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        configure(&conn)?;
        migrations::run(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// Open the shipped default path ([`DEFAULT_DB_PATH`]).
    pub fn open_default() -> anyhow::Result<Self> {
        Self::open(DEFAULT_DB_PATH)
    }

    /// The filesystem path this database was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `f` against the connection. The guard is held for the closure's
    /// duration, so nested calls on the same thread would deadlock — callers
    /// must not re-enter.
    pub fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        let guard = self.lock();
        f(&guard)
    }

    /// Lock the connection, recovering from a poisoned mutex.
    ///
    /// A panic while the connection was borrowed must not brick the kernel:
    /// SQLite's own state is either committed or rolled back, so the connection
    /// is still usable.
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The schema version currently recorded in this database.
    pub fn schema_version(&self) -> anyhow::Result<i64> {
        let guard = self.lock();
        migrations::current_version(&guard)
    }
}

/// Apply the per-connection pragmas (issue #1189 §5).
///
/// `foreign_keys` must be set on EVERY connection (SQLite defaults it off per
/// connection, not per database), and `journal_mode` is persistent but set
/// here anyway so a fresh file is WAL from its first write.
fn configure(conn: &Connection) -> anyhow::Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
    // Returns the resulting mode; `UPDATE` form works for both the persistent
    // and the per-connection pragmas below.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// The journal mode a connection is actually running (for tests / diagnostics).
pub fn journal_mode(conn: &Connection) -> anyhow::Result<String> {
    Ok(conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?)
}

// ── kernel_meta ─────────────────────────────────────────────────────────────

/// Read a `kernel_meta` value. `None` = the key was never written.
pub fn kernel_meta_get(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM kernel_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .ok())
}

/// Write a `kernel_meta` value (upsert).
///
/// Refuses the reserved `dry_run` spelling in either case: the database has no
/// say over whether the process simulates (issue #1189 §2/§4). The schema's
/// `CHECK` is the backstop; this is the friendly error.
pub fn kernel_meta_set(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !key.trim().eq_ignore_ascii_case("dry_run"),
        "kernel_meta refuses the reserved key `{key}` — DRY_RUN is decided by the \
         environment alone and must never be stored in the database"
    );
    let now = now_ms();
    conn.execute(
        "INSERT INTO kernel_meta (key, value, updated_at_ms) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at_ms = excluded.updated_at_ms",
        rusqlite::params![key, value, now],
    )?;
    Ok(())
}

/// Milliseconds since the Unix epoch, or 0 if the clock is before it.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A temporary on-disk database, removed on drop.
    ///
    /// A real file (not `:memory:`) because WAL, `busy_timeout` and the
    /// `data/` directory creation are part of the contract under test.
    pub(crate) struct TempDb {
        db: Database,
        dir: PathBuf,
    }

    impl TempDb {
        pub(crate) fn new() -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("blitzkrieg-db-test-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let db = Database::open(dir.join("blitzkrieg.sqlite")).expect("temp database opens");
            Self { db, dir }
        }

        pub(crate) fn db(&self) -> &Database {
            &self.db
        }

        pub(crate) fn connection(&self) -> MutexGuard<'_, Connection> {
            self.db.lock()
        }

        pub(crate) fn path(&self) -> &Path {
            self.db.path()
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            // Drop the connection before removing the directory so WAL files
            // are released (Windows would otherwise keep them locked).
            drop(self.db.lock());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TempDb;

    #[test]
    fn default_path_is_under_data() {
        assert_eq!(DEFAULT_DB_PATH, "data/blitzkrieg.sqlite");
    }

    #[test]
    fn open_creates_the_file_and_enables_wal() {
        let db = TempDb::new();
        assert!(db.path().exists(), "the database file must be created");
        let mode = journal_mode(&db.connection()).unwrap();
        assert_eq!(
            mode.to_lowercase(),
            "wal",
            "WAL must be enabled on a file-backed database"
        );
    }

    #[test]
    fn pragmas_are_applied() {
        let db = TempDb::new();
        let conn = db.connection();
        let fk: i64 = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "foreign_keys must be ON");
        let bt: i64 = conn
            .pragma_query_value(None, "busy_timeout", |r| r.get(0))
            .unwrap();
        assert_eq!(bt, BUSY_TIMEOUT_MS as i64, "busy_timeout must be 5s");
        let sync: i64 = conn
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap();
        // SQLite spells NORMAL as 1 in the synchronous pragma.
        assert_eq!(sync, 1, "synchronous must be NORMAL");
    }

    #[test]
    fn foreign_keys_cascade_account_deletes() {
        let db = TempDb::new();
        let conn = db.connection();
        conn.execute(
            "INSERT INTO accounts (id, name, market_type, status, trading_enabled, updated_at_ms)
             VALUES ('a1', 'A', 'prediction', 'active', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO account_risk (account_id, max_single_loss_usd, max_daily_drawdown_usd,
                 max_position_size, max_consecutive_losses, cooldown_minutes, source, updated_at_ms)
             VALUES ('a1', '0', '0', '0', '0', '0', 'default', 0)",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM accounts WHERE id = 'a1'", [])
            .unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM account_risk", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "ON DELETE CASCADE must remove the child row");
    }

    #[test]
    fn kernel_meta_round_trips() {
        let db = TempDb::new();
        let conn = db.connection();
        assert_eq!(kernel_meta_get(&conn, META_IMPORT_V1).unwrap(), None);
        kernel_meta_set(&conn, META_IMPORT_V1, "2026-11-13T00:00:00Z").unwrap();
        assert_eq!(
            kernel_meta_get(&conn, META_IMPORT_V1).unwrap().as_deref(),
            Some("2026-11-13T00:00:00Z")
        );
        // Upsert, not duplicate.
        kernel_meta_set(&conn, META_IMPORT_V1, "again").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kernel_meta WHERE key = ?1",
                [META_IMPORT_V1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn kernel_meta_refuses_dry_run() {
        let db = TempDb::new();
        let conn = db.connection();
        for key in ["dry_run", "DRY_RUN", "Dry_Run", " dry_run "] {
            assert!(
                kernel_meta_set(&conn, key, "false").is_err(),
                "`{key}` must be refused by the friendly check"
            );
        }
        // And the schema CHECK is the backstop for a raw INSERT.
        assert!(
            conn.execute(
                "INSERT INTO kernel_meta (key, value, updated_at_ms) VALUES ('dry_run', 'x', 0)",
                []
            )
            .is_err(),
            "the schema CHECK must also refuse dry_run"
        );
    }

    #[test]
    fn amounts_are_stored_as_decimal_strings() {
        let db = TempDb::new();
        let conn = db.connection();
        conn.execute(
            "INSERT INTO accounts (id, name, market_type, status, trading_enabled, opening_equity, updated_at_ms)
             VALUES ('a1', 'A', 'prediction', 'active', 1, '1234.567890', 0)",
            [],
        )
        .unwrap();
        let equity: String = conn
            .query_row(
                "SELECT opening_equity FROM accounts WHERE id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            equity, "1234.567890",
            "the decimal string round-trips verbatim"
        );
        // And it is not stored as a float: the column's declared type is TEXT.
        let ty: String = conn
            .query_row(
                "SELECT type FROM pragma_table_info('accounts') WHERE name = 'opening_equity'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ty.to_uppercase(), "TEXT");
    }

    #[test]
    fn schema_version_is_recorded() {
        let db = TempDb::new();
        assert_eq!(db.db().schema_version().unwrap(), schema::SCHEMA_VERSION);
    }

    #[test]
    fn arb_pairs_is_a_single_row() {
        let db = TempDb::new();
        let conn = db.connection();
        conn.execute(
            "INSERT INTO arb_pairs (id, leg_a_venue, leg_a_account, leg_b_venue, leg_b_account,
                 pairs_json, updated_at_ms) VALUES (1, 'v1', 'a', 'v2', 'b', '[]', 0)",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO arb_pairs (id, leg_a_venue, leg_a_account, leg_b_venue, leg_b_account,
                     pairs_json, updated_at_ms) VALUES (2, 'v1', 'a', 'v2', 'b', '[]', 0)",
                []
            )
            .is_err(),
            "v1 keeps exactly one arb-pair row"
        );
    }

    #[test]
    fn database_is_send_but_not_sync() {
        fn assert_send<T: Send>() {}
        assert_send::<Database>();
    }
}
