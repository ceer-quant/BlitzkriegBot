use super::*;

/// B1d (轮次多族): the runtime mount walks EVERY declared family — a
/// bind whose account declares `rounds = [900, 3600]` mounts the
/// global 900 family AND a 3600 instance of the same venue, each with
/// its own duration baked into the discovery config (the plugin picks
/// its series/slug from that field). The per-pair idempotence guard: a
/// second venue.set with every family already mounted mounts nothing
/// (runtimeEnabled=false, feed untouched).
#[tokio::test]
async fn venue_set_mounts_every_declared_family_of_the_account() {
    let dir = std::env::temp_dir().join(format!("bk-family-mount-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=paper\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");

    let (core, registry, peer) = {
        let now = now_ms();
        let book = crate::account::AccountLedgers::new(
            vec![crate::account::Account {
                id: AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: String::new(),
                trading_enabled: true,
                strategies: Vec::new(),
                // The declaration under test: two families on one venue.
                rounds: vec![900, 3600],
                updated_at_ms: now,
            }],
            AccountId::from("paper"),
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
                tag: "familylive".to_string(),
            },
            name: "familylive".to_string(),
        }));
        (
            Arc::new(AsyncMutex::new(c)),
            registry,
            PeerAuth::SameUid { uid: own_uid() },
        )
    };
    // Boot shape: venuelive never mounted; the boot's global family (900)
    // sits on the polymarket slot only.
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
    VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .retain(|(tag, _)| tag != "familylive");

    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{"accountId":"paper","venue":"familylive","reason":"family mount"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "the bind must land: {reply}");
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(true),
        "{reply}"
    );
    // BOTH families mounted: the boot's global 900 AND the declared
    // 3600 — each pair recorded in the book, each with its own
    // discovery instance (two starts, two distinct durations).
    {
        let book = data_plane.discovery_mounted.lock().unwrap();
        assert!(book.contains(&("familylive".to_string(), 900)), "{book:?}");
        assert!(book.contains(&("familylive".to_string(), 3600)), "{book:?}");
    }
    let durations: Vec<i64> = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == "familylive")
        .map(|(_, cfg)| cfg.round_duration_sec)
        .collect();
    assert!(
        durations.contains(&900) && durations.contains(&3600),
        "the mount started one discovery per family, each with its own \
         duration: {durations:?}"
    );

    // Idempotence per pair: a second venue.set with everything mounted
    // mounts nothing.
    let mark = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == "familylive")
        .count();
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.venue.set","params":{"accountId":"paper","venue":"familylive","reason":"idempotence"}}"#
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
        "every declared family already mounted: {reply}"
    );
    assert_eq!(
        VENUE_LIVE_DISCOVERY_CFG
            .lock()
            .expect("venue live discovery cfg poisoned")
            .iter()
            .filter(|(tag, _)| tag == "familylive")
            .count(),
        mark,
        "the idempotent re-bind started no discovery"
    );
}

