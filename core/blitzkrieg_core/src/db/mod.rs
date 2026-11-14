//! SQLite-backed persistence for BlitzkriegBot (issue #1189).
//!
//! Account configuration, credentials, risk, exit discipline, runtime state,
//! markets and audit live in ONE database file (`data/blitzkrieg.sqlite` by
//! default). The migration is what retires `config.toml` and the `data/*.json`
//! / `*.jsonl` family as configuration and state sources; `.env` keeps only
//! the truly global static settings, the panel password and the credential
//! master key.
//!
//! Orders, fills, positions, trades and the cash ledger are NOT here: since
//! v2 (#1199 PR-0.5 T-02) those five tables are dropped, because the runtime
//! truth for all of them is the `data/*.jsonl` files the kernel's JSONL stores
//! own (`order_db` / `trade_db` / `position_db` / `ledger`) — the tables only
//! ever held one-shot importer rows and made the schema advertise storage
//! nothing reads.
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
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;

/// Default database path, resolved against the process working directory.
///
/// This spelling is the RELATIVE half of the resolution rule
/// ([`resolve_db_path`] / issue #1199 R2): an explicit relative
/// `BLITZKRIEG_DB_PATH` is honoured verbatim (the operator's cwd-relative
/// spelling is their choice), and the DEFAULT is anchored to the install root
/// so the same binary resolves the same database from any cwd. The constant
/// itself stays cwd-relative — it is the joinable tail, not the location.
pub const DEFAULT_DB_PATH: &str = "data/blitzkrieg.sqlite";

/// The environment variable that names an EXPLICIT database path
/// (issue #1189). Set by the operator to point the kernel at a store other
/// than the shipped default; read through [`default_db_path`] so the launcher
/// (legacy import) and the session (store open) can never resolve two
/// different files (issue #1199: the split-brain root — the launcher imported
/// into the default path while the session only opened an explicit one).
pub const DB_PATH_ENV: &str = "BLITZKRIEG_DB_PATH";

/// Resolve the database path this process runs: the explicit
/// `BLITZKRIEG_DB_PATH` when it is set and non-blank, else the shipped
/// [`DEFAULT_DB_PATH`] anchored to the install root (the executable's
/// directory and its ancestors — the #380 `execution_policy` convention,
/// reused verbatim: a release bundle's `bin/`, a checkout's `target/release/`
/// walking up to the repo root). The ONE resolver — the launcher's one-shot
/// import and the session's store open both call it, so the file the import
/// fills is the file the kernel reads.
pub fn default_db_path() -> std::path::PathBuf {
    resolve_db_path(std::env::var(DB_PATH_ENV).ok().as_deref())
}

/// The pure resolution rule (the test seam — mutating the process environment
/// to test precedence would be global state parallel tests race on).
///
/// * explicit non-blank `BLITZKRIEG_DB_PATH` → honoured VERBATIM (a relative
///   spelling stays cwd-relative: an explicit path is a promise about which
///   file to use, not a default to second-guess);
/// * blank/absent → [`DEFAULT_DB_PATH`] joined onto the install root: the
///   executable's own directory first, then its ancestors (the same walk
///   [`crate::execution_policy::resolve_default_config_path`] runs — exe
///   chain first, cwd second). The exe anchor is what makes the answer
///   cwd-independent: `bin/blitzkrieg-core` in a bundle and
///   `target/release/blitzkrieg-core` in a checkout both resolve to the
///   deployment root's `data/blitzkrieg.sqlite` no matter where the process
///   was started from (issue #1199 R2: a cwd-relative default silently
///   created a NEW empty database in whatever directory the operator
///   happened to launch from — the panel then read and wrote the empty file
///   while the real data sat in the deployment's database).
fn resolve_db_path(explicit: Option<&str>) -> std::path::PathBuf {
    match explicit.map(str::trim).filter(|v| !v.is_empty()) {
        Some(path) => std::path::PathBuf::from(path),
        None => install_root()
            .map(|root| root.join(DEFAULT_DB_PATH))
            .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_DB_PATH)),
    }
}

