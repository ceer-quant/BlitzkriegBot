#![cfg(test)]

use super::*;

use crate::account::AccountId;
use crate::engine::EngineConfig;
use crate::strategies::test_support;
use blitzkrieg_market_api::net::now_ms;

// ── issue 925: the default-account strategy toggle PERSISTS at the wire ──────

/// A core whose accounts book names a LEGACY path that does NOT exist (the
/// exact #920 production shape: `accounts_config_path` configured, the toml
/// retired, .env the single source) and a `.env` beside it in a scratch dir.
/// `strategy.enable` for the default account must land the durable half —
/// the old gate (`accounts_config_path.exists()`) held it permanently false,
/// so the engine flag moved but the env half never did and every toggle died
/// at the next boot.
#[tokio::test]
async fn default_enable_persists_the_env_half_without_the_legacy_toml() {
    let dir = std::env::temp_dir().join(format!("bk-strategy-verbs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    // The legacy path is only ever READ (its absence is the point); the .env
    // is ABSENT at start and must exist after the enable.
    let accounts_path = dir.join("accounts.toml");
    let env_path = dir.join(".env");
    let book = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: AccountId::from("default"),
            name: "default".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            opening_equity: None,
            updated_at_ms: now_ms(),
        }],
        AccountId::from("default"),
    );
    let cfg = CoreConfig {
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        env_file_path: Some(env_path.to_string_lossy().into_owned()),
        accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg, book);
    c.open_account_risk_db(dir.join("store.sqlite"))
        .expect("settings store opens");
    let mut engine = crate::engine::Engine::new(EngineConfig::default());
    test_support::host_disabled(
        &mut engine,
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    c.enable_engine(engine);
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    assert!(
        !env_path.exists(),
        "precondition: no .env before the operator touches anything"
    );
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"strategy.enable","params":{"name":"spread_arb","enabled":true,"accountId":"default"}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the enable must answer: {reply}"
    );
    assert_eq!(
        reply["result"]["found"],
        serde_json::json!(true),
        "the registered package is addressed: {reply}"
    );
    assert_eq!(
        reply["result"]["applied"],
        serde_json::json!(true),
        "issue 925: the scoped write owns the enablement — applied must be TRUE \
         even though the live dispatch hosted no pre-existing instance: {reply}"
    );
    assert_eq!(reply["result"]["accountId"], serde_json::json!("default"),);
    let db = crate::risk::account_store::AccountRiskDb::open(dir.join("store.sqlite"))
        .expect("reopen the store");
    assert_eq!(
        db.load_settings()
            .unwrap()
            .get("default")
            .and_then(|k| k.get(crate::risk::account_store::STRATEGIES_SETTING))
            .map(String::as_str),
        Some("[\"spread_arb\"]"),
        "the durable half is the account_settings row"
    );

    // The toggle-off half: empty set = the key LEAVES the file (the same
    // contract the non-default branch runs).
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"strategy.enable","params":{"name":"spread_arb","enabled":false,"accountId":"default"}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the disable must answer: {reply}"
    );
    assert_eq!(
        reply["result"]["applied"],
        serde_json::json!(true),
        "{reply}"
    );
    assert_eq!(
        db.load_settings()
            .unwrap()
            .get("default")
            .and_then(|k| k.get(crate::risk::account_store::STRATEGIES_SETTING))
            .map(String::as_str),
        Some("[]"),
        "an empty set is an explicit row, never a resurrected env key"
    );
    let written = std::fs::read_to_string(&env_path).unwrap_or_default();
    assert!(
        !written.contains("ACCOUNT_DEFAULT_STRATEGIES"),
        "the strategy set never lands in .env: {written}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The OTHER side of the same seam: a core with NO durable accounts path at
/// all (the backtester's None-ing, backtest.rs) keeps the legacy engine-only
/// success — the toggle answers `applied: true` for the engine flag without
/// any pre-existing instance, and nothing is (or can be) persisted.
#[tokio::test]
async fn replay_shaped_core_keeps_the_engine_only_toggle() {
    let book = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: AccountId::from("default"),
            name: "default".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: String::new(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            opening_equity: None,
            updated_at_ms: now_ms(),
        }],
        AccountId::from("default"),
    );
    let cfg = CoreConfig {
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg, book);
    let mut engine = crate::engine::Engine::new(EngineConfig::default());
    test_support::host_disabled(
        &mut engine,
        EngineConfig::default().trend,
        EngineConfig::default().spread_arb,
    );
    c.enable_engine(engine);
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"strategy.enable","params":{"name":"spread_arb","enabled":true,"accountId":"default"}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the toggle must answer: {reply}"
    );
    assert_eq!(
        reply["result"]["found"],
        serde_json::json!(true),
        "the package registry still names it: {reply}"
    );
    assert_eq!(
        reply["result"]["applied"],
        serde_json::json!(true),
        "the engine flag moved — the persistent state is what stays home here"
    );
}

/// issue 984: strategy.reload / strategy.packages.reload 改变活分发表，
/// 必须登记进 READONLY_REFUSED —— 只读内核拒绝二者，且拒绝发生在
/// handler 之前（同 strategy.params.set）。
#[tokio::test]
async fn strategy_reload_and_packages_reload_are_refused_in_read_only_mode() {
    let cfg = CoreConfig::default();
    let mut c = Core::new(cfg);
    c.config_mut().mode = Mode::ReadOnly;
    let core = Arc::new(AsyncMutex::new(c));
    let registry = crate::market::registry::MarketPluginRegistry::new();
    let peer = PeerAuth::SameUid { uid: own_uid() };

    for method in ["strategy.reload", "strategy.packages.reload"] {
        let reply = rpc(
            &core,
            &registry,
            &peer,
            format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{}}}}"#),
        )
        .await;
        assert_eq!(
            reply["error"]["code"],
            serde_json::json!(Failure::APPLICATION),
            "read-only must refuse {method}: {reply}"
        );
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("read-only mode"),
            "read-only message must name the mode: {}",
            reply["error"]["message"]
        );
    }
}