/// B2 (轮次多族): `account.rounds.set` at the wire — the ADD half. A set
/// of two families on a bound account mounts BOTH (venue, duration)
/// discovery instances at runtime (增族即挂), reports
/// `runtimeEnabled: true`, persists the rounds into the private
/// .env, and the account book reads the new set back. The
/// idempotence posture: a second write with the same set mounts nothing
/// (`runtimeEnabled: false` — every pair already live).
#[tokio::test]
async fn rounds_set_mounts_every_added_family_and_persists_the_declaration() {
    // A private .env — the persist half refuses while a legacy toml sits
    // beside it, the same posture venue.set's mount test runs under.
    let dir = std::env::temp_dir().join(format!("bk-rounds-set-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=paper\nACCOUNT_PAPER_NAME=\"paper\"\n",
    )
    .expect("fixture");

    let (core, registry, peer) = {
        let now = now_ms();
        let book = crate::account::AccountLedgers::new(
            vec![crate::account::Account {
                id: AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                // Bound to the plugin under test: added families mount
                // on THIS venue.
                venue: "roundsmount".to_string(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: now,
            }],
            AccountId::from("paper"),
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
                tag: "roundsmount".to_string(),
            },
            name: "roundsmount".to_string(),
        }));
        (
            Arc::new(AsyncMutex::new(c)),
            registry,
            PeerAuth::SameUid { uid: own_uid() },
        )
    };
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    // Boot shape: roundslive never mounted (the whole point: the write
    // must mount the ADDED families itself).
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
    VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .retain(|(tag, _)| tag != "roundsmount");

    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.rounds.set","params":{"accountId":"paper","rounds":[900,3600],"reason":"family add"}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "the write must land: {reply}");
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(true),
        "added families must mount at runtime: {reply}"
    );
    assert_eq!(
        reply["result"]["rounds"],
        serde_json::json!([900, 3600]),
        "the reply echoes the set in force: {reply}"
    );
    assert_eq!(
        reply["result"]["persisted"],
        serde_json::json!(true),
        "{reply}"
    );
    // BOTH families mounted with their own discovery instance.
    {
        let book = data_plane.discovery_mounted.lock().unwrap();
        assert!(book.contains(&("roundsmount".to_string(), 900)), "{book:?}");
        assert!(
            book.contains(&("roundsmount".to_string(), 3600)),
            "{book:?}"
        );
    }
    let durations: Vec<i64> = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == "roundsmount")
        .map(|(_, cfg)| cfg.round_duration_sec)
        .collect();
    assert!(
        durations.contains(&900) && durations.contains(&3600),
        "the mount started one discovery per added family: {durations:?}"
    );
    // The durable half: rounds are on disk in .env, sibling fields intact.
    let file = std::fs::read_to_string(&env_path).expect("fixture readable");
    assert!(
        file.contains("ACCOUNT_PAPER_ROUNDS=\"900,3600\""),
        "the declaration must land on disk: {file}"
    );
    assert!(
        file.contains("ACCOUNT_PAPER_NAME=\"paper\""),
        "sibling field kept: {file}"
    );
    // And the in-memory book reads the new set.
    {
        let c = core.lock().await;
        let account = c
            .accounts()
            .account(&AccountId::from("paper"))
            .expect("account");
        assert_eq!(account.rounds, vec![900, 3600], "memory moved too");
    }

    // Idempotence: a second write with the SAME set mounts nothing.
    let mark = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == "roundsmount")
        .count();
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.rounds.set","params":{"accountId":"paper","rounds":[3600,900]}}"#
            .to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "re-set must succeed: {reply}");
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(false),
        "every family already mounted: {reply}"
    );
    assert_eq!(
        VENUE_LIVE_DISCOVERY_CFG
            .lock()
            .expect("venue live discovery cfg poisoned")
            .iter()
            .filter(|(tag, _)| tag == "roundsmount")
            .count(),
        mark,
        "the idempotent re-set started no discovery"
    );
}

