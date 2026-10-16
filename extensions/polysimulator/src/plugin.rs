//! PolySimulator market plugin (#454).
//!
//! Bundles the market-plugin components behind the generic `MarketPlugin`
//! contract, with the network probe wired to [`crate::net_check`]. Everything
//! the core sees is the market-api boundary; all PolySimulator knowledge
//! lives in this crate.
//!
//! ⚠️ This venue is a SIMULATED exchange (paper-trading against live
//! Polymarket-shaped data), not a real one: fills are the venue engine's own
//! model, balances are play money. The declaration carries the
//! `simulated` capability bit so every surface that renders capabilities —
//! the plugin panel included — can label it honestly without any venue-named
//! special case (venue is data, #427).
//!
//! Components: the orderbook feed, round discovery and the order executor,
//! all REST-polling. The executor is inert without credentials, exactly like
//! the sibling plugins: [`crate::executor::spawn_if_configured`] returns
//! `Ok(None)` when the environment carries no `POLYSIM_API_KEY`, so dry-run
//! and read-only builds stay honest.
//!
//! Capabilities declared from what THIS extension exercises, not what the
//! venue advertises (§7.2): the feed polls REST snapshots (so
//! `LEVEL2_SNAPSHOT`, not `WEBSOCKET_FEED` — the bit describes the delivery
//! contract at the seam), discovery is round-list polling, and the venue is
//! simulated (`SIMULATED`).

use crate::rest::{Credentials, PolySimRest, RestConfig};
use blitzkrieg_market_api::{
    BoxFuture, CoreError, CoreErrorCode, CoreResult, DataFeed, DataFeedConfig, DiscoveryConfig,
    ExecutorConfig, MarketCapabilities, MarketDiscovery, MarketHost, MarketMode, MarketPlugin,
    MarketStructure, MarketType, NetCheckReport, OrderExecutor, TokenId,
};
use std::sync::Arc;

pub struct PolySimulatorPlugin;

impl PolySimulatorPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PolySimulatorPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the shared REST client for the plugin's components. Unauthenticated
/// (market data works without credentials); the executor builds its own
/// credentialed client.
pub fn rest_client() -> Result<Arc<PolySimRest>, CoreError> {
    let mut config = RestConfig::default();
    // The feed and discovery poll from a long-lived task; respect an operator's
    // explicit no-proxy wish if the client environment asks for it (same env
    // contract as the trading client).
    if std::env::var("POLYSIM_NO_PROXY").is_ok_and(|v| v == "1" || v == "true") {
        config.no_proxy = true;
    }
    PolySimRest::new(config, None::<Credentials>)
        .map(Arc::new)
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::VenueError,
                format!("polysimulator client: {e}"),
            )
        })
}

impl MarketPlugin for PolySimulatorPlugin {
    fn name(&self) -> &str {
        "polysimulator"
    }
    fn market_type(&self) -> MarketType {
        MarketType::Prediction
    }
    /// One mode: prediction on the binary outcome wheel, REST-poll snapshots,
    /// on a SIMULATED venue. The `simulated` bit is what lets any surface
    /// label this plugin "模拟盘" without a venue-named branch.
    fn declare_modes(&self) -> Vec<MarketMode> {
        vec![MarketMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
            capabilities: MarketCapabilities::LEVEL2_SNAPSHOT | MarketCapabilities::SIMULATED,
        }]
    }
    fn data_feed(&self) -> Option<&dyn DataFeed> {
        Some(&POLYSIM_DATA_FEED)
    }
    fn discovery(&self) -> Option<&dyn MarketDiscovery> {
        Some(&POLYSIM_DISCOVERY)
    }
    fn executor(&self) -> Option<&dyn OrderExecutor> {
        Some(&POLYSIM_EXECUTOR)
    }
    fn net_check(&self) -> BoxFuture<'_, NetCheckReport> {
        Box::pin(crate::net_check::probe())
    }
}

// ── Executor ─────────────────────────────────────────────────────────────────

struct PolySimExecutor;

static POLYSIM_EXECUTOR: PolySimExecutor = PolySimExecutor;

impl OrderExecutor for PolySimExecutor {
    fn name(&self) -> &str {
        "polysimulator-executor"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: ExecutorConfig,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            // Inert without credentials (dry-run / read-only builds), same
            // contract as the sibling bridges — start() still succeeds and
            // the core runs with no execution egress.
            crate::executor::spawn_if_configured(host, config).await?;
            Ok(())
        })
    }
}

// ── Data feed ────────────────────────────────────────────────────────────────

struct PolySimDataFeed;

static POLYSIM_DATA_FEED: PolySimDataFeed = PolySimDataFeed;

impl DataFeed for PolySimDataFeed {
    fn name(&self) -> &str {
        "polysimulator-orderbook"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        _config: DataFeedConfig,
        initial_tokens: Vec<TokenId>,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            let client = rest_client()?;
            // Register the handle so later `subscribe_tokens` calls (round
            // rollovers from discovery) reach the poll loop.
            let handle = crate::feed::spawn_feed_with(host.clone(), client, initial_tokens, 0);
            host.install_subscription_control("polysimulator", Arc::new(handle))
                .await;
            Ok(())
        })
    }
}

// ── Discovery ────────────────────────────────────────────────────────────────

struct PolySimDiscovery;

static POLYSIM_DISCOVERY: PolySimDiscovery = PolySimDiscovery;

impl MarketDiscovery for PolySimDiscovery {
    fn name(&self) -> &str {
        "polysimulator-events"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: DiscoveryConfig,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            let client = rest_client()?;
            crate::discovery::spawn(
                host,
                client,
                config.assets,
                config.round_duration_sec,
                config.poll_sec,
            );
            Ok(())
        })
    }
}

/// Sanity: the plugin's poll cadence constants stay in a sane band.
const _: () = {
    assert!(
        crate::feed::MIN_POLL_MS < crate::feed::DEFAULT_POLL_MS,
        "the floor must not override the default"
    );
    assert!(
        crate::feed::MAX_POLL_MS > crate::feed::DEFAULT_POLL_MS,
        "the ceiling must not override the default"
    );
};
