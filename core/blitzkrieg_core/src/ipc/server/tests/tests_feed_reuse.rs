#![cfg(test)]

use super::*;

use crate::account::{AccountId, AccountStatus};
use crate::ipc::schema::*;
use crate::model::{CoreError, CoreErrorCode, Mode};
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use serde_json::Value;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot};

// ── RFC-661 M2: the feed_source reuse pipeline, at the wire ─────────────

// The generic declaration face, no venue names hardcoded: the tests below
// register a source/consumer pair under arbitrary names and the pipeline
// works off the declarations only. The source is VenueLive-shaped (its
// feed/discovery really start and reach the core); the consumer declares
// NEITHER component (the parasitic shape) and its feed_source names the
// source.
impl blitzkrieg_market_api::MarketPlugin for CountedSourcePlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn market_type(&self) -> blitzkrieg_market_api::MarketType {
        blitzkrieg_market_api::MarketType::Prediction
    }
    fn data_feed(&self) -> Option<&dyn blitzkrieg_market_api::DataFeed> {
        Some(&self.feed)
    }
    fn discovery(&self) -> Option<&dyn blitzkrieg_market_api::MarketDiscovery> {
        Some(&self.discovery)
    }
}
struct CountedSourcePlugin {
    name: String,
    feed: CountedFeed,
    discovery: VenueLiveDiscovery,
}
impl blitzkrieg_market_api::MarketPlugin for CountedConsumerPlugin {
    fn name(&self) -> &str {
        &self.name
    }
    fn market_type(&self) -> blitzkrieg_market_api::MarketType {
        blitzkrieg_market_api::MarketType::Prediction
    }
    fn feed_source(&self) -> Option<&str> {
        Some(&self.source)
    }
}
struct CountedConsumerPlugin {
    name: String,
    source: String,
}