/// B2 (轮次多族): the REMOVAL guard — a set that would remove a family
/// while the account holds an open position in the removed families'
/// venues is refused WHOLE (INVALID_PARAMS naming the removed families),
/// and NEITHER store moves: the file keeps the old declaration, the
/// in-memory book keeps the old set, no discovery restarted.
#[tokio::test]
async fn rounds_set_refuses_a_removal_with_open_exposure_and_moves_nothing() {
    let dir = std::env::temp_dir().join(format!("bk-rounds-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=paper\nACCOUNT_PAPER_NAME=\"paper\"\nACCOUNT_PAPER_VENUE=\"roundslive\"\nACCOUNT_PAPER_ROUNDS=\"900,3600\"\n",
    )
    .expect("fixture");

    let (core, registry, peer) = {
        let now = now_ms();
        let book = crate::account::AccountLedgers::new(
            vec![crate::account::Account {
                id: AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "roundslive".to_string(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: vec![900, 3600],
                updated_at_ms: now,
            }],
            AccountId::from("paper"),
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
        // The OPEN position that blocks the removal: an entry admitted
        // under the 3600 family (the same way the account.remove guard
        // test fabricates its open book row).
        c.positions_mut().open(
            crate::position::OpenParams {
                strategy: "spread_arb".into(),
                asset: "BTC".into(),
                direction: crate::model::SignalDirection::Up,
                token_id: "tok-up".into(),
                condition_id: "cond".into(),
                account_id: AccountId::from("paper"),
                venue: "roundslive".to_string(),
                partner_token_id: String::new(),
                entry_price: rust_decimal_macros::dec!(0.48),
                expires_at_ms: now_ms() + 3_600_000,
                was_maker: true,
                target_exit_price: None,
                round_duration_sec: 3600,
                round_slot: 0,
            },
            now_ms(),
        );
        let registry = crate::market::registry::MarketPluginRegistry::new();
        registry.register(Box::new(VenueLivePlugin {
            discovery: VenueLiveDiscovery {
                tag: "roundslive".to_string(),
            },
            name: "roundslive".to_string(),
        }));
        (
            Arc::new(AsyncMutex::new(c)),
            registry,
            PeerAuth::SameUid { uid: own_uid() },
        )
    };
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> =
        Arc::new(crate::market::host::CoreHost::new(core.clone()));
    let mut mounted = std::collections::BTreeSet::new();
    mounted.insert(("roundslive".to_string(), 900));
    mounted.insert(("roundslive".to_string(), 3600));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: true,
        discovery_enabled: true,
        discovery_assets: vec!["BTC".to_string()],
        discovery_round_duration_sec: 900,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(mounted)),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };
    VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .retain(|(tag, _)| tag != "roundslive");

    // And nothing mounted (the guard fired before any runtime half).
    // A DELTA of zero, not absolute absence: the discovery config
    // register is a process-wide shared cell, sibling tests legitimately
    // add their own entries concurrently — what THIS refusal must pin
    // is that it added none.
    let before = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == "roundslive")
        .count();
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.rounds.set","params":{"accountId":"paper","rounds":[900]}}"#
            .to_string(),
    )
    .await;
    // The venue.set envelope: APPLICATION carrying coreCode
    // INVALID_PARAMS — a client fault at heart, typed through the
    // arm's CoreError tail.
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "the removal must be refused with INVALID_PARAMS: {reply}"
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("[3600]"),
        "the refusal must name the removed family: {reply}"
    );
    // NEITHER store moved: the file keeps both families, memory too.
    let file = std::fs::read_to_string(&env_path).expect("fixture readable");
    assert!(
        file.contains("ACCOUNT_PAPER_ROUNDS=\"900,3600\""),
        "the file must be untouched: {file}"
    );
    {
        let c = core.lock().await;
        let account = c
            .accounts()
            .account(&AccountId::from("paper"))
            .expect("account");
        assert_eq!(account.rounds, vec![900, 3600], "memory must be untouched");
    }
    // And nothing mounted (the guard fired before any runtime half):
    // the tag's entry count is exactly what it was before the refused
    // write — the guard's move-nothing guarantee.
    assert_eq!(
        VENUE_LIVE_DISCOVERY_CFG
            .lock()
            .expect("venue live discovery cfg poisoned")
            .iter()
            .filter(|(tag, _)| tag == "roundslive")
            .count(),
        before,
        "the refused write started no discovery"
    );
}

