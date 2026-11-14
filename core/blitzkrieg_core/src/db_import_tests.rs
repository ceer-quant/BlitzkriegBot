//! The #1189 importer's test module — the acceptance matrix: idempotency,
//! per-account isolation, no DRY_RUN leakage, the `.migrated`-free import
//! (the JSONL folds were removed with the five ledger tables, #1199 PR-0.5
//! T-02), the canonical-table spot checks, and the empty-tree import. Tmpdirs
//! follow the repo discipline (temp dir + pid + atomic counter; no `tempfile`
//! dependency).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

use super::*;

/// A process-unique tmpdir root, created on demand and removed on drop.
struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    fn new() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "bk-db-import-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create tmpdir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Open the database read-only and hand back a one-value query.
fn scalar_i64(db_path: &Path, sql: &str) -> i64 {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open db");
    conn.query_row(sql, [], |row| row.get(0)).expect("query")
}

fn scalar_text(db_path: &Path, sql: &str) -> String {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open db");
    conn.query_row(sql, [], |row| row.get(0)).expect("query")
}

/// Test 1 — idempotency: the second `import_once` inserts nothing.
#[test]
fn import_once_is_idempotent_second_run_is_all_zero() {
    let dir = TmpDir::new();
    let env = |key: &str| match key {
        "ACCOUNT_LIST" => Some("default".to_string()),
        "BK_RISK_MAX_POSITION_SIZE" => Some("100".to_string()),
        _ => None,
    };

    let first = import_once(&dir.path().join("bank.db"), env).expect("first import");
    assert_eq!(first.accounts, 1, "one account imported");
    assert_eq!(first.risk_rows, 1, "one risk row lands");
    assert_eq!(first.audit, 1, "one audit row");

    let second = import_once(&dir.path().join("bank.db"), env).expect("second import");
    assert_eq!(second.accounts, 0);
    assert_eq!(second.risk_rows, 0);
    assert_eq!(second.exit_rows, 0);
    assert_eq!(second.settings, 0);
    assert_eq!(second.audit, 0);
    assert_eq!(
        second.skipped,
        Vec::<String>::new(),
        "marker hit, no re-scan"
    );
}

/// Test 2 — per-account risk overrides never overwrite the sibling's row.
#[test]
fn per_account_risk_overrides_do_not_overwrite_siblings() {
    let dir = TmpDir::new();
    let env = |key: &str| match key {
        "ACCOUNT_LIST" => Some("alpha,beta".to_string()),
        "BK_RISK_MAX_POSITION_SIZE" => Some("100".to_string()),
        // `alpha` overrides the global; `beta` inherits it.
        "BK_RISK_MAX_POSITION_SIZE_ALPHA" => Some("7".to_string()),
        _ => None,
    };

    let report = import_once(&dir.path().join("bank.db"), env).expect("import");
    assert_eq!(report.accounts, 2);
    assert_eq!(report.risk_rows, 2, "one row per account, not per key");

    let db = dir.path().join("bank.db");
    let alpha = scalar_text(
        &db,
        "SELECT max_position_size FROM account_risk WHERE account_id = 'alpha'",
    );
    let beta = scalar_text(
        &db,
        "SELECT max_position_size FROM account_risk WHERE account_id = 'beta'",
    );
    assert_eq!(alpha, "7", "alpha carries the override");
    assert_eq!(beta, "100", "beta inherits the global value");
}

/// Test 3 — DRY_RUN never lands in kernel_meta.
#[test]
fn dry_run_env_key_is_never_persisted() {
    let dir = TmpDir::new();
    let env = |key: &str| match key {
        "DRY_RUN" => Some("true".to_string()),
        _ => None,
    };

    let report = import_once(&dir.path().join("bank.db"), env).expect("import");
    assert_eq!(report.accounts, 1, "default account still imports");

    let db = dir.path().join("bank.db");
    let count = scalar_i64(
        &db,
        "SELECT count(*) FROM kernel_meta WHERE key = 'DRY_RUN'",
    );
    assert_eq!(count, 0, "DRY_RUN is a mode flag, not a meta key");

    // The marker is the only thing that must exist.
    let keys: Vec<String> = {
        let conn = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open db");
        let mut stmt = conn
            .prepare("SELECT key FROM kernel_meta")
            .expect("prepare");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect")
    };
    assert!(keys.iter().any(|k| k == "import_v1"));
    assert!(
        !keys
            .iter()
            .any(|k| k.contains("DRY_RUN") || k.contains("dry_run"))
    );
}

