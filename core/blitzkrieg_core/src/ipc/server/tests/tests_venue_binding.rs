#![cfg(test)]

use super::*;

use crate::account::{AccountId, AccountStatus};
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

// ── the account↔venue binding verbs, at the wire ─────────────────────────

/// 运行时半边（绑定即启用 at runtime）：绑到一个本会话从未挂载的 venue
/// 时，`account.venue.set` 的写路径必须把该 venue 的数据面挂进运行中的
/// 会话 —— 回执 `runtimeEnabled: true`，feed 的订阅控制句柄落到内核
/// （`subscribe_feed_tokens` 的路由键就位），discovery 注册的带 venue
/// 标签的市场行出现在 scanner（ENGINE_ROUND 富化的数据源）。缺了这半边
/// 的会话里，运行中改绑的 venue 永远没有行情 —— 行情页对新所永远
/// 「加载中」。
#[tokio::test]
async fn venue_set_mounts_a_never_booted_venues_data_plane_at_runtime() {
    // A private .env — the persist half of account.venue.set refuses while
    // a legacy toml sits beside it (the same posture the round-duration
    // card runs), so the fixture must carry one, exactly the way the
    // round-trip test below does. The ENGINE is installed too: the
    // scanner assertion at the bottom reads `engine.scanner().round_state`
    // and an engine-less core would answer it with an empty list for the
    // WRONG reason (no scanner at all, not "the venue never mounted").
    let dir = std::env::temp_dir().join(format!("bk-venue-mount-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=default,paper\nACCOUNT_DEFAULT_NAME=\"main\"\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");

    let (core, registry, peer) = {
        let now = now_ms();
        let book = crate::account::AccountLedgers::new(
            vec![
                crate::account::Account {
                    id: AccountId::from("default"),
                    name: "main".into(),
                    market_type: blitzkrieg_market_api::MarketType::Prediction,
                    status: AccountStatus::Active,
                    credential_keys: crate::account::CredentialKeys::default(),
                    venue: String::new(),
                    trading_enabled: true,
                    strategies: Vec::new(),
                    rounds: Vec::new(),
                    updated_at_ms: now,
                },
                crate::account::Account {
                    id: AccountId::from("paper"),
                    name: "paper".into(),
                    market_type: blitzkrieg_market_api::MarketType::Prediction,
                    status: AccountStatus::Active,
                    credential_keys: crate::account::CredentialKeys::default(),
                    venue: String::new(),
                    trading_enabled: true,
                    strategies: Vec::new(),
                    rounds: Vec::new(),
                    updated_at_ms: now,
                },
            ],
            AccountId::from("default"),
        );
        let cfg = CoreConfig {
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            engine_enabled: true,
            round_duration_sec: 900,
            allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let mut c = Core::with_account_book(cfg, book);
        c.enable_engine(crate::engine::Engine::new(c.config().engine_config()));
        // A registry with ONLY the runtime venue: boot actives below name
        // a polymarket plugin that this test never registers — the point
        // is that the venue.set mount works for a plugin the boot never
        // selected, and the compiled-in registry is empty in a bare test
        // binary anyway (the same trick the round-trip test uses).
        let registry = crate::market::registry::MarketPluginRegistry::new();
        registry.register(Box::new(VenueLivePlugin {
            discovery: VenueLiveDiscovery {
                tag: "venuelive".to_string(),
            },
            name: "venuelive".to_string(),
        }));
        (
            Arc::new(AsyncMutex::new(c)),
            registry,
            PeerAuth::SameUid { uid: own_uid() },
        )
    };
    // Boot shape: engine + feed-ws ON (production supervisor's default
    // extra_args), boot actives = {polymarket} — venuelive NEVER mounted.
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let mut mounted = std::collections::BTreeSet::new();
    mounted.insert(("polymarket".to_string(), 900));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: true,
        discovery_enabled: true,
        discovery_assets: vec!["BTC".to_string(), "ETH".to_string()],
        discovery_round_duration_sec: 900,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(mounted)),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };

    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{"accountId":"paper","venue":"venuelive","reason":"runtime mount"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "the bind must land: {reply}");
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(true),
        "the reply must say the venue's data plane was mounted: {reply}"
    );

    // Feed: the subscription control landed under the venue's key — the
    // route `subscribe_feed_tokens` takes on the next rollover. The call
    // is the assertion: it resolves through `Core.feeds`, so reaching it
    // without a panic proves the runtime feed's control is installed.
    core.lock()
        .await
        .subscribe_feed_tokens("venuelive", vec!["tok-a".to_string()])
        .await;
    assert!(
        data_plane.venue_mounted("venuelive"),
        "the mount is recorded — a re-bind must not restart the live feed"
    );

    // Discovery: the venue-tagged market row is in the scanner (the
    // ENGINE_ROUND per-row enrichment source).
    let c = core.lock().await;
    let venues: Vec<String> = c
        .engine()
        .map(|e| {
            e.scanner()
                .round_state(now_ms())
                .markets
                .iter()
                .map(|m| m.venue.clone())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        venues.iter().any(|v| v == "venuelive"),
        "the runtime-mounted venue's market must appear in the scanner: {venues:?}"
    );

    // Boot-shape config: the runtime mount must hand the plugin the
    // boot's discovery parameters, NOT zeroes — kalshi picks its series
    // from (asset, round_duration_sec) and a zero/empty config discovers
    // nothing forever (the #535 production replay defect). The stub's
    // data_plane carries BTC/ETH + 900s; that is exactly what must arrive.
    let seen = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == "venuelive")
        .map(|(_, cfg)| cfg)
        .next_back()
        .cloned()
        .expect("the runtime mount must have started the discovery");
    assert_eq!(
        seen.assets,
        vec!["BTC".to_string(), "ETH".to_string()],
        "runtime discovery must receive the boot's asset set: {seen:?}"
    );
    assert_eq!(
        seen.round_duration_sec, 900,
        "runtime discovery must receive the boot's round length: {seen:?}"
    );

    // Idempotence: a SECOND account bound to the SAME venue must not
    // restart the live feed (the mounted-set guard fires, the reply says
    // the venue was already live).
    drop(c);
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.venue.set","params":{"accountId":"default","venue":"venuelive","reason":"idempotence"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "re-bind must succeed: {reply}"
    );
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(false),
        "an already-mounted venue must not be re-mounted: {reply}"
    );
}