/// The install root: the first directory at-or-above the EXECUTABLE that
/// looks like a deployment root, else the first at-or-above the CWD.
///
/// The probe marker is the repo's own layout — a directory containing
/// `user_layer/` (the #380 install-root convention's marker, same as
/// [`crate::execution_policy`]'s lookup). Exe ancestors are probed FIRST so a
/// bundle/checkout root wins over an unrelated directory the process happens
/// to run from; the cwd chain is the dev-convenience second (a core run from
/// an unusual binary location but the repo root still finds the shipped
/// layout). `None` inputs (current_exe/current_dir failed) simply shorten
/// their chain — a probe that cannot name a directory is skipped, not an
/// error; no hit at all falls back to the exe's own directory (absolute
/// whenever the exe path is), never to the bare cwd.
fn install_root() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let cwd = std::env::current_dir().ok();
    for dir in exe_dir
        .iter()
        .flat_map(|d| ancestor_chain(d))
        .chain(cwd.iter().flat_map(|d| ancestor_chain(d)))
    {
        if dir.join("user_layer").is_dir() {
            return Some(dir);
        }
    }
    exe_dir
}

/// Walk `start` upward through its ancestors, `start` included (the #380
/// chain shape: for a release bundle the exe sits at `<bundle>/bin/` and the
/// layout lands at `<bundle>/user_layer/`, so the walk from the exe's own
/// directory already covers the dev-checkout shape).
fn ancestor_chain(start: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = Some(start);
    while let Some(dir) = cur {
        out.push(dir.to_path_buf());
        cur = dir.parent();
    }
    out
}

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
/// Cloning dups the handle (same file, same single-writer discipline); there
/// is still exactly one store on disk.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
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
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    /// Open the shipped default path — the install-root-resolved spelling of
    /// [`DEFAULT_DB_PATH`] ([`default_db_path`]).
    pub fn open_default() -> anyhow::Result<Self> {
        Self::open(default_db_path())
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

    /// Whether this database carries no data at all (issue #1199 R2): zero
    /// `account_risk` rows and no `import_v1` marker — the shape a freshly
    /// created (or never-written) file has, and the fact the boot path WARNs
    /// about so "the panel reads/writes an empty DB" is never silent.
    pub fn is_empty(&self) -> anyhow::Result<bool> {
        let guard = self.lock();
        is_empty_database(&guard)
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

/// Whether this database carries NO data: zero `account_risk` rows AND no
/// `import_v1` done-marker (issue #1199 R2, the empty-database probe).
///
/// The two halves answer two different questions, and BOTH must be absent for
/// "empty": the marker says "the one-shot legacy import already folded this
/// database" (an imported-but-empty database is a valid operator state — the
/// env simply had nothing), while the row count says "the panel/risk write
/// path has ever landed anything". Either one present means the file is NOT
/// the silently-new empty database the boot WARN is about.
pub fn is_empty_database(conn: &Connection) -> anyhow::Result<bool> {
    let risk_rows: i64 = conn.query_row("SELECT COUNT(*) FROM account_risk", [], |r| r.get(0))?;
    if risk_rows > 0 {
        return Ok(false);
    }
    Ok(kernel_meta_get(conn, META_IMPORT_V1)?.is_none())
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

// ── account_settings ────────────────────────────────────────────────────────
// The per-account EAV family (issue #1189 §2): execution policy, sizing
// ratios, daily loss, round anchor, hot caps — money and ratios as decimal
// strings, JSON values as JSON text. The risk matrix's order-notional cap
// (`risk.max_order_notional_usd`) already lives here; the account book's
// runtime knobs land here too, so one table backs the whole settings family.

/// Read one `account_settings` value. `None` = the key was never written for
/// this account (the caller's fallback applies).
pub fn account_setting_get(
    conn: &Connection,
    account_id: &str,
    key: &str,
) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM account_settings WHERE account_id = ?1 AND key = ?2",
            rusqlite::params![account_id, key],
            |r| r.get(0),
        )
        .ok())
}

/// Write one `account_settings` value (upsert).
///
/// The account row is ensured first (INSERT OR IGNORE, the same shape
/// `AccountRiskDb` uses): a settings write may arrive before the book is
/// mirrored into the DB (tests, embedders), and the foreign key must be
/// satisfied without inventing a second account source.
pub fn account_setting_set(
    conn: &Connection,
    account_id: &str,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO accounts
                (id, name, market_type, status, trading_enabled, updated_at_ms)
             VALUES (?1, ?1, 'prediction', 'active', 1, ?2)",
        rusqlite::params![account_id, now_ms()],
    )?;
    conn.execute(
        "INSERT INTO account_settings (account_id, key, value, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, key) DO UPDATE SET
            value = excluded.value, updated_at_ms = excluded.updated_at_ms",
        rusqlite::params![account_id, key, value, now_ms()],
    )?;
    Ok(())
}

