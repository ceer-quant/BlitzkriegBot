//! 操作者裁决 2026-11-14（#1199 第六轮）：**`.env` 对内核只读**。这是守门
//! 用例：把一整面写路径打一遍（轮次、策略、凭证），`.env` 必须逐字节不变；
//! 所有事实必须落进 SQLite。任何一条失败都说明 env 写入路径复活了——那是
//! 违约，不是回归。

use std::sync::Arc;

use crate::db::credentials::MASTER_KEY_ENV;
use crate::ipc::server::PeerAuth;
use crate::ipc::server::tests::{own_uid, rpc};
use crate::service::{Core, CoreConfig};
use tokio::sync::Mutex as AsyncMutex;

use crate::db::credentials::ENV_LOCK;

/// The stub credential-bearing plugin (same shape as the shared one in
/// `tests/mod.rs`): declares one env var so `market.credentials.set` has an
/// allowed-set to validate against.
struct CredStubPlugin;
impl blitzkrieg_market_api::MarketPlugin for CredStubPlugin {
    fn name(&self) -> &str {
        "stub"
    }
    fn market_type(&self) -> blitzkrieg_market_api::MarketType {
        blitzkrieg_market_api::MarketType::Prediction
    }
    fn supported_round_secs(&self) -> Option<Vec<i64>> {
        Some(crate::account::config::TEST_ALLOWED_ROUNDS.to_vec())
    }
    fn required_credential_env(&self) -> &'static [&'static str] {
        &["STUB_API_KEY"]
    }
}

#[tokio::test]
async fn the_write_battery_never_touches_the_env_file() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let dir = std::env::temp_dir().join(format!("bk-env-readonly-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let env_path = dir.join(".env");
    let seed = "KEEP=1\nPOLYMARKET_PRIVATE_KEY=\"legacy\"\n";
    std::fs::write(&env_path, seed).unwrap();

    // A fresh master key so the credential write face encrypts (the installer
    // generates exactly this spelling on first boot).
    let key = crate::db::credentials::encode_blob(&{
        use rand::RngCore as _;
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        raw.to_vec()
    });
    // SAFETY: the ENV_LOCK serializes every test that touches this var; the
    // guard restores the boot spelling on drop.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            #[cfg(unix)]
            unsafe { std::env::remove_var(MASTER_KEY_ENV) };
        }
    }
    unsafe { std::env::set_var(MASTER_KEY_ENV, &key) };
    let _restore = Restore;

    let cfg = CoreConfig {
        db_path: Some(dir.join("store.sqlite")),
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        round_labels: [(300, "5m"), (900, "15m")].into_iter().collect(),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut core = Core::new(cfg.clone());
    core.set_balance(cfg.dry_seed_balance);
    let core = Arc::new(AsyncMutex::new(core));
    let mut registry = crate::market::registry::MarketPluginRegistry::new();
    registry.register(Box::new(CredStubPlugin));
    let peer = PeerAuth::SameUid { uid: own_uid() };

    // ── the write battery ──────────────────────────────────────────────
    let set_round = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"round_duration.set","params":{"accountId":"default","durationSec":300,"reason":"guard"}}"#.to_string(),
    )
    .await;
    assert!(
        set_round.get("error").is_none(),
        "round_duration.set must land: {set_round}"
    );

    let set_venue = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.venue.set","params":{"accountId":"default","venue":"stub"}}"#.to_string(),
    )
    .await;
    assert!(
        set_venue.get("error").is_none(),
        "account.venue.set must land: {set_venue}"
    );

    let set_creds = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"market.credentials.set","params":{"plugin":"stub","vars":{"STUB_API_KEY":"stub-value"}}}"#.to_string(),
    )
    .await;
    assert!(
        set_creds.get("error").is_none(),
        "market.credentials.set must land (the master key is set): {set_creds}"
    );

    // ── the contract: the file is byte-identical ───────────────────────
    let after = std::fs::read_to_string(&env_path).unwrap();
    assert_eq!(
        after, seed,
        "the kernel must NEVER write `.env` — the file changed under the \
         write battery; an env write path is alive"
    );

    // ── and every fact landed in SQLite instead ────────────────────────
    let db = crate::risk::account_store::AccountRiskDb::open(dir.join("store.sqlite")).unwrap();
    let settings = db.load_settings().unwrap();
    assert_eq!(
        settings
            .get("default")
            .and_then(|k| k.get(crate::risk::account_store::ROUND_DURATION_SETTING))
            .map(String::as_str),
        Some("300"),
        "the round anchor is a database row"
    );
    let venue = db
        .load_accounts()
        .unwrap()
        .into_iter()
        .find(|r| r.id == "default")
        .expect("the default account row");
    assert_eq!(venue.venue, "stub", "the venue binding is an accounts row");
    let cred_rows = db
        .load_kernel_meta_by_prefix(crate::risk::account_store::CREDENTIAL_META_PREFIX)
        .unwrap();
    assert!(
        cred_rows
            .iter()
            .any(|(k, _)| k == "credential.STUB_API_KEY"),
        "the credential value is an encrypted database row: {cred_rows:?}"
    );
    // The plaintext never leaks into any file in the scratch dir.
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        assert!(
            !text.contains("stub-value"),
            "the credential plaintext must never land on disk: {}",
            entry.path().display()
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
