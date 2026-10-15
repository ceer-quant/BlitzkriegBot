//! predict.fun market plugin (#424, executor #483).
//!
//! Bundles the stage-2 components (orderbook feed, market discovery) and the
//! stage-4 order executor behind the generic `MarketPlugin` contract, with
//! the network probe wired to [`crate::net_check`]. Everything the core sees
//! is the market-api boundary; all predict.fun knowledge lives in this crate.
//!
//! The executor is inert without credentials ([`crate::executor::
//! spawn_if_configured`] answers `Ok(None)`), so declaring it keeps dry-run
//! and read-only builds honest — `has_executor` is true and order egress is
//! simply absent, the same contract as the kalshi plugin (#479).
//!
//! Capabilities declared from what THIS extension exercises, not what the
//! venue advertises (§7.2): the feed polls REST snapshots (so
//! `LEVEL2_SNAPSHOT`, not `WEBSOCKET_FEED` — the bit describes the delivery
//! contract at the seam), discovery is market-list polling. Prices are
//! probabilities at the venue already, so the feed's only normalization is
//! the degenerate-band filter ([`crate::feed::prob_in_band`]); the UP/DOWN
//! pairing is documented in [`crate::discovery`].

use crate::rest::{Credentials, PredictRest, RestConfig};
use blitzkrieg_market_api::{
    BoxFuture, CoreError, CoreErrorCode, CoreResult, DataFeed, DataFeedConfig, DiscoveryConfig,
    ExecutorConfig, MarketCapabilities, MarketDiscovery, MarketHost, MarketMode, MarketPlugin,
    MarketStructure, MarketType, NetCheckReport, OrderExecutor, TokenId,
};
use std::sync::Arc;

pub struct PredictFunPlugin;

impl PredictFunPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PredictFunPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the shared REST client for the plugin's components. Unauthenticated
/// (public market data works without credentials); the executor, when it
/// arrives in stage 4, builds its own credentialed client.
pub fn rest_client() -> Result<Arc<PredictRest>, CoreError> {
    let mut config = RestConfig::default();
    // The feed and discovery poll from a long-lived task; respect an operator's
    // explicit no-proxy wish if the client environment asks for it (same env
    // contract as the trading client).
    if std::env::var("PREDICT_NO_PROXY").is_ok_and(|v| v == "1" || v == "true") {
        config.no_proxy = true;
    }
    PredictRest::new(config, None::<Credentials>)
        .map(Arc::new)
        .map_err(|e| CoreError::new(CoreErrorCode::VenueError, format!("predictfun client: {e}")))
}

impl MarketPlugin for PredictFunPlugin {
    fn name(&self) -> &str {
        "predictfun"
    }
    fn market_type(&self) -> MarketType {
        MarketType::Prediction
    }
    /// One mode: prediction on the binary outcome wheel. predict.fun binaries
    /// are outcome-token pairs (structure maps to BinaryOutcomeWheel; the two
    /// tokens sharing a condition_id are UP/DOWN — see [`crate::discovery`]).
    fn declare_modes(&self) -> Vec<MarketMode> {
        vec![MarketMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
            capabilities: MarketCapabilities::LEVEL2_SNAPSHOT,
        }]
    }
    fn data_feed(&self) -> Option<&dyn DataFeed> {
        Some(&PREDICT_DATA_FEED)
    }
    fn discovery(&self) -> Option<&dyn MarketDiscovery> {
        Some(&PREDICT_DISCOVERY)
    }
    fn executor(&self) -> Option<&dyn OrderExecutor> {
        Some(&PREDICT_EXECUTOR)
    }
    fn net_check(&self) -> BoxFuture<'_, NetCheckReport> {
        Box::pin(crate::net_check::probe())
    }
}

// ── Executor ─────────────────────────────────────────────────────────────────

struct PredictExecutor;

static PREDICT_EXECUTOR: PredictExecutor = PredictExecutor;

impl OrderExecutor for PredictExecutor {
    fn name(&self) -> &str {
        "predictfun-executor"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: ExecutorConfig,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            // Inert without credentials (dry-run / read-only builds), same
            // contract as the kalshi executor — start() still succeeds and
            // the core runs with no execution egress.
            crate::executor::spawn_if_configured(host, config).await?;
            Ok(())
        })
    }
}

// ── Data feed ────────────────────────────────────────────────────────────────

struct PredictDataFeed;

static PREDICT_DATA_FEED: PredictDataFeed = PredictDataFeed;

impl DataFeed for PredictDataFeed {
    fn name(&self) -> &str {
        "predictfun-orderbook"
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
            let handle = crate::feed::spawn_feed(host.clone(), client, initial_tokens);
            install_control(&host, handle).await;
            Ok(())
        })
    }
}

async fn install_control(host: &Arc<dyn MarketHost>, handle: crate::feed::FeedHandle) {
    host.install_subscription_control(
        Arc::new(handle) as Arc<dyn blitzkrieg_market_api::SubscriptionControl>
    )
    .await;
}

// ── Discovery ────────────────────────────────────────────────────────────────

struct PredictDiscovery;

static PREDICT_DISCOVERY: PredictDiscovery = PredictDiscovery;

impl MarketDiscovery for PredictDiscovery {
    fn name(&self) -> &str {
        "predictfun-markets"
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

// Poll-interval floor/ceiling sanity: a config mistake must never be able to
// turn the poll loop into a busy loop or a stalled feed.
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
