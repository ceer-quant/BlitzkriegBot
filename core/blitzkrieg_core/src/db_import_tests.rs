//! The #1189 importer's test module — the acceptance matrix: idempotency,
//! per-account isolation, no DRY_RUN leakage, the orders fold, and the
//! empty-tree import. Tmpdirs follow the repo discipline (temp dir + pid +
//! atomic counter; no `tempfile` dependency).

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
    assert_eq!(first.risk_rows, 1, "the global risk key lands once");
    assert_eq!(first.audit, 1, "one audit row");

    let second = import_once(&dir.path().join("bank.db"), env).expect("second import");
    assert_eq!(second.accounts, 0);
    assert_eq!(second.risk_rows, 0);
    assert_eq!(second.exit_rows, 0);
    assert_eq!(second.settings, 0);
    assert_eq!(second.orders, 0);
    assert_eq!(second.positions, 0);
    assert_eq!(second.trades, 0);
    assert_eq!(second.ledger_entries, 0);
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
    assert_eq!(report.risk_rows, 2, "the override replaces, not doubles");

    let db = dir.path().join("bank.db");
    let alpha = scalar_text(
        &db,
        "SELECT value FROM account_risk WHERE account_id = 'alpha' AND key = 'MAX_POSITION_SIZE'",
    );
    let beta = scalar_text(
        &db,
        "SELECT value FROM account_risk WHERE account_id = 'beta' AND key = 'MAX_POSITION_SIZE'",
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

/// Test 4 — the orders.jsonl fold: a missing accountId reads as `default`,
/// and the latest snapshot per order id wins (OrderDb::load rule).
#[test]
fn orders_jsonl_folds_to_latest_snapshot_per_id() {
    let dir = TmpDir::new();
    std::fs::create_dir_all(dir.path().join("data/orders")).expect("mkdir");
    let jsonl = r#"{"orderId":"o1","strategy":"s","asset":"BTC","direction":"up","side":"buy","price":"0.55","size":"10","filledSize":"0","status":"Submitted","submittedAtMs":100,"updatedAtMs":100}
{"orderId":"o2","accountId":"paper","strategy":"s","asset":"ETH","direction":"down","side":"sell","price":"12.5","size":"3","filledSize":"0","status":"Submitted","submittedAtMs":200,"updatedAtMs":200}
{"orderId":"o1","strategy":"s","asset":"BTC","direction":"up","side":"buy","price":"0.55","size":"10","filledSize":"4","status":"PartiallyFilled","submittedAtMs":100,"updatedAtMs":300}
"#;
    std::fs::write(dir.path().join("data/orders/orders.jsonl"), jsonl).expect("write");

    // The db lives INSIDE `data/` — that directory is the legacy root the
    // importer scans for the JSONL trees.
    let db = dir.path().join("data/bank.db");
    let report = import_once(&db, |_| None).expect("import");
    assert_eq!(report.orders, 2, "o1 folds to ONE row (latest wins)");

    let count = scalar_i64(&db, "SELECT count(*) FROM orders");
    assert_eq!(count, 2, "two rows in the table");
    let o1_status = scalar_text(&db, "SELECT status FROM orders WHERE id = 'o1'");
    assert_eq!(o1_status, "PartiallyFilled", "the later snapshot wins");
    let o2_account = scalar_text(&db, "SELECT account_id FROM orders WHERE id = 'o2'");
    assert_eq!(o2_account, "paper", "accountId is read");
    let o1_account = scalar_text(&db, "SELECT account_id FROM orders WHERE id = 'o1'");
    assert_eq!(
        o1_account, "default",
        "missing accountId = the default book"
    );
}

/// Test 5 — a completely empty legacy tree imports cleanly: schema +
/// marker, and every missing input is listed in `skipped`.
#[test]
fn empty_legacy_tree_imports_schema_and_marker_only() {
    let dir = TmpDir::new();
    let report = import_once(&dir.path().join("bank.db"), |_| None).expect("import");
    assert_eq!(report.accounts, 1, "the default account still lands");
    assert_eq!(report.orders, 0);
    assert_eq!(report.positions, 0);
    assert_eq!(report.trades, 0);
    assert_eq!(report.ledger_entries, 0);

    // Every expected legacy input is reported missing, never silently absent.
    for rel in [
        "orders/orders.jsonl",
        "positions/positions.jsonl",
        "positions/positions.dry.jsonl",
        "trades/trades.jsonl",
        "trades/trades-live.jsonl",
        "ledger/default.journal.dry.jsonl",
        "ledger/default.journal.live.jsonl",
        "state.json",
        "runtime/config.json",
        "update/state.json",
    ] {
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
    assert_eq!(tables, 14, "the 14-table v1 schema landed");
    let marker = scalar_i64(
        &db,
        "SELECT count(*) FROM kernel_meta WHERE key = 'import_v1'",
    );
    assert_eq!(marker, 1);
}
