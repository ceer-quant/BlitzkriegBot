//! Kalshi market plugin (#424).
//!
//! Bundles the market-plugin components behind the generic `MarketPlugin`
//! contract, with the network probe wired to [`crate::net_check`]. Everything
//! the core sees is the market-api boundary; all Kalshi knowledge lives in
//! this crate.
//!
//! Components: the orderbook feed and event discovery (stage 2, #424) and the
//! order executor (stage 4 of the integration — the component #426's plan
//! deferred and this crate now ships, closing the gap that made kalshi a
//! read-only plugin next to polymarket). The executor is inert without
//! credentials, exactly like the polymarket bridge: [`crate::executor::
//! spawn_if_configured`] returns `Ok(None)` when the environment carries no
//! key pair, so dry-run and read-only builds stay honest.
//!
//! Capabilities declared from what THIS extension exercises, not what the
//! venue advertises (§7.2): the feed polls REST snapshots (so
//! `LEVEL2_SNAPSHOT`, not `WEBSOCKET_FEED` — the bit describes the delivery
//! contract at the seam), discovery is event-list polling. The feed's
//! cent→probability mapping is the 0..1 normalization #424 requires; the
//! YES/NO book split is documented in [`crate::discovery`].

use crate::rest::{Credentials, KalshiRest, RestConfig};
use blitzkrieg_market_api::{
    BoxFuture, CoreError, CoreErrorCode, CoreResult, DataFeed, DataFeedConfig, DiscoveryConfig,
    ExecutorConfig, MarketCapabilities, MarketDiscovery, MarketHost, MarketMode, MarketPlugin,
    MarketStructure, MarketType, NetCheckReport, OrderExecutor, TokenId,
};
use std::sync::Arc;

pub struct KalshiPlugin;

impl KalshiPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for KalshiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the shared REST client for the plugin's components. Unauthenticated
/// (public market data works without credentials); the executor, when it
/// arrives in stage 4, builds its own credentialed client.
pub fn rest_client() -> Result<Arc<KalshiRest>, CoreError> {
    let mut config = RestConfig::default();
    // The feed and discovery poll from a long-lived task; respect an operator's
    // explicit no-proxy wish if the client environment asks for it (same env
    // contract as the trading client).
    if std::env::var("KALSHI_NO_PROXY").is_ok_and(|v| v == "1" || v == "true") {
        config.no_proxy = true;
    }
    KalshiRest::new(config, None::<Credentials>)
        .map(Arc::new)
        .map_err(|e| CoreError::new(CoreErrorCode::VenueError, format!("kalshi client: {e}")))
}

impl MarketPlugin for KalshiPlugin {
    fn name(&self) -> &str {
        "kalshi"
    }
    fn market_type(&self) -> MarketType {
        MarketType::Prediction
    }
    /// One mode: prediction on the binary outcome wheel. Kalshi markets are
    /// YES/NO binaries (structure maps to BinaryOutcomeWheel; YES↔UP, NO↔DOWN
    /// — see [`crate::discovery`]).
    fn declare_modes(&self) -> Vec<MarketMode> {
        vec![MarketMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
            capabilities: MarketCapabilities::LEVEL2_SNAPSHOT,
        }]
    }
    /// The only up/down family this venue routes (see `crate::discovery`:
    /// `series_ticker_for` maps 900 → KX{ASSET}15M and nothing else — the
    /// daily KX{ASSET}D is a strike ladder, not a binary). Declared here so
    /// the panel's rounds editor never offers a duration the venue cannot
    /// serve (the 4h configuration used to discover nothing, silently).
    fn supported_round_secs(&self) -> Option<Vec<i64>> {
        Some(vec![900])
    }
    fn data_feed(&self) -> Option<&dyn DataFeed> {
        Some(&KALSHI_DATA_FEED)
    }
    fn discovery(&self) -> Option<&dyn MarketDiscovery> {
        Some(&KALSHI_DISCOVERY)
    }
    fn executor(&self) -> Option<&dyn OrderExecutor> {
        Some(&KALSHI_EXECUTOR)
    }
    fn net_check(&self) -> BoxFuture<'_, NetCheckReport> {
        Box::pin(crate::net_check::probe())
    }
}

// ── Executor ─────────────────────────────────────────────────────────────────

struct KalshiExecutor;

static KALSHI_EXECUTOR: KalshiExecutor = KalshiExecutor;

impl OrderExecutor for KalshiExecutor {
    fn name(&self) -> &str {
        "kalshi-executor"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: ExecutorConfig,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            // Inert without credentials (dry-run / read-only builds), same
            // contract as the polymarket bridge — start() still succeeds and
            // the core runs with no execution egress.
            crate::executor::spawn_if_configured(host, config).await?;
            Ok(())
        })
    }
}

// ── Data feed ────────────────────────────────────────────────────────────────

struct KalshiDataFeed;

static KALSHI_DATA_FEED: KalshiDataFeed = KalshiDataFeed;

impl DataFeed for KalshiDataFeed {
    fn name(&self) -> &str {
        "kalshi-orderbook"
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
            host.install_subscription_control("kalshi", Arc::new(handle))
                .await;
            Ok(())
        })
    }
}

// ── Discovery ────────────────────────────────────────────────────────────────

struct KalshiDiscovery;

static KALSHI_DISCOVERY: KalshiDiscovery = KalshiDiscovery;

impl MarketDiscovery for KalshiDiscovery {
    fn name(&self) -> &str {
        "kalshi-events"
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
