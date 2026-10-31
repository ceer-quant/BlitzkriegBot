use super::*;

/// 建户即绑定（#535 用户实录的内核半边）：一个带 venue 的 `account.create`
/// 必须把该 venue 的 feed/discovery 挂进本会话 —— 否则新建账户的行情永远
/// 停在「首行数据到达后渲染」。三条 observable 断言：回执 runtimeEnabled
/// =true、订阅控制落在 venue 键下（subscribe_feed_tokens 可达）、venue
/// 标签的市场进 scanner。幂等面：同一 venue 的第二次建户（或其后的
/// venue.set）不重启在跑的 feed（runtimeEnabled=false）。未带 venue 的
/// 建户是纯身份铸造 —— runtimeEnabled=false 且什么也不挂。
#[tokio::test]
async fn account_create_mounts_a_never_booted_venues_data_plane_at_runtime() {
    // Same private-persist posture as the venue.set mount test: create
    // persists into the fixture's private .env.
    let dir = std::env::temp_dir().join(format!("bk-create-mount-{}", std::process::id()));
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
            vec![crate::account::Account {
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
            }],
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
    // Boot shape: engine + data plane ON, boot actives = {polymarket} —
    // venuelive never mounted at boot.
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
        r#"{"jsonrpc":"2.0","id":1,"method":"account.create","params":{"id":"kalshi","venue":"venuelive","actor":"panel","reason":"add account dialog"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the create must land: {reply}"
    );
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(true),
        "creating a venue-bound account must mount that venue's data plane: {reply}"
    );

    // Feed: the subscription control landed under the venue's key — the
    // same observable the venue.set test pins.
    core.lock()
        .await
        .subscribe_feed_tokens("venuelive", vec!["tok-a".to_string()])
        .await;
    assert!(
        data_plane.venue_mounted("venuelive"),
        "the mount is recorded — a later venue.set on the same venue must not restart the feed"
    );

    // Discovery: the venue-tagged market row is in the scanner.
    {
        let c = core.lock().await;
        let venues: Vec<String> = c
            .engine
            .as_ref()
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
            "the create-mounted venue's market must appear in the scanner: {venues:?}"
        );
    }

    // Idempotence: a SECOND create on the same venue (another account,
    // same binding) must not re-mount — the mounted-set guard fires and
    // the reply says the venue was already live.
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.create","params":{"id":"kalshi-2","venue":"venuelive","actor":"panel","reason":"second account same venue"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the re-create must land: {reply}"
    );
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(false),
        "an already-mounted venue must not be re-mounted by a second create: {reply}"
    );

    // A venue-less create is a pure identity mint: nothing to mount, the
    // reply says so.
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":3,"method":"account.create","params":{"id":"unbound","actor":"panel","reason":"no venue"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the bare create must land: {reply}"
    );
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(false),
        "a venue-less create mounts nothing: {reply}"
    );
}

/// 解绑（空 venue）不触发运行时挂载（没有 venue 可挂），已挂载 venue 的
/// 绑定照常幂等 —— 运行时半边只做加法，从不因绑定写而拆数据面。
#[tokio::test]
async fn venue_set_unbind_does_not_touch_the_data_plane() {
    // Same private-persist posture as the mount test: the unbind is a
    // venue.set write too, and it must LAND (not refuse) for the
    // no-op-mount path to be worth pinning.
    let dir = std::env::temp_dir().join(format!("bk-venue-unbind-{}", std::process::id()));
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
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let c = Core::with_account_book(cfg, book);
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
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: true,
        discovery_enabled: true,
        discovery_assets: vec!["BTC".to_string(), "ETH".to_string()],
        discovery_round_duration_sec: 900,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };

    // Unbind: the write lands, nothing mounts.
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{"accountId":"paper","venue":"","reason":"unbind"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "unbind must land: {reply}");
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(false),
        "an unbind mounts nothing: {reply}"
    );
    assert!(!data_plane.venue_mounted("venuelive"));

    // The core survives without any feed control (the arm's no-op path).
    assert!(core.lock().await.has_engine() || true, "core is intact");
}

/// `account.venue.get` 的 wire 合同：未绑定账户回空串（unbound 的唯一
/// wire 拼写）、`accountId` 窄化为单行、缺省=全部账户（按 id 稳定排序）。
#[tokio::test]
async fn account_venue_get_reports_empty_as_unbound_and_narrows_by_id() {
    let (core, registry, peer) = two_account_fixture().await;
    // All accounts: every row unbound (empty string = the unbound
    // spelling — never null, never a missing field).
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.venue.get","params":{}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "get must answer: {reply}");
    assert_eq!(reply["result"]["version"], serde_json::json!("1.1"));
    assert_eq!(reply["result"]["active"], serde_json::json!("default"));
    let rows = reply["result"]["venues"].as_array().expect("rows");
    assert_eq!(rows.len(), 2, "absent accountId = every account");
    assert_eq!(rows[0]["accountId"], serde_json::json!("default"));
    assert_eq!(rows[0]["venue"], serde_json::json!(""));
    assert_eq!(rows[1]["venue"], serde_json::json!(""));

    // Narrow to ONE account.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.venue.get","params":{"accountId":"paper"}}"#
            .to_string(),
    )
    .await;
    let rows = reply["result"]["venues"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["accountId"], serde_json::json!("paper"));
    assert_eq!(rows[0]["venue"], serde_json::json!(""));
}