/// Remove one `account_settings` key. Ok even when the key was absent (the
/// remove face's "absence is the value" spelling — an idempotent sweep).
pub fn account_setting_remove(
    conn: &Connection,
    account_id: &str,
    key: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM account_settings WHERE account_id = ?1 AND key = ?2",
        rusqlite::params![account_id, key],
    )?;
    Ok(())
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

    /// issue #1199 R2: the path resolution rule — an explicit non-blank
    /// `BLITZKRIEG_DB_PATH` wins (verbatim, relative spellings included —
    /// the operator's choice), blank/absent falls to the install-root-anchored
    /// shipped default. The pure seam (no process env mutation — parallel
    /// tests race on it).
    #[test]
    fn resolve_db_path_prefers_an_explicit_spelling() {
        assert_eq!(
            resolve_db_path(Some("/mnt/bank/other.sqlite")),
            std::path::PathBuf::from("/mnt/bank/other.sqlite"),
            "an explicit path wins verbatim"
        );
        assert_eq!(
            resolve_db_path(Some("  /tmp/spaced.sqlite  ")),
            std::path::PathBuf::from("/tmp/spaced.sqlite"),
            "the explicit path is trimmed"
        );
        // A relative explicit spelling stays relative: an explicit path is a
        // promise about which file to use, not a default to second-guess.
        assert_eq!(
            resolve_db_path(Some("var/bank.sqlite")),
            std::path::PathBuf::from("var/bank.sqlite"),
            "an explicit relative path is honoured as the operator spelled it"
        );
        // The default branch: whatever install root this test binary can
        // derive, the answer is that root joined with DEFAULT_DB_PATH — never
        // the bare relative spelling (issue #1199 R2: a bare relative default
        // resolved against the LAUNCH cwd, so the same binary started from a
        // different directory silently opened a NEW empty database).
        let resolved = resolve_db_path(None);
        assert!(
            resolved.is_absolute(),
            "the default resolves to an absolute path (install-root anchored): {}",
            resolved.display()
        );
        assert!(
            resolved.ends_with(DEFAULT_DB_PATH),
            "the default ends with the shipped spelling: {}",
            resolved.display()
        );
        for blank in [Some(""), Some("   ")] {
            assert_eq!(
                resolve_db_path(blank),
                resolved,
                "blank falls to the same default: {blank:?}"
            );
        }
    }

    /// issue #1199 R2: the same binary resolves the SAME database from any
    /// cwd. The install-root walk probes the exe chain first, so two
    /// invocations with different cwds (the split-brain scenario — the panel
    /// opened one file, the import filled another) cannot land on two
    /// different default databases.
    #[test]
    fn default_db_path_is_the_same_file_from_any_cwd() {
        // The pure half: the install-root anchor derivation is a function of
        // the exe location only (the cwd chain is the second probe, and it
        // only matters when the exe has no `user_layer/` ancestor at all).
        // Build a scratch tree shaped like a deployment root, "run" the
        // resolution from two different cwds against it, and require the
        // same absolute answer both times.
        let root = std::env::temp_dir().join(format!(
            "bk-dbpath-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("user_layer")).unwrap();
        let exe_dir = root.join("bin");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let other_cwd = std::env::temp_dir().join(format!(
            "bk-dbpath-other-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&other_cwd).unwrap();

        let from_a = resolve_default_with_anchors(Some(&exe_dir), Some(&exe_dir));
        let from_b = resolve_default_with_anchors(Some(&exe_dir), Some(&other_cwd));
        assert_eq!(
            from_a, from_b,
            "the same binary from two different cwds resolves one database"
        );
        assert_eq!(from_a, root.join(DEFAULT_DB_PATH));
        assert!(from_a.is_absolute());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other_cwd);
    }

    /// The exe-anchored default resolution with the anchor chain INJECTED
    /// (the test seam for [`install_root`]'s walk: the process's own exe/cwd
    /// are global state parallel tests must not depend on).
    fn resolve_default_with_anchors(exe_dir: Option<&Path>, cwd: Option<&Path>) -> PathBuf {
        let mut exe_chain: Vec<PathBuf> = Vec::new();
        if let Some(d) = exe_dir {
            let mut cur = Some(d);
            while let Some(dir) = cur {
                exe_chain.push(dir.to_path_buf());
                cur = dir.parent();
            }
        }
        let mut cwd_chain: Vec<PathBuf> = Vec::new();
        if let Some(d) = cwd {
            let mut cur = Some(d);
            while let Some(dir) = cur {
                cwd_chain.push(dir.to_path_buf());
                cur = dir.parent();
            }
        }
        for dir in exe_chain.iter().chain(cwd_chain.iter()) {
            if dir.join("user_layer").is_dir() {
                return dir.join(DEFAULT_DB_PATH);
            }
        }
        exe_chain
            .first()
            .map(|d| d.join(DEFAULT_DB_PATH))
            .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_DB_PATH))
    }

    /// The empty-database probe (issue #1199 R2): a fresh database is empty;
    /// a risk row OR the import marker alone makes it non-empty (the marker
    /// is the "the importer already folded this database" fact — an
    /// imported-but-empty database is a valid operator state, not the
    /// silently-new file the boot WARN is about).
    #[test]
    fn empty_database_probe_flips_on_rows_or_marker() {
        let db = TempDb::new();
        assert!(
            db.db().is_empty().unwrap(),
            "a fresh database is empty (no rows, no marker)"
        );
        // A risk row alone makes it non-empty.
        db.connection()
            .execute(
                "INSERT INTO accounts (id, name, market_type, status, trading_enabled, updated_at_ms)
                 VALUES ('a1', 'A', 'prediction', 'active', 1, 0)",
                [],
            )
            .unwrap();
        db.connection()
            .execute(
                "INSERT INTO account_risk (account_id, max_single_loss_usd,
                     max_daily_drawdown_usd, max_position_size, max_consecutive_losses,
                     cooldown_minutes, source, updated_at_ms)
                 VALUES ('a1', '0', '0', '0', '0', '0', 'default', 0)",
                [],
            )
            .unwrap();
        assert!(!db.db().is_empty().unwrap(), "one account_risk row is data");
        // The marker alone (a second, marker-only database) also makes it
        // non-empty: the import ran.
        let dir = std::env::temp_dir().join(format!(
            "bk-dbpath-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db2 = Database::open(dir.join("m.sqlite")).unwrap();
        kernel_meta_set(&db2.lock(), META_IMPORT_V1, "1").unwrap();
        assert!(
            !db2.is_empty().unwrap(),
            "the import marker alone means the database is known"
        );
        drop(db2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `account_settings` EAV round-trip: set, get, upsert, remove, and
    /// the account-row ensure (a settings write for an unknown id creates
    /// the account row — the FK is satisfied without a second source).
    #[test]
    fn account_settings_round_trip_and_upsert() {
        let db = TempDb::new();
        let conn = db.connection();
        assert_eq!(
            account_setting_get(&conn, "paper", "risk.cap").unwrap(),
            None
        );
        account_setting_set(&conn, "paper", "risk.cap", "25").unwrap();
        assert_eq!(
            account_setting_get(&conn, "paper", "risk.cap")
                .unwrap()
                .as_deref(),
            Some("25")
        );
        // Upsert, not duplicate.
        account_setting_set(&conn, "paper", "risk.cap", "30").unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM account_settings WHERE account_id = 'paper' AND key = 'risk.cap'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
        // The ensure: the FK account row exists exactly once.
        let accounts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE id = 'paper'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(accounts, 1);
        // Remove, then a second remove is Ok (idempotent sweep).
        account_setting_remove(&conn, "paper", "risk.cap").unwrap();
        account_setting_remove(&conn, "paper", "risk.cap").unwrap();
        assert_eq!(
            account_setting_get(&conn, "paper", "risk.cap").unwrap(),
            None
        );
        // Sibling isolation: the same key on another account is a different row.
        account_setting_set(&conn, "paper", "risk.cap", "1").unwrap();
        account_setting_set(&conn, "live", "risk.cap", "2").unwrap();
        assert_eq!(
            account_setting_get(&conn, "paper", "risk.cap")
                .unwrap()
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            account_setting_get(&conn, "live", "risk.cap")
                .unwrap()
                .as_deref(),
            Some("2")
        );
    }

    /// `default_db_path` reads the process env — with `BLITZKRIEG_DB_PATH`
    /// unset in the test harness it resolves to the install-root-anchored
    /// shipped default (an absolute path ending in the shipped spelling; the
    /// anchor is this test binary's own exe location, the same walk the
    /// production binary runs).
    #[test]
    fn default_db_path_reads_the_environment() {
        // Guarded: a developer exporting BLITZKRIEG_DB_PATH must not flake
        // the suite — skip the assertion when the env speaks.
        if std::env::var(DB_PATH_ENV).is_ok() {
            return;
        }
        let resolved = default_db_path();
        assert!(
            resolved.is_absolute(),
            "the default resolves to an absolute path: {}",
            resolved.display()
        );
        assert!(
            resolved.ends_with(DEFAULT_DB_PATH),
            "the default ends with the shipped spelling: {}",
            resolved.display()
        );
    }
}