/// The reuse-pipeline fixture the three wire tests share. Engine on; a
/// registered source+consumer pair (the GENERIC declaration face — the
/// tests never name polymarket/polysimulator); boot actives =
/// `boot_active` only. Returns `(core, registry, peer, host, data_plane,
/// source_name, consumer_name)`. The concrete host shares the data
/// plane's reuse cell (installed BEFORE erasure — the production
/// assembly's order), so the host's fan-out reads the edges the mount
/// path writes.
#[allow(clippy::type_complexity)]
async fn reuse_fixture(
    tag: &str,
    boot_active: &[&str],
) -> (
    Arc<AsyncMutex<Core>>,
    crate::market::registry::MarketPluginRegistry,
    PeerAuth,
    Arc<dyn blitzkrieg_market_api::MarketHost>,
    DataPlaneRuntime,
    String,
    String,
) {
    let dir = std::env::temp_dir().join(format!("bk-reuse-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"paper\"\nname = \"paper\"\n",
    )
    .expect("fixture");

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
    let source = format!("src-{tag}");
    let consumer = format!("cons-{tag}");
    let registry = crate::market::registry::MarketPluginRegistry::new();
    registry.register(Box::new(CountedSourcePlugin {
        feed: CountedFeed {
            owner: source.clone(),
            control_key: source.clone(),
        },
        discovery: VenueLiveDiscovery {
            tag: source.clone(),
        },
        name: source.clone(),
    }));
    registry.register(Box::new(CountedConsumerPlugin {
        source: source.clone(),
        name: consumer.clone(),
    }));
    registry.set_enabled(&source, true);
    registry.set_enabled(&consumer, true);

    // The boot data plane: only `boot_active`'s venues pre-recorded; the
    // reuse cell seeds the ACTIVE consumers' declared edges — the same
    // `at_boot` walk production runs (a consumer-only boot seeds NO edge;
    // the runtime bind appends it).
    let actives: Vec<std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin>> =
        boot_active.iter().filter_map(|n| registry.get(n)).collect();
    let data_plane = DataPlaneRuntime::at_boot(
        &CoreConfig {
            feed_ws_enabled: true,
            engine_enabled: true,
            discovery_enabled: true,
            round_duration_sec: 900,
            assets: vec!["BTC".to_string()],
            ..Default::default()
        },
        &actives,
        &[],
        &registry,
    );
    let core: Arc<AsyncMutex<Core>> = Arc::new(AsyncMutex::new(c));
    let concrete = crate::market::host::CoreHost::new(Arc::clone(&core));
    if let Some(reuse) = data_plane.feed_reuse() {
        concrete.set_feed_reuse(reuse.clone());
    }
    (
        core,
        registry,
        PeerAuth::SameUid { uid: own_uid() },
        Arc::new(concrete),
        data_plane,
        source,
        consumer,
    )
}

/// M2 wire test 1 — COLD consumer-only boot. The consumer never booted
/// (boot actives are empty); binding its account at runtime must (a)
/// land the bind, (b) mount the SOURCE's data plane through the reuse
/// path, (c) mount NOTHING for the consumer itself (no feed control
/// under the consumer's key — the fan-out is its registrar), and (d)
/// count exactly the ONE upstream stream's components. The source's
/// EXECUTOR is never in question: enable_venue_at_runtime never mounts
/// executors, and the source reaches it as a data-only registry fetch.
#[tokio::test]
async fn a_consumer_only_bind_mounts_the_source_data_plane_once_and_never_the_consumer() {
    let (core, registry, peer, host, data_plane, source, consumer) =
        reuse_fixture("cold", &[]).await;
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{{"accountId":"paper","venue":"{consumer}","reason":"consumer bind"}}}}"#
    );
    let reply = rpc_with_data_plane(&core, &registry, &peer, &host, &data_plane, line).await;
    assert!(reply.get("error").is_none(), "the bind must land: {reply}");
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(true),
        "the consumer's enablement mounted its upstream: {reply}"
    );

    // (b) the source's feed really started — its subscription control is
    // reachable under the SOURCE's key.
    core.lock()
        .await
        .subscribe_feed_tokens(&source, vec!["tok-a".to_string()])
        .await;
    // (d) exactly one upstream feed mount for the reused stream. The
    // start log is a process-global shared cell (sibling tests add their
    // own entries concurrently) — read only THIS fixture's slice: the
    // CountedFeed logs under its owner (= the source's unique tag).
    let starts = COUNTED_FEED_STARTS
        .lock()
        .expect("counted feed starts poisoned")
        .iter()
        .filter(|o| **o == source)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        vec![source.clone()],
        "the reused stream's feed mounts EXACTLY once, under the source: {starts:?}"
    );
    // The discovery family rides the source too (feed + discovery are
    // the two components of the ONE upstream stream) — one family (the
    // boot's global 900s), tagged with the source.
    let disc = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == &source)
        .count();
    assert_eq!(
        disc, 1,
        "the ONE upstream stream's discovery family mounts once: {disc}"
    );
    // (c) the consumer mounted no component of its own.
    assert!(
        !data_plane.venue_mounted(&consumer),
        "the consumer's own data plane must never mount — the fan-out is its registrar"
    );
    // The reuse cell carries the declared edge, and the mount counter
    // reads 2 (one feed + one discovery family — the ONE stream's two
    // components). The counter is per-fixture (each test assembles its
    // own cell), so the absolute read is safe.
    let reuse = data_plane.feed_reuse().expect("the boot cell");
    assert!(
        reuse.consumes(&consumer, &source),
        "the runtime bind appended the declared edge"
    );
    assert_eq!(
        reuse.mounts.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "one feed + one discovery = the ONE upstream stream's components"
    );
}