/// B2 (轮次多族): the CLEAN removal — no exposure in the removed family —
/// lands on both stores, does NOT unmount the running discovery instance
/// (软卸: `discovery_mounted_at` stays true) and the poll-output gate
/// drops the soft-unloaded family's next batch, bumping the
/// `soft_unloaded_polling_instances` gauge.
#[tokio::test]
async fn rounds_set_soft_unloads_a_clean_removal_and_the_poll_gate_drops_it() {
    let dir = std::env::temp_dir().join(format!("bk-rounds-soft-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=paper\nACCOUNT_PAPER_NAME=\"paper\"\nACCOUNT_PAPER_VENUE=\"roundssoft\"\nACCOUNT_PAPER_ROUNDS=\"900,3600\"\n",
    )
    .expect("fixture");

    let (core, registry, peer) = {
        let now = now_ms();
        let book = crate::account::AccountLedgers::new(
            vec![crate::account::Account {
                id: AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "roundssoft".to_string(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: vec![900, 3600],
                updated_at_ms: now,
            }],
            AccountId::from("paper"),
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
                tag: "roundssoft".to_string(),
            },
            name: "roundssoft".to_string(),
        }));
        (
            Arc::new(AsyncMutex::new(c)),
            registry,
            PeerAuth::SameUid { uid: own_uid() },
        )
    };
    // The SHARED concrete host — the same shape the assembly builds. The
    // gate under test is `admit_poll_batch` itself (the assembly's
    // closure wraps exactly this call): asserting its verdicts directly
    // exercises the poll side without rebuilding the closure a second
    // way.
    let core_host = crate::market::host::CoreHost::new(core.clone());
    core_host.set_active_plugin("roundssoft");
    let host: Arc<dyn blitzkrieg_market_api::MarketHost> = Arc::new(core_host);
    let mut mounted = std::collections::BTreeSet::new();
    mounted.insert(("roundssoft".to_string(), 900));
    mounted.insert(("roundssoft".to_string(), 3600));
    let data_plane = DataPlaneRuntime {
        feed_ws_enabled: true,
        discovery_enabled: true,
        discovery_assets: vec!["BTC".to_string()],
        discovery_round_duration_sec: 900,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(mounted)),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };
    VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .retain(|(tag, _)| tag != "roundssoft");
    // A probe batch the 3600 family's instance would emit (its own slot
    // grid: the span derived from slot×expiry reproduces 3600s).
    let now = now_ms();
    let slot3600 = now / 1000 / 3600;
    let batch_3600 = vec![blitzkrieg_market_api::MarketDescriptor {
        asset: "BTC".to_string(),
        condition_id: "live-cond-3600".to_string(),
        question_id: "live-q-3600".to_string(),
        up_token_id: "live-up-3600".to_string(),
        down_token_id: "live-down-3600".to_string(),
        up_price: rust_decimal_macros::dec!(0.55),
        down_price: rust_decimal_macros::dec!(0.45),
        expires_at_ms: (slot3600 + 1) * 3600 * 1000,
        round_slot: slot3600,
        venue: "roundssoft".to_string(),
        neg_risk: false,
        question: "soft-unload probe".to_string(),
    }];
    // Pre-write: the 3600 family is ACTIVE (an account declares it) —
    // the batch must be admitted.
    assert!(
        data_plane.admit_poll_batch(&core, &batch_3600).await,
        "an active family's poll batch must be admitted"
    );

    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.rounds.set","params":{"accountId":"paper","rounds":[900],"reason":"family shrink"}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the shrink must land: {reply}"
    );
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(false),
        "a pure removal mounts nothing: {reply}"
    );
    // Both stores moved.
    let file = std::fs::read_to_string(&env_path).expect("fixture readable");
    assert!(
        file.contains("ACCOUNT_PAPER_ROUNDS=\"900\"") && !file.contains("3600"),
        "the file must carry the shrunk set: {file}"
    );
    {
        let c = core.lock().await;
        let account = c
            .accounts()
            .account(&AccountId::from("paper"))
            .expect("account");
        assert_eq!(account.rounds, vec![900], "memory moved too");
    }
    // 软卸 does NOT unmount: the instance book still holds the pair.
    assert!(
        data_plane.discovery_mounted_at("roundssoft", 3600),
        "the soft-unload must leave the running instance mounted"
    );
    // The poll side: the same batch is now DROPPED, gauge bumps.
    assert!(
        !data_plane.admit_poll_batch(&core, &batch_3600).await,
        "the soft-unloaded family's poll batch must be dropped"
    );
    assert_eq!(
        data_plane.soft_unloaded_count(),
        1,
        "the gauge counts the dropped batch"
    );
    // The ACTIVE family (900) still admits.
    let slot900 = now / 1000 / 900;
    let batch_900 = vec![blitzkrieg_market_api::MarketDescriptor {
        asset: "BTC".to_string(),
        condition_id: "live-cond-900".to_string(),
        question_id: "live-q-900".to_string(),
        up_token_id: "live-up-900".to_string(),
        down_token_id: "live-down-900".to_string(),
        up_price: rust_decimal_macros::dec!(0.55),
        down_price: rust_decimal_macros::dec!(0.45),
        expires_at_ms: (slot900 + 1) * 900 * 1000,
        round_slot: slot900,
        venue: "roundssoft".to_string(),
        neg_risk: false,
        question: "active probe".to_string(),
    }];
    assert!(
        data_plane.admit_poll_batch(&core, &batch_900).await,
        "the still-active family's batch must keep flowing"
    );
    assert_eq!(data_plane.soft_unloaded_count(), 1, "no double count");
}