/// Test 4 — #1199 PR-0.5 (T-02): the five zero-runtime-write tables are
/// gone, so the importer must NOT touch the ledger JSONL trees at all: no
/// rows in the five tables, no `.migrated` rename of the JSONL files (they
/// are the LIVE books the JSONL stores own), and the skipped report carries
/// no ledger-tree entries.
#[test]
fn importer_leaves_the_ledger_jsonl_trees_alone() {
    let dir = TmpDir::new();
    // A fully-populated legacy tree — exactly the fixture the old folds ate.
    for sub in ["orders", "positions", "trades", "ledger"] {
        std::fs::create_dir_all(dir.path().join(format!("data/{sub}"))).expect("mkdir");
    }
    std::fs::write(
        dir.path().join("data/orders/orders.jsonl"),
        "{\"orderId\":\"o1\",\"accountId\":\"default\",\"updatedAtMs\":100}\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("data/positions/positions.jsonl"),
        "{\"id\":\"p1\",\"accountId\":\"default\",\"open\":true}\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("data/trades/trades.jsonl"),
        "{\"id\":\"t1\",\"accountId\":\"default\",\"exitTime\":100}\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("data/ledger/default.journal.live.jsonl"),
        "{\"kind\":\"seed\",\"amount\":\"100\",\"atMs\":100}\n",
    )
    .expect("write");

    let db = dir.path().join("data/bank.db");
    let report = import_once(&db, |_| None).expect("import");

    // The five tables stay EMPTY (the tables themselves exist — the
    // importer's embedded v1 DDL still builds them; v2 of the migration
    // runner is what drops them on a migrated database).
    for (table, ddl_shape) in [
        ("orders", "SELECT count(*) FROM orders"),
        ("fills", "SELECT count(*) FROM fills"),
        ("positions", "SELECT count(*) FROM positions"),
        ("trades", "SELECT count(*) FROM trades"),
        ("ledger_entries", "SELECT count(*) FROM ledger_entries"),
    ] {
        let rows = scalar_i64(&db, ddl_shape);
        assert_eq!(
            rows, 0,
            "{table} must stay empty — no importer fold feeds it"
        );
    }

    // No rename: the JSONL books are the runtime truth and stay in place,
    // with no `.migrated` siblings appearing beside them.
    for rel in [
        "data/orders/orders.jsonl",
        "data/positions/positions.jsonl",
        "data/trades/trades.jsonl",
        "data/ledger/default.journal.live.jsonl",
    ] {
        assert!(dir.path().join(rel).exists(), "live book stays: {rel}");
        let migrated = format!("{rel}.migrated");
        assert!(
            !dir.path().join(&migrated).exists(),
            "no .migrated backup may appear: {migrated}"
        );
    }

    // The skipped report lists only the state snapshots (missing), never a
    // ledger-tree entry.
    for rel in [
        "orders/orders.jsonl",
        "positions/positions.jsonl",
        "ledger/default.journal.live.jsonl",
    ] {
        assert!(
            !report.skipped.iter().any(|s| s == rel),
            "ledger tree must not be scanned at all: {rel}"
        );
    }
    assert!(report.audit == 1, "the import itself still audits");
}

/// Test 5 — a completely empty legacy tree imports cleanly: schema +
/// marker, and every missing input is listed in `skipped`.
#[test]
fn empty_legacy_tree_imports_schema_and_marker_only() {
    let dir = TmpDir::new();
    let report = import_once(&dir.path().join("bank.db"), |_| None).expect("import");
    assert_eq!(report.accounts, 1, "the default account still lands");

    // Every expected legacy input is reported missing, never silently absent.
    // Since #1199 PR-0.5 (T-02) the ledger trees are not scanned at all, so
    // only the three state snapshots are expected here.
    for rel in ["state.json", "runtime/config.json", "update/state.json"] {
        assert!(
            report.skipped.iter().any(|s| s == rel),
            "missing input not reported: {rel}"
        );
    }

    let db = dir.path().join("bank.db");
    let tables = scalar_i64(
        &db,
        "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
    );
    assert_eq!(
        tables, 19,
        "the 18-table v1 schema + sqlite_sequence landed (the importer builds the v1 shape; v2 of the migration runner is what drops the five tables)"
    );
    let marker = scalar_i64(
        &db,
        "SELECT count(*) FROM kernel_meta WHERE key = 'import_v1'",
    );
    assert_eq!(marker, 1);
}

