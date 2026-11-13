//! The #1189 importer's test module — the acceptance matrix: idempotency,
//! per-account isolation, no DRY_RUN leakage, the orders fold, the
//! `.migrated` backup, the canonical-table spot checks, and the empty-tree
//! import. Tmpdirs follow the repo discipline (temp dir + pid + atomic
//! counter; no `tempfile` dependency).

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

/// Test 4 — the orders.jsonl fold: a missing accountId reads as `default`,
/// the latest snapshot per order id wins (OrderDb::load rule), the whole
/// row lands in payload_json (no business-column split), and the file is
/// backed up as `.migrated` after the commit.
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
    let o1_status: String = {
        let conn = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open db");
        conn.query_row(
            "SELECT json_extract(payload_json, '$.status') FROM orders WHERE order_id = 'o1'",
            [],
            |row| row.get(0),
        )
        .expect("payload status")
    };
    assert_eq!(o1_status, "PartiallyFilled", "the later snapshot wins");
    let o2_account = scalar_text(&db, "SELECT account_id FROM orders WHERE order_id = 'o2'");
    assert_eq!(o2_account, "paper", "accountId is read");
    let o1_account = scalar_text(&db, "SELECT account_id FROM orders WHERE order_id = 'o1'");
    assert_eq!(
        o1_account, "default",
        "missing accountId = the default book"
    );

    // The backup: the file is renamed, not deleted; the second run is a
    // marker no-op and does not double-import.
    assert!(
        !dir.path().join("data/orders/orders.jsonl").exists(),
        "the original is renamed away"
    );
    assert!(
        dir.path()
            .join("data/orders/orders.jsonl.migrated")
            .exists(),
        "orders.jsonl -> orders.jsonl.migrated"
    );
    let second = import_once(&db, |_| None).expect("second import");
    assert_eq!(second.orders, 0, "second run inserts nothing");
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM orders"), 2);
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
    assert_eq!(
        tables, 19,
        "the 18-table v1 schema + sqlite_sequence landed"
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

/// Test 7 — the backup renames every legacy family the run read, and a
/// second run over a half-backed tree is a marker no-op (no double import,
/// no error on the already-migrated files).
#[test]
fn legacy_files_are_renamed_after_commit_and_rerun_is_noop() {
    let dir = TmpDir::new();
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
        "{\"id\":\"p1\",\"accountId\":\"default\",\"updatedAtMs\":100}\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("data/trades/trades.jsonl"),
        "{\"id\":\"t1\",\"accountId\":\"default\",\"exitTime\":100}\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("data/ledger/default.journal.live.jsonl"),
        "{\"kind\":\"fill\",\"amount\":\"1.5\",\"atMs\":100}\n",
    )
    .expect("write");
    std::fs::write(dir.path().join("data/state.json"), "{\"round\":7}").expect("write");
    std::fs::create_dir_all(dir.path().join("data/runtime")).expect("mkdir");
    std::fs::write(dir.path().join("data/runtime/config.json"), "{}").expect("write");

    let db = dir.path().join("data/bank.db");
    let report = import_once(&db, |_| None).expect("import");
    assert_eq!(report.orders, 1);
    assert_eq!(report.positions, 1);
    assert_eq!(report.trades, 1);
    assert_eq!(report.ledger_entries, 1);

    for (rel, migrated) in [
        (
            "data/orders/orders.jsonl",
            "data/orders/orders.jsonl.migrated",
        ),
        (
            "data/positions/positions.jsonl",
            "data/positions/positions.jsonl.migrated",
        ),
        (
            "data/trades/trades.jsonl",
            "data/trades/trades.jsonl.migrated",
        ),
        (
            "data/ledger/default.journal.live.jsonl",
            "data/ledger/default.journal.live.jsonl.migrated",
        ),
        ("data/state.json", "data/state.json.migrated"),
        (
            "data/runtime/config.json",
            "data/runtime/config.json.migrated",
        ),
    ] {
        assert!(!dir.path().join(rel).exists(), "renamed away: {rel}");
        assert!(dir.path().join(migrated).exists(), "backed up: {migrated}");
    }

    // The second run is the marker no-op: no error even though every
    // legacy file is gone (all `.migrated`), and nothing is re-imported.
    let second = import_once(&db, |_| None).expect("second import");
    assert_eq!(second.orders, 0);
    assert_eq!(second.positions, 0);
    assert_eq!(second.trades, 0);
    assert_eq!(second.ledger_entries, 0);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM orders"), 1);
}