/// #777: `account.create`'s runtime-mount half must FAIL CLOSED when the core
/// lock is not free. The old `try_lock()` silently mounted only the global
/// family, then `mark_discovery_mounted` recorded it — the account's `rounds`
/// families were never retried. The extracted helper is the logic under test:
/// on timeout it returns `Timeout` and touches NEITHER the mounted set NOR the
/// venue guard; a later call (the caller's retry) mounts the full family set.
#[tokio::test]
async fn account_create_mount_times_out_fail_closed_then_retry_mounts_all_families() {
    let now = now_ms();
    let book = crate::account::AccountLedgers::new(
        vec![crate::account::Account {
            id: AccountId::from("paper"),
            name: "paper".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: "venuelive".into(),
            trading_enabled: true,
            strategies: Vec::new(),
            // The new account's round family — must be mounted alongside the
            // global 900s family.
            rounds: vec![60],
            updated_at_ms: now,
        }],
        AccountId::from("paper"),
    );
    let cfg = CoreConfig {
        engine_enabled: true,
        round_duration_sec: 900,
        allowed_round_secs: crate::account::config::TEST_ALLOWED_ROUNDS.to_vec(),
        trade_log_path: None,
        order_log_path: None,
        position_log_path: None,
        ..Default::default()
    };
    let mut c = Core::with_account_book(cfg, book);
    c.enable_engine(crate::engine::Engine::new(c.config().engine_config()));
    let core = Arc::new(AsyncMutex::new(c));

    let registry = crate::market::registry::MarketPluginRegistry::new();
    registry.register(Box::new(VenueLivePlugin {
        discovery: VenueLiveDiscovery {
            tag: "venuelive".to_string(),
        },
        name: "venuelive".to_string(),
    }));
    let plugin = registry.get("venuelive").expect("the venue is registered");
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: true,
        discovery_enabled: true,
        discovery_assets: vec!["BTC".to_string()],
        discovery_round_duration_sec: 900,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::BTreeSet::new(),
        )),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };

    // Hold the core lock — the maintenance-loop shape #777 describes.
    let guard = core.lock().await;
    let err = crate::ipc::server::handlers::account::mount_account_create_families(
        &core,
        plugin.as_ref(),
        &host,
        &registry,
        &data_plane,
        "venuelive",
        std::time::Duration::from_millis(20),
    )
    .await
    .expect_err("a held core lock must time out, never silently mount a partial set");
    assert_eq!(
        err.code,
        crate::model::CoreErrorCode::Timeout,
        "the failure must be an explicit Timeout: {err}"
    );
    assert!(
        !data_plane.discovery_mounted_at("venuelive", 900),
        "the global family must NOT be marked mounted on the failure path"
    );
    assert!(
        !data_plane.discovery_mounted_at("venuelive", 60),
        "the account's round family must NOT be marked mounted on the failure path"
    );
    assert!(
        !data_plane.venue_mounted("venuelive"),
        "the feed guard must NOT be set on the failure path"
    );
    drop(guard);

    // Retry (what the caller does after the explicit error): the full family
    // set mounts — the earlier failure did not poison the idempotence guard.
    let mounted = crate::ipc::server::handlers::account::mount_account_create_families(
        &core,
        plugin.as_ref(),
        &host,
        &registry,
        &data_plane,
        "venuelive",
        std::time::Duration::from_secs(2),
    )
    .await
    .expect("the retry acquires the free lock");
    assert!(mounted, "the retry must report runtimeEnabled");
    assert!(
        data_plane.discovery_mounted_at("venuelive", 900),
        "the global family is mounted on the success path"
    );
    assert!(
        data_plane.discovery_mounted_at("venuelive", 60),
        "the account's rounds family is mounted on the success path"
    );
    assert!(
        data_plane.venue_mounted("venuelive"),
        "the feed guard is set only after a real mount"
    );
}