/// B2 (轮次多族): the parametric refusals — a union-foreign duration
/// refuses before anything is computed; an unknown account refuses with
/// the standard unknown-account error; a duplicate-carrying list is
/// deduplicated with first-seen order kept (900, 3600, 900 → [900,
/// 3600]); the EMPTY set removes the ACCOUNT_*_ROUNDS key entirely (missing
/// and `[]` are the same unrestricted spelling, the deleted key is the
/// cleaner one) and a NEW core reading the same file boots with the
/// unrestricted account (the persistence round-trip).
#[tokio::test]
async fn rounds_set_refuses_bad_values_and_the_empty_set_removes_the_file_key() {
    let dir = std::env::temp_dir().join(format!("bk-rounds-params-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=paper\nACCOUNT_PAPER_NAME=\"paper\"\nACCOUNT_PAPER_VENUE=\"roundslive\"\n",
    )
    .expect("fixture");

    let (core, registry, peer) = {
        let now = now_ms();
        let book = crate::account::AccountLedgers::new(
            vec![crate::account::Account {
                id: AccountId::from("paper"),
                name: "paper".into(),
                market_type: blitzkrieg_market_api::MarketType::Prediction,
                status: AccountStatus::Active,
                credential_keys: crate::account::CredentialKeys::default(),
                venue: "roundslive".to_string(),
                trading_enabled: true,
                strategies: Vec::new(),
                rounds: Vec::new(),
                updated_at_ms: now,
            }],
            AccountId::from("paper"),
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
        registry.register(Box::new(VenueStubPlugin {
            name: "roundslive".to_string(),
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
        feed_ws_enabled: false,
        discovery_enabled: false,
        discovery_assets: Vec::new(),
        discovery_round_duration_sec: 900,
        discovery_mounted: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        soft_unloaded_polling_instances: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            0,
        )),
        feed_reuse: None,
    };

    // Whitelist-foreign duration: refused before anything moved.
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":1,"method":"account.rounds.set","params":{"accountId":"paper","rounds":[9000]}}"#
            .to_string(),
    )
    .await;
    // The venue.set envelope: APPLICATION carrying coreCode
    // INVALID_PARAMS — a client fault at heart, typed through the
    // arm's CoreError tail.
    assert_eq!(
        reply["error"]["data"]["coreCode"],
        serde_json::json!("INVALID_PARAMS"),
        "a foreign duration must refuse: {reply}"
    );
    // Unknown account: the standard unknown-account refusal.
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":2,"method":"account.rounds.set","params":{"accountId":"ghost","rounds":[900]}}"#
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
            .contains("unknown account"),
        "the standard unknown-account spelling: {reply}"
    );
    // Dedup with first-seen order kept: [900, 3600, 900] → [900, 3600].
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":3,"method":"account.rounds.set","params":{"accountId":"paper","rounds":[900,3600,900]}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the dedup write must land: {reply}"
    );
    assert_eq!(
        reply["result"]["rounds"],
        serde_json::json!([900, 3600]),
        "duplicates dropped, first-seen order kept: {reply}"
    );
    let file = std::fs::read_to_string(&env_path).expect("fixture readable");
    assert!(
        file.contains("ACCOUNT_PAPER_ROUNDS=\"900,3600\""),
        "the deduplicated set is what lands on disk: {file}"
    );

    // The EMPTY set: the key is REMOVED from the file (missing == []).
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        r#"{"jsonrpc":"2.0","id":4,"method":"account.rounds.set","params":{"accountId":"paper","rounds":[]}}"#
            .to_string(),
    )
    .await;
    assert!(
        reply.get("error").is_none(),
        "the empty set is the legal unrestricted write: {reply}"
    );
    assert_eq!(reply["result"]["rounds"], serde_json::json!([]), "{reply}");
    let file = std::fs::read_to_string(&env_path).expect("fixture readable");
    assert!(
        !file.contains("_ROUNDS"),
        // ("roundslive" the venue contains the substring — match the
        // KEY spelling, not a bare substring.)
        "the rounds key must be GONE from the file: {file}"
    );
    assert!(
        file.contains("ACCOUNT_PAPER_NAME=\"paper\""),
        "siblings kept: {file}"
    );

    // Persistence round-trip: a NEW core reading the same sources boots
    // unrestricted (the deleted key parses as the empty set).
    let reread = crate::account::config::load_account_ledgers_single_source(
        &accounts_path,
        Some(&env_path),
        now_ms(),
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .expect("the file the write produced must boot");
    let account = reread.account(&AccountId::from("paper")).expect("account");
    assert!(
        account.rounds.is_empty(),
        "the deleted key boots unrestricted"
    );
}
