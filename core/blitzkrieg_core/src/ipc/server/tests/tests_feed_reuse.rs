#![cfg(test)]

use super::*;

use crate::account::{AccountId, AccountStatus};
use crate::model::Mode;
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

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
    // issue 751: the persist half writes .env (the writers refuse while a
    // legacy toml sits beside it), so the fixture keeps a PRIVATE .env and
    // lets `accounts_config_path` derive the sibling for the refusal probe.
    let accounts_path = dir.join("accounts.toml"); // the LEGACY path — never written
    let env_path = dir.join(".env");
    std::fs::write(
        &env_path,
        "ACCOUNT_LIST=paper\nACCOUNT_PAPER_NAME=\"paper\"\n",
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

// #780 — the file is now a directory: the shared reuse fixture above stays
// here, and the wire tests are grouped by behaviour into child modules.
mod feed_reuse_account_venue_tests;
mod feed_reuse_bind_tests;
mod feed_reuse_venue_rounds_tests;