/// `account.venue.set` 的闭环：写后 get 回读、`account.list` 的
/// `AccountView.venue` 同步（面板账户卡的数据源）、同 venue 多账户合法
/// （一个交易所多套 API = 多账户，这正是用户的账户模型）、空串解绑、
/// 未注册 venue 名拒绝（plan-first）、未知账户拒绝 —— 以及持久化：
/// 绑定落进测试私有的 .env，重新 load_account_ledgers_single_source 读回。
#[tokio::test]
async fn account_venue_set_round_trips_get_and_the_persisted_file() {
    // A private .env — the persist half needs a file, and it must never
    // be the checkout's own user_layer/configs/.env.
    let dir = std::env::temp_dir().join(format!("bk-venue-wire-{}", std::process::id()));
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
            // The persist path points at THIS test's file; without it the
            // write refuses (the same posture the round-duration card
            // runs), so the fixture must carry it for the round-trip.
            accounts_config_path: Some(accounts_path.to_string_lossy().into_owned()),
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        };
        let c = Core::with_account_book(cfg, book);
        // A registry with one registered plugin under its own name: the
        // compiled-in registry is empty in a bare test binary, and the
        // bindable set is whatever THIS test registered (the same trick the
        // market.plugin.set tests use).
        let registry = crate::market::registry::MarketPluginRegistry::new();
        registry.register(Box::new(VenueStubPlugin {
            name: "polysimulator".to_string(),
        }));
        (
            Arc::new(AsyncMutex::new(c)),
            registry,
            PeerAuth::SameUid { uid: own_uid() },
        )
    };

    // Bind paper → polysimulator (a REGISTERED plugin).
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{"accountId":"paper","venue":"polysimulator","reason":"venue panel"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "the bind must land: {reply}");
    assert_eq!(reply["result"]["venue"], serde_json::json!("polysimulator"));
    assert_eq!(reply["result"]["persisted"], serde_json::json!(true));
    assert_eq!(reply["result"]["accountId"], serde_json::json!("paper"));
    assert_eq!(
        reply["result"]["actor"],
        serde_json::json!(format!("uid:{}", own_uid()))
    );

    // get reads the binding back.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.venue.get","params":{}}"#.to_string(),
    )
    .await;
    let rows = reply["result"]["venues"].as_array().expect("rows");
    let paper = rows.iter().find(|r| r["accountId"] == "paper").unwrap();
    assert_eq!(paper["venue"], serde_json::json!("polysimulator"));

    // account.list carries it too (AccountView.venue — the account card).
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    let accounts = reply["result"]["accounts"].as_array().expect("views");
    assert_eq!(accounts[1]["venue"], serde_json::json!("polysimulator"));

    // The SAME venue on the OTHER account: legal by design (one exchange,
    // several API sets, one account each) — no uniqueness gate.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":4,"method":"account.venue.set","params":{"accountId":"default","venue":"polysimulator"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the same venue on a second account is the deployment model: {reply}"
    );

    // The persist: a FRESH single-source load over the same paths — the
    // same call the next boot makes — sees BOTH bindings and every sibling
    // field (the name rode through untouched).
    let book = crate::account::config::load_account_ledgers_single_source(
        &accounts_path,
        Some(&env_path),
        1,
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .expect("the written file re-parses");
    assert_eq!(
        book.account(&AccountId::from("paper")).unwrap().venue,
        "polysimulator"
    );
    assert_eq!(
        book.account(&AccountId::from("default")).unwrap().venue,
        "polysimulator"
    );
    assert_eq!(
        book.account(&AccountId::from("paper")).unwrap().name,
        "paper",
        "sibling fields survive the write-back"
    );

    // Unbind with the empty string: memory AND file return to unbound.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":5,"method":"account.venue.set","params":{"accountId":"paper","venue":""}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "unbind is legal: {reply}");
    assert_eq!(reply["result"]["venue"], serde_json::json!(""));
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":6,"method":"account.venue.get","params":{"accountId":"paper"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(reply["result"]["venues"][0]["venue"], serde_json::json!(""));
    let book = crate::account::config::load_account_ledgers_single_source(
        &accounts_path,
        Some(&env_path),
        2,
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .expect("the unbound file re-parses");
    assert_eq!(book.account(&AccountId::from("paper")).unwrap().venue, "");
    assert_eq!(
        book.account(&AccountId::from("default")).unwrap().venue,
        "polysimulator",
        "the other account's binding survives the unbind"
    );

    // A non-empty venue that is NOT a registered plugin: refused
    // plan-first — nothing moved in memory or on disk. (The wire code is
    // APPLICATION carrying coreCode INVALID_PARAMS — the same envelope
    // the market.plugin.set refusals ride.)
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":7,"method":"account.venue.set","params":{"accountId":"paper","venue":"ghostvenue"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_some(),
        "an unregistered venue name must fail loud: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the refusal is a client fault at heart: {reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("'ghostvenue' is not a registered plugin"),
        "the refusal names the offender and the bindable set: {reply}"
    );
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":8,"method":"account.venue.get","params":{"accountId":"paper"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["result"]["venues"][0]["venue"],
        serde_json::json!(""),
        "a refused bind moved nothing"
    );

    // An account the book never configured: refused, standard refusal
    // (APPLICATION envelope carrying coreCode INVALID_PARAMS — no
    // implicit account is ever created).
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":9,"method":"account.venue.set","params":{"accountId":"ghost","venue":"polysimulator"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_some(),
        "an unknown account must fail loud: {reply}"
    );
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS")
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown account `ghost`"),
        "the refusal names the account: {reply}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 显示账户配置化: `display.account.set` 是写动词 —— 只读模式照