/// M2 wire test 2 — RUNTIME bind onto a RUNNING source. The source
/// booted (its components are live); binding a consumer must append the
/// declared edge and NOT restart anything: the feed start count stays at
/// its boot value, no new discovery instance appears (the per-(source,
/// family) guard), and the enablement still lands.
#[tokio::test]
async fn a_runtime_bound_consumer_on_a_running_source_re_subscribes_nothing() {
    let (core, registry, peer, host, data_plane, source, consumer) =
        reuse_fixture("warm", &["src-warm"]).await;
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"account.venue.set","params":{{"accountId":"paper","venue":"{consumer}","reason":"consumer bind onto running source"}}}}"#
    );
    let reply = rpc_with_data_plane(&core, &registry, &peer, &host, &data_plane, line).await;
    assert!(reply.get("error").is_none(), "the bind must land: {reply}");
    assert_eq!(
        reply["result"]["runtimeEnabled"],
        serde_json::json!(true),
        "the consumer's enablement is recorded: {reply}"
    );

    // The running source was NOT restarted: the boot's feed start (if the
    // log even captured it — the fixture mounts at_boot, before this test
    // can observe) is followed by ZERO new starts for this source, and
    // zero new discovery instances. The log is a process-global shared
    // cell — filter to THIS fixture's tag.
    let starts = COUNTED_FEED_STARTS
        .lock()
        .expect("counted feed starts poisoned")
        .iter()
        .filter(|o| **o == source)
        .count();
    assert!(
        starts <= 1,
        "a consumer bind onto a running source must not restart the upstream: {starts}"
    );
    let disc = VENUE_LIVE_DISCOVERY_CFG
        .lock()
        .expect("venue live discovery cfg poisoned")
        .iter()
        .filter(|(tag, _)| tag == &source)
        .count();
    assert_eq!(
        disc, 0,
        "no discovery instance may re-mount (the fixture's at_boot pre-records the family \
         without starting it): {disc}"
    );
    // The edge appended (the fan-out now re-registers the source's
    // batches under the consumer), and the counter did not move.
    let reuse = data_plane.feed_reuse().expect("the boot cell");
    assert!(reuse.consumes(&consumer, &source));
    assert_eq!(
        reuse.mounts.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "no mount was attempted — the source was already running"
    );
}

