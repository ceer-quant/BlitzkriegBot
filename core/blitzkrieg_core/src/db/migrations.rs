//! Migration runner for the SQLite store (issue #1189 §2/§5).
//!
//! The schema version is tracked in `schema_migrations` — one row per applied
//! migration — so a later wave can add a v2 without guessing what a database
//! on disk already contains. Migrations are applied in ascending order, each
//! inside its own transaction: a failure rolls that migration back and leaves
//! the previous version intact, so the next boot retries the same step.
//!
//! [`SCHEMA_V1`](crate::db::schema::SCHEMA_V1) is idempotent (`IF NOT EXISTS`
//! everywhere), so a partially applied v1 is completed by re-running it.

use rusqlite::Connection;

use crate::db::schema;

/// One schema migration: a version number, a human name, and the SQL that
/// brings a database to that version.
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Every migration this build knows, ascending. Append-only: an existing
/// version's SQL is never edited, a correction is a new version.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: schema::SCHEMA_VERSION,
    name: "v1: accounts, credentials, risk, runtime, ledger",
    sql: schema::SCHEMA_V1,
}];

/// The version ledger's own DDL. Created before the first read so a brand-new
/// database can be inspected without a chicken-and-egg step;
/// [`schema::SCHEMA_V1`] repeats it to stay a complete standalone schema.
const BOOTSTRAP: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
  version       INTEGER PRIMARY KEY,
  applied_at_ms INTEGER NOT NULL
);";

/// Milliseconds since the Unix epoch, or 0 if the clock is before it.
fn now_ms() -> i64 {
    crate::db::now_ms()
}

/// Validate the migration list before touching a database: versions must be
/// unique, contiguous and start at 1. A gap would let a database skip a step
/// and a duplicate would apply one twice.
pub fn validate(migrations: &[Migration]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !migrations.is_empty(),
        "migration list is empty — a database with no v1 is not a database"
    );
    for (index, m) in migrations.iter().enumerate() {
        let expected = index as i64 + 1;
        anyhow::ensure!(
            m.version == expected,
            "migration versions must be contiguous from 1: expected {expected}, found {} ({})",
            m.version,
            m.name
        );
    }
    Ok(())
}

/// The versions already recorded in `schema_migrations`.
pub fn applied_versions(conn: &Connection) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// The highest applied version, or 0 on a fresh database.
pub fn current_version(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )?)
}

/// Apply every migration the database has not seen yet. Safe to call on every
/// open: applied versions are skipped.
pub fn run(conn: &Connection) -> anyhow::Result<()> {
    validate(MIGRATIONS)?;
    conn.execute_batch(BOOTSTRAP)?;
    let applied = applied_versions(conn)?;
    for migration in MIGRATIONS {
        if applied.contains(&migration.version) {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at_ms) VALUES (?1, ?2)",
            rusqlite::params![migration.version, now_ms()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::TempDb;

    #[test]
    fn migration_list_is_valid() {
        validate(MIGRATIONS).expect("shipped migration list is contiguous from 1");
    }

    #[test]
    fn validate_rejects_gaps_and_duplicates() {
        let one = Migration {
            version: 1,
            name: "one",
            sql: "SELECT 1",
        };
        let three = Migration {
            version: 3,
            name: "three",
            sql: "SELECT 1",
        };
        assert!(validate(&[one, three]).is_err(), "gap must be refused");
        assert!(
            validate(&[
                Migration {
                    version: 1,
                    name: "a",
                    sql: ""
                },
                Migration {
                    version: 1,
                    name: "b",
                    sql: ""
                }
            ])
            .is_err(),
            "duplicate version must be refused"
        );
        assert!(
            validate(&[Migration {
                version: 2,
                name: "two",
                sql: ""
            }])
            .is_err(),
            "must start at 1"
        );
    }

    #[test]
    fn fresh_database_records_the_current_version() {
        // A raw connection with NO migrations applied: version 0.
        let dir = std::env::temp_dir().join(format!(
            "blitzkrieg-mig-fresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = Connection::open(dir.join("fresh.sqlite")).unwrap();
        raw.execute_batch(BOOTSTRAP).unwrap();
        assert_eq!(current_version(&raw).unwrap(), 0);
        run(&raw).unwrap();
        assert_eq!(current_version(&raw).unwrap(), schema::SCHEMA_VERSION);
        assert_eq!(
            applied_versions(&raw).unwrap(),
            vec![schema::SCHEMA_VERSION]
        );
        drop(raw);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rerunning_is_idempotent() {
        let db = TempDb::new();
        run(&db.connection()).unwrap();
        run(&db.connection()).unwrap();
        run(&db.connection()).unwrap();
        assert_eq!(
            applied_versions(&db.connection()).unwrap(),
            vec![schema::SCHEMA_VERSION],
            "an applied version is never recorded twice"
        );
    }

    #[test]
    fn every_table_and_index_exists() {
        let db = TempDb::new();
        run(&db.connection()).unwrap();
        let conn = db.connection();
        for table in schema::TABLES {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "table {table} must exist exactly once");
        }
        for index in schema::INDEXES {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    [index],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "index {index} must exist exactly once");
        }
    }

    #[test]
    fn every_table_is_queryable() {
        let db = TempDb::new();
        run(&db.connection()).unwrap();
        for table in schema::TABLES {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            db.connection()
                .query_row(&sql, [], |r| r.get::<_, i64>(0))
                .unwrap_or_else(|e| panic!("table {table} must be queryable: {e}"));
        }
    }
}