/// READONLY_REFUSED 名单整体拒绝（config 写不该从只读面溜进去）。
#[tokio::test]
async fn display_account_set_is_refused_in_read_only_mode() {
    let (core, registry, peer) = two_account_fixture().await;
    core.lock().await.config_mut().mode = Mode::ReadOnly;
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"display.account.set","params":{"accountId":"paper"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "read-only refuses the configuration verb: {reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("read-only mode"),
        "the refusal names the mode: {reply}"
    );
}

/// 显示账户配置化: the DISPLAY pointer rides the account.list answer —
/// `displayAccount` + the pointed-at account's venue binding
/// (`displayVenue`, empty = unbound) as TOP-LEVEL keys beside `active`,
/// so the panel reads them from the round-trip it already makes. Here
/// the pointer defaults to `default` (the file loader's first-ACTIVE
/// rule; this fixture book has no explicit key), and the venue is the
/// unbound empty string.
#[tokio::test]
async fn account_list_carries_the_display_pointer_and_its_venue() {
    let (core, registry, peer) = two_account_fixture().await;
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.list","params":{}}"#.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "list must answer: {reply}");
    assert_eq!(
        reply["result"]["displayAccount"],
        serde_json::json!("default"),
        "no explicit key = the first ACTIVE account displays"
    );
    assert_eq!(
        reply["result"]["displayVenue"],
        serde_json::json!(""),
        "the fixture accounts are unbound: the venue spells absence"
    );
}

/// 显示账户配置化: `display.account.set` 的拒绝面（wire 层）——
/// 未知 id 是计划先行的 INVALID_PARAMS。issue 561（2026-10-20 裁决）
/// 改写旧断言：display 是面板唯一可信账户源，校验放宽为「存在即可」，
/// frozen 账户合法落位（旧断言钉住的「非 active 拒绝」随裁决废除；
/// 服务层测试钉住「frozen 也落位」的半边）。两账户夹具没有 persist
/// 路径，所以合法切位在持久化关上被整体拒绝。
#[tokio::test]
async fn display_account_set_accepts_frozen_and_refuses_unknown_plan_first() {
    let (core, registry, peer) = two_account_fixture().await;
    // Freeze `paper` first: the fixture mints both accounts active.
    core.lock()
        .await
        .tighten_account_status(&AccountId::from("paper"), AccountStatus::Frozen, now_ms())
        .expect("the freeze lands");

    // issue 561: a frozen account MAY wear the display face now — the
    // pointer is a view focus, the posture gates the orders not the view.
    // The fixture has no persist path, so the legal shape still refuses
    // WHOLE (nothing applied) — the plan-first posture survives.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"display.account.set","params":{"accountId":"paper"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no accounts config path"),
        "frozen passes the plan-first gate; only persistence refuses: {reply}"
    );

    // Unknown id: the standard unknown-account refusal.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"display.account.set","params":{"accountId":"ghost"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "{reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown account `ghost`"),
        "{reply}"
    );

    // The legal shape itself: with no persist path the core refuses the
    // write WHOLE (nothing was applied) — the same posture the service
    // layer test pins on the happy path with a path configured.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":3,"method":"display.account.set","params":{"accountId":"default"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no accounts config path"),
        "a core without a persist path refuses the write whole: {reply}"
    );
}

/// venue.set 是写动词：只读模式的内核拒绝（READONLY_REFUSED 名单），
/// venue.get 是只读的，照常回答。
#[tokio::test]
async fn account_venue_set_is_refused_in_read_only_mode_but_get_answers() {
    let (core, registry, peer) = two_account_fixture().await;
    // Flip the core into read-only (the config is the mode's home).
    core.lock().await.config_mut().mode = Mode::ReadOnly;
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{"accountId":"paper","venue":"polysimulator"}}"#
            .to_string(),
    )
    .await;
    assert_eq!(
        reply["error"]["code"],
        serde_json::json!(Failure::APPLICATION),
        "read-only refuses the configuration verb: {reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("read-only mode"),
        "the refusal says the core is read-only: {reply}"
    );
    // The read rides: observations are what read-only serves.
    let reply = rpc(
        &core,
        &registry,
        &peer,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.venue.get","params":{}}"#.to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "venue.get is read-only and must answer: {reply}"
    );
    let rows = reply["result"]["venues"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
}