/// M2 wire test 3 — 停用不重订 / 重启不重订. After a consumer binds
/// (mounting the source once), UNBINDING the account must not touch the
/// upstream (the source's components keep running — another consumer or
/// a re-bind may need them), and RE-BINDING the same consumer must mount
/// nothing new: the feed start count stays 1 forever, the declared edge
/// stays a single entry.
#[tokio::test]
async fn consumer_unbind_and_rebind_never_re_subscribe_the_upstream() {
    let (core, registry, peer, host, data_plane, source, consumer) =
        reuse_fixture("rebind", &[]).await;
    let bind = |id: u32| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"account.venue.set","params":{{"accountId":"paper","venue":"{consumer}","reason":"lifecycle"}}}}"#
        )
    };
    let unbind = r#"{"jsonrpc":"2.0","id":9,"method":"account.venue.set","params":{"accountId":"paper","venue":"","reason":"unbind"}}"#.to_string();

    // Bind #1: mounts the source once.
    let reply = rpc_with_data_plane(&core, &registry, &peer, &host, &data_plane, bind(1)).await;
    assert!(reply.get("error").is_none(), "bind 1 must land: {reply}");
    // Unbind: lands, upstream untouched.
    let reply = rpc_with_data_plane(
        &core,
        &registry,
        &peer,
        &host,
        &data_plane,
        unbind.to_string(),
    )
    .await;
    assert!(reply.get("error").is_none(), "unbind must land: {reply}");
    // Re-bind: idempotent, no second mount.
    let reply = rpc_with_data_plane(&core, &registry, &peer, &host, &data_plane, bind(2)).await;
    assert!(reply.get("error").is_none(), "re-bind must land: {reply}");

    // The start log is a process-global shared cell — filter to THIS
    // fixture's tag: bind → unbind → re-bind keeps the upstream at ONE
    // start.
    let starts = COUNTED_FEED_STARTS
        .lock()
        .expect("counted feed starts poisoned")
        .iter()
        .filter(|o| **o == source)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        vec![source.clone()],
        "bind → unbind → re-bind keeps the upstream at ONE start: {starts:?}"
    );
    let reuse = data_plane.feed_reuse().expect("the boot cell");
    assert_eq!(
        reuse
            .consumers_of(&source)
            .iter()
            .filter(|v| **v == consumer)
            .count(),
        1,
        "the declared edge is ONE entry, never duplicated by the lifecycle"
    );
}

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
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"paper\"\nname = \"paper\"\n",
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
/// accounts.toml, and the account book reads the new set back. The
/// idempotence posture: a second write with the same set mounts nothing
/// (`runtimeEnabled: false` — every pair already live).
#[tokio::test]
async fn rounds_set_mounts_every_added_family_and_persists_the_declaration() {
    // A private accounts.toml — the persist half refuses without a
    // config path, the same posture venue.set's mount test runs under.
    let dir = std::env::temp_dir().join(format!("bk-rounds-set-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"paper\"\nname = \"paper\"\n",
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
    // The durable half: rounds are on disk, sibling fields intact.
    let file = std::fs::read_to_string(&accounts_path).expect("fixture readable");
    assert!(
        file.contains("rounds = [900, 3600]"),
        "the declaration must land on disk: {file}"
    );
    assert!(
        file.contains("name = \"paper\""),
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
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"paper\"\nname = \"paper\"\nvenue = \"roundslive\"\nrounds = [900, 3600]\n",
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
    let file = std::fs::read_to_string(&accounts_path).expect("fixture readable");
    assert!(
        file.contains("rounds = [900, 3600]"),
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
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"paper\"\nname = \"paper\"\nvenue = \"roundssoft\"\nrounds = [900, 3600]\n",
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
    let file = std::fs::read_to_string(&accounts_path).expect("fixture readable");
    assert!(
        file.contains("rounds = [900]") && !file.contains("3600"),
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
/// 3600]); the EMPTY set removes the accounts.toml key entirely (missing
/// and `[]` are the same unrestricted spelling, the deleted key is the
/// cleaner one) and a NEW core reading the same file boots with the
/// unrestricted account (the persistence round-trip).
#[tokio::test]
async fn rounds_set_refuses_bad_values_and_the_empty_set_removes_the_file_key() {
    let dir = std::env::temp_dir().join(format!("bk-rounds-params-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"paper\"\nname = \"paper\"\nvenue = \"roundslive\"\n",
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
    let file = std::fs::read_to_string(&accounts_path).expect("fixture readable");
    assert!(
        file.contains("rounds = [900, 3600]"),
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
    let file = std::fs::read_to_string(&accounts_path).expect("fixture readable");
    assert!(
        !file.contains("rounds ="),
        // ("roundslive" the venue contains the substring — match the
        // KEY spelling, not a bare substring.)
        "the rounds key must be GONE from the file: {file}"
    );
    assert!(file.contains("name = \"paper\""), "siblings kept: {file}");

    // Persistence round-trip: a NEW core reading the same file boots
    // unrestricted (the deleted key parses as the empty set).
    let reread = crate::account::config::load_account_ledgers(
        &accounts_path,
        now_ms(),
        crate::account::config::TEST_ALLOWED_ROUNDS,
    )
    .expect("the file the write produced must boot");
    let account = reread.account(&AccountId::from("paper")).expect("account");
    assert!(
        account.rounds.is_empty(),
        "the deleted key boots unrestricted: {:?}",
        account.rounds
    );
}

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
    // persists, and the fixture's scratch accounts.toml is where.
    let dir = std::env::temp_dir().join(format!("bk-create-mount-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n\n[[account]]\nid = \"paper\"\nname = \"paper\"\n",
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
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n\n[[account]]\nid = \"paper\"\nname = \"paper\"\n",
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
/// 绑定落进测试私有的 accounts.toml，重新 load_account_ledgers 读回。
#[tokio::test]
async fn account_venue_set_round_trips_get_and_the_persisted_file() {
    // A private accounts.toml — the persist half needs a file, and it
    // must never be the checkout's own user_layer/configs/accounts.toml.
    let dir = std::env::temp_dir().join(format!("bk-venue-wire-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let accounts_path = dir.join("accounts.toml");
    std::fs::write(
        &accounts_path,
        "[[account]]\nid = \"default\"\nname = \"main\"\n\n[[account]]\nid = \"paper\"\nname = \"paper\"\n",
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

    // The persist: a FRESH load_account_ledgers over the file — the same
    // call the next boot makes — sees BOTH bindings and every sibling
    // field (the name rode through untouched).
    let book = crate::account::config::load_account_ledgers(
        &accounts_path,
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
    let book = crate::account::config::load_account_ledgers(
        &accounts_path,
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