/// Test 6 — the canonical table shapes spot check: the TABLES / INDEXES
/// inventories are fully present, and the columns the import writes match
/// the canonical DDL (account_credentials' env-name-only shape, orders'
/// payload_json column, account_risk's fixed six-column row).
#[test]
fn canonical_tables_and_columns_are_present() {
    let dir = TmpDir::new();
    import_once(&dir.path().join("bank.db"), |_| None).expect("import");
    let conn = Connection::open_with_flags(
        dir.path().join("bank.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open db");

    for table in TABLES {
        let found: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(found, 1, "canonical table missing: {table}");
    }
    for index in INDEXES {
        let found: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(found, 1, "canonical index missing: {index}");
    }

    // The column shape of the tables the import writes, per the canonical
    // DDL: credentials store env NAMES (no value column), orders keep the
    // whole row in payload_json (no business-column split).
    let column_names = |table: &str| -> Vec<String> {
        let sql = format!("SELECT name FROM pragma_table_info('{table}')");
        let mut stmt = conn.prepare(&sql).expect("prepare");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect")
    };
    let credentials = column_names("account_credentials");
    for col in [
        "account_id",
        "api_key_env",
        "secret_env",
        "passphrase_env",
        "private_key_env",
        "encrypted_payload",
        "encryption_key_env",
        "updated_at_ms",
    ] {
        assert!(
            credentials.iter().any(|c| c == col),
            "account_credentials missing column: {col}"
        );
    }
    assert!(
        !credentials
            .iter()
            .any(|c| c == "env_value" || c == "api_key" || c == "secret"),
        "credentials must store env NAMES, never values"
    );

    let orders = column_names("orders");
    for col in ["account_id", "order_id", "payload_json", "updated_at_ms"] {
        assert!(
            orders.iter().any(|c| c == col),
            "orders missing column: {col}"
        );
    }
    assert_eq!(
        orders.len(),
        4,
        "orders is the fixed four-column payload shape"
    );

    let risk = column_names("account_risk");
    for col in [
        "account_id",
        "max_single_loss_usd",
        "max_daily_drawdown_usd",
        "max_position_size",
        "max_consecutive_losses",
        "cooldown_minutes",
        "source",
        "updated_at_ms",
    ] {
        assert!(
            risk.iter().any(|c| c == col),
            "account_risk missing column: {col}"
        );
    }
    assert_eq!(
        risk.len(),
        8,
        "account_risk is the fixed eight-column shape"
    );
}

/// Test 7 — the state snapshots are read into `kernel_meta` verbatim and stay
/// IN PLACE (live cells), and a second run over the same tree is a marker
/// no-op. (The `.migrated` backup for the ledger trees went with the folds,
/// #1199 PR-0.5 T-02.)
#[test]
fn state_snapshots_fold_to_kernel_meta_and_rerun_is_noop() {
    let dir = TmpDir::new();
    std::fs::create_dir_all(dir.path().join("data/runtime")).expect("mkdir");
    std::fs::write(dir.path().join("data/state.json"), "{\"round\":7}").expect("write");
    std::fs::write(dir.path().join("data/runtime/config.json"), "{}").expect("write");

    let db = dir.path().join("data/bank.db");
    let report = import_once(&db, |_| None).expect("import");
    assert_eq!(report.accounts, 1);

    // The state snapshots are LIVE cells, not dead history: they are read
    // into kernel_meta and stay in place (the boot's credentials-cell
    // migration strips runtime/config.json and the update state keeps
    // being rewritten).
    for rel in ["data/state.json", "data/runtime/config.json"] {
        assert!(dir.path().join(rel).exists(), "live cell stays: {rel}");
    }
    let db2 = Connection::open(&db).expect("reopen");
    let legacy_state: String = db2
        .query_row(
            "SELECT value FROM kernel_meta WHERE key = 'legacy_state_json'",
            [],
            |r| r.get(0),
        )
        .expect("state.json folded");
    assert!(legacy_state.contains("\"round\":7"), "{legacy_state}");
    let runtime_state: String = db2
        .query_row(
            "SELECT value FROM kernel_meta WHERE key = 'runtime_config_json'",
            [],
            |r| r.get(0),
        )
        .expect("config.json folded");
    assert!(runtime_state.contains("{}"), "{runtime_state}");

    // The second run is the marker no-op: nothing is re-imported.
    let second = import_once(&db, |_| None).expect("second import");
    assert_eq!(second.accounts, 0);
    assert_eq!(second.audit, 0);
}

/// Issue #1189 acceptance (handoff §4): "after importing old data, the
/// ledger, positions, and rounds match what they were before the import".
/// Since #1199 PR-0.5 (T-02) the ledger/positions half of that contract is
/// retired — those books live in the JSONL files, untouched by the import
/// (test 4) — but the round cells (the `rounds_json` the account book
/// carried pre-import) still land on the account rows, so this test keeps
/// the rounds half of the acceptance.
#[test]
fn account_rounds_cells_survive_the_import() {
    let dir = TmpDir::new();
    // A round cell on the account row (the rounds_json the account book
    // carried pre-import).
    let env = |key: &str| match key {
        "ACCOUNT_LIST" => Some("default,paper".to_string()),
        "ACCOUNT_DEFAULT_ROUNDS" => Some("[900,3600]".to_string()),
        "ACCOUNT_PAPER_ROUNDS" => Some("[300]".to_string()),
        _ => None,
    };

    let db = dir.path().join("data/bank.db");
    let report = import_once(&db, env).expect("import");
    assert_eq!(report.accounts, 2);
    assert_eq!(report.risk_rows, 2);
    assert_eq!(report.audit, 1);

    // The round cells ride the account rows (rounds_json).
    let rounds_a = scalar_text(&db, "SELECT rounds_json FROM accounts WHERE id = 'default'");
    assert!(rounds_a.contains("900"), "{rounds_a}");
    assert!(rounds_a.contains("3600"), "{rounds_a}");
    let rounds_b = scalar_text(&db, "SELECT rounds_json FROM accounts WHERE id = 'paper'");
    assert!(rounds_b.contains("300"), "{rounds_b}");
}