/// Issue #1189 acceptance (handoff §4): "after importing old data, the
/// ledger, positions, and rounds match what they were before the import".
/// The fold keeps every row's identity and payload: an open position keeps
/// its id and account, the cash journal keeps every entry's kind/amount/ts,
/// and the round cells land in kernel_meta verbatim (rounds_json is part of
/// the account row). This test writes a pre-import book, imports it, and
/// asserts the SQLite rows carry the same facts the files did.
#[test]
fn imported_rows_match_the_pre_import_book() {
    let dir = TmpDir::new();
    for sub in ["orders", "positions", "ledger"] {
        std::fs::create_dir_all(dir.path().join(format!("data/{sub}"))).expect("mkdir");
    }
    // Pre-import book: one open position on default, one on paper; a cash
    // journal with a seed and a fill on both sides.
    std::fs::write(
        dir.path().join("data/positions/positions.jsonl"),
        "{\"id\":\"pos-a\",\"accountId\":\"default\",\"tokenId\":\"tok-1\",\"entryPrice\":\"0.42\",\"shares\":\"10\",\"open\":true,\"expiresAtMs\":900000}\n\
         {\"id\":\"pos-b\",\"accountId\":\"paper\",\"tokenId\":\"tok-2\",\"entryPrice\":\"0.55\",\"shares\":\"4\",\"open\":true,\"expiresAtMs\":900000}\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("data/ledger/default.journal.dry.jsonl"),
        "{\"kind\":\"seed\",\"amount\":\"100\",\"atMs\":100}\n\
         {\"kind\":\"fill\",\"amount\":\"-0.42\",\"atMs\":200}\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join("data/ledger/paper.journal.dry.jsonl"),
        "{\"kind\":\"seed\",\"amount\":\"50\",\"atMs\":100}\n",
    )
    .expect("write");
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
    assert_eq!(report.positions, 2);
    assert_eq!(report.ledger_entries, 3);

    // The positions table carries both rows with their payloads.
    let pos_json = scalar_text(
        &db,
        "SELECT payload_json FROM positions WHERE account_id = 'default' AND position_id = 'pos-a'",
    );
    assert!(pos_json.contains("\"tokenId\":\"tok-1\""), "{pos_json}");
    assert!(pos_json.contains("\"entryPrice\":\"0.42\""), "{pos_json}");
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT count(*) FROM positions WHERE account_id = 'paper' AND position_id = 'pos-b'"
        ),
        1
    );

    // The ledger entries keep kind/amount/ts per account.
    assert_eq!(
        scalar_text(
            &db,
            "SELECT kind FROM ledger_entries WHERE account_id = 'default' ORDER BY id"
        ),
        "seed"
    );
    assert_eq!(
        scalar_text(
            &db,
            "SELECT amount FROM ledger_entries WHERE account_id = 'default' AND kind = 'fill'"
        ),
        "-0.42"
    );
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT count(*) FROM ledger_entries WHERE account_id = 'paper'"
        ),
        1
    );

    // The round cells ride the account rows (rounds_json).
    let rounds_a = scalar_text(&db, "SELECT rounds_json FROM accounts WHERE id = 'default'");
    assert!(rounds_a.contains("900"), "{rounds_a}");
    assert!(rounds_a.contains("3600"), "{rounds_a}");
    let rounds_b = scalar_text(&db, "SELECT rounds_json FROM accounts WHERE id = 'paper'");
    assert!(rounds_b.contains("300"), "{rounds_b}");
}
