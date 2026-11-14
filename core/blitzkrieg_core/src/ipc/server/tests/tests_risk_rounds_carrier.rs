#![cfg(test)]

use super::*;

use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

// ── issue #1199 r2 P1-1: the T-03 zeroing — the carrier matrix ─────────────
//
// The dual `if let Some(db) … else env` branch at the risk write port is
// zeroed (the carrier decision lives in `Core::persist_risk_bounds` /
// `Core::persist_exit` alone). Split out of `tests_risk_rounds.rs` by the
// #780 ratchet (the parent file crossed 1500 lines with these three; the
// split campaign's rule: new growth past the limit splits, it does not
// allowlist). One file per carrier state:
//   - DB open (the production shape since PR-2): the whole merged row lands
//     in the DB, the .env is NOT written (read-only at runtime);
//   - env carrier (no DB): the changed keys land in .env, the receipt says
//     `persistence: store`, the working copy re-derives from the file;
//   - no store (backtester): the memory write is the whole story, honestly
//     `persisted: false` / `persistence: none` — byte for byte the
//     pre-zeroing else branch's posture.

/// A core with the SQLite risk store open (the production shape since
/// PR-2): `risk.setSystemic` / `risk.setExit` write the DB, the working
/// copies move, and the `.env` stays untouched (the DB is the cockpit
/// carrier when one is open — issue #1199 ruling).
#[tokio::test]
async fn risk_set_systemic_and_exit_write_the_db_not_the_env() {
    use rust_decimal_macros::dec;
    let dir = std::env::temp_dir().join(format!("bk-1199-db-write-{}", now_ms()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    std::fs::write(&env_path, "BK_RISK_MAX_POSITION_SIZE=999\n").unwrap();

    let cfg = CoreConfig {
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);
    c.open_account_risk_db(dir.join("risk.sqlite"))
        .expect("temp risk db opens");
    let core = Arc::new(AsyncMutex::new(c));

    // risk.setSystemic: the patch lands in the DB row, the receipt reports
    // persisted: true, and the .env is NOT touched (the DB is the carrier).
    let applied = rpc(
        &core,
        &crate::market::registry::MarketPluginRegistry::new(),
        &PeerAuth::SameUid { uid: own_uid() },
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxPositionSize":7,"maxSingleLossUsd":"0.75","reason":"db carrier"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the patch must land: {applied}"
    );
    let audit = &applied["result"];
    assert_eq!(
        audit["persisted"],
        serde_json::json!(true),
        "the DB carrier is wired, so the write must land: {audit}"
    );
    // The DB row carries the values (the boot overlay's read).
    let db = crate::risk::account_store::AccountRiskDb::open(dir.join("risk.sqlite"))
        .expect("reopen the db for the read-back");
    let rows = db.load_risk();
    assert_eq!(
        rows["default"].max_position_size.value(),
        dec!(7),
        "the DB row carries the new position size: {rows:?}"
    );
    assert_eq!(
        rows["default"].max_single_loss_usd.value(),
        dec!(0.75),
        "the DB row carries the new single-loss cap: {rows:?}"
    );
    // The .env is NOT written (the DB is the carrier; the stale 999 stays
    // on file — the env is read-only at runtime).
    let on_file: std::collections::HashMap<String, String> = std::fs::read_to_string(&env_path)
        .unwrap_or_default()
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    assert_eq!(
        on_file.get("BK_RISK_MAX_POSITION_SIZE").map(String::as_str),
        Some("999"),
        "the DB carrier must not write the .env (the stale key stands): {on_file:?}"
    );
    assert!(
        !on_file.contains_key("BK_RISK_MAX_SINGLE_LOSS_USD"),
        "the DB carrier must not write the .env: {on_file:?}"
    );

    // risk.setExit: the same DB-carrier shape (the whole exit row lands).
    let applied = rpc(
        &core,
        &crate::market::registry::MarketPluginRegistry::new(),
        &PeerAuth::SameUid { uid: own_uid() },
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.setExit","params":{"stopLossPct":"8","trailingEnabled":true,"reason":"db carrier"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the patch must land: {applied}"
    );
    assert_eq!(
        applied["result"]["persisted"],
        serde_json::json!(true),
        "the DB carrier is wired, so the write must land: {applied}"
    );
    let exits = db.load_exits(&crate::exit_policy::ExitConfig::default());
    assert_eq!(
        exits["default"].stop_loss_pct,
        dec!(8),
        "the DB row carries the new stop: {exits:?}"
    );
    assert!(
        exits["default"].trailing_enabled,
        "the DB row carries the trailing switch: {exits:?}"
    );
    // The .env still untouched.
    let on_file: std::collections::HashMap<String, String> = std::fs::read_to_string(&env_path)
        .unwrap_or_default()
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    assert!(
        !on_file.contains_key("BK_EXIT_STOP_LOSS_PCT"),
        "the DB carrier must not write the .env: {on_file:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The env-carrier state of the T-03 reconciliation (the no-DB core with an
/// env file): the changed `BK_RISK_*` keys land in `.env`, the receipt
/// reports `persisted: true` / `persistence: store` (the carrier-neutral
/// spelling since the R2 P1-6 pass), and the working copies re-derive from
/// the file (the file-first ruling).
#[tokio::test]
async fn risk_set_systemic_and_exit_write_the_env_when_no_db() {
    use rust_decimal_macros::dec;
    let dir = std::env::temp_dir().join(format!("bk-env-carrier-{}", now_ms()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    std::fs::write(&env_path, "BK_RISK_MAX_POSITION_SIZE=999\n").unwrap();

    let cfg = CoreConfig {
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::new(cfg.clone());
    c.set_balance(cfg.dry_seed_balance);
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxPositionSize":2,"maxSingleLossUsd":"0.75","reason":"env carrier"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the patch must land: {applied}"
    );
    assert_eq!(
        applied["result"]["persisted"],
        serde_json::json!(true),
        "the env file is wired, so the write must land: {applied}"
    );
    assert_eq!(
        applied["result"]["persistence"],
        serde_json::json!("store"),
        "the receipt names the carrier: {applied}"
    );
    let on_file: std::collections::HashMap<String, String> = std::fs::read_to_string(&env_path)
        .unwrap_or_default()
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    assert_eq!(
        on_file.get("BK_RISK_MAX_POSITION_SIZE").map(String::as_str),
        Some("2"),
        "the env carrier wrote the key: {on_file:?}"
    );
    assert_eq!(
        on_file
            .get("BK_RISK_MAX_SINGLE_LOSS_USD")
            .map(String::as_str),
        Some("0.75"),
        "the env carrier wrote the key: {on_file:?}"
    );

    // risk.setExit: the env carrier writes the changed BK_EXIT_* keys and
    // re-derives the working copy from the file.
    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.setExit","params":{"stopLossPct":"8","trailingEnabled":true,"reason":"env carrier"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the patch must land: {applied}"
    );
    assert_eq!(applied["result"]["persistence"], serde_json::json!("store"));
    let on_file: std::collections::HashMap<String, String> = std::fs::read_to_string(&env_path)
        .unwrap_or_default()
        .lines()
        .filter_map(crate::env_file::parse_line)
        .collect();
    assert_eq!(
        on_file.get("BK_EXIT_STOP_LOSS_PCT").map(String::as_str),
        Some("8"),
        "the env carrier wrote the exit key: {on_file:?}"
    );
    assert_eq!(
        on_file.get("BK_EXIT_TRAILING_ENABLED").map(String::as_str),
        Some("true"),
        "the env carrier wrote the trailing switch: {on_file:?}"
    );
    // The working copy re-derived from the file (the file-first ruling).
    {
        let c = core.lock().await;
        assert_eq!(
            c.config().positions.exit.stop_loss_pct,
            dec!(8),
            "the working copy re-derived from the file"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The backtester state of the T-03 reconciliation (the core with NO store
/// at all): the direct memory write is the whole story, honestly reported
/// as `persisted: false` / `persistence: none` — the same honest posture
/// the pre-zeroing else branch gave, byte for byte.
#[tokio::test]
async fn risk_set_systemic_and_exit_stay_memory_only_without_a_store() {
    let (core, registry, peer) = hot_reload_fixture().await;

    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"risk.setSystemic","params":{"maxPositionSize":1,"maxDailyDrawdownUsd":50,"reason":"memory only"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the patch must land: {applied}"
    );
    assert_eq!(
        applied["result"]["persisted"],
        serde_json::json!(false),
        "no store = the honest memory-only report: {applied}"
    );
    assert_eq!(
        applied["result"]["persistence"],
        serde_json::json!("none"),
        "no store = the honest memory-only report: {applied}"
    );
    // The in-memory matrix DOES carry the operator's intent for this session.
    {
        let c = core.lock().await;
        assert_eq!(
            c.risk_config().systemic.account.max_position_size.value(),
            rust_decimal::Decimal::from(1),
            "the memory write landed"
        );
    }

    let applied = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"risk.setExit","params":{"stopLossPct":"8","trailingEnabled":true,"reason":"memory only"}}"#.to_string(),
    )
    .await;
    assert!(
        applied.get("error").is_none(),
        "the patch must land: {applied}"
    );
    assert_eq!(applied["result"]["persisted"], serde_json::json!(false));
    assert_eq!(applied["result"]["persistence"], serde_json::json!("none"));
    {
        let c = core.lock().await;
        assert_eq!(
            c.config().positions.exit.stop_loss_pct,
            rust_decimal::Decimal::from(8),
            "the memory write landed"
        );
    }
}
