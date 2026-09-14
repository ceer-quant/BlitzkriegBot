//! Polymarket market plugin.
//!
//! Bundles the three components (orderbook feed, Gamma discovery, CLOB executor)
//! behind the generic `MarketPlugin` contract. Everything the core sees is the
//! market-api boundary; all Polymarket/SDK knowledge lives in this crate.

use blitzkrieg_market_api::{
    BoxFuture, CoreError, CoreErrorCode, CoreResult, DataFeed, DataFeedConfig, DiscoveryConfig,
    ExecutorConfig, MarketDiscovery, MarketHost, MarketPlugin, MarketType, OrderExecutor, TokenId,
};
use std::sync::Arc;

pub struct PolymarketPlugin;

impl PolymarketPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PolymarketPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl MarketPlugin for PolymarketPlugin {
    fn name(&self) -> &str {
        "polymarket"
    }
    fn market_type(&self) -> MarketType {
        MarketType::Prediction
    }
    fn data_feed(&self) -> Option<&dyn DataFeed> {
        Some(&POLY_DATA_FEED)
    }
    fn discovery(&self) -> Option<&dyn MarketDiscovery> {
        Some(&POLY_DISCOVERY)
    }
    fn executor(&self) -> Option<&dyn OrderExecutor> {
        Some(&POLY_EXECUTOR)
    }
}

// ── Data feed ────────────────────────────────────────────────────────────────

struct PolymarketDataFeed;

static POLY_DATA_FEED: PolymarketDataFeed = PolymarketDataFeed;

impl DataFeed for PolymarketDataFeed {
    fn name(&self) -> &str {
        "polymarket-orderbook"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: DataFeedConfig,
        initial_tokens: Vec<TokenId>,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            // `spawn_feed` returns a handle immediately and drives the loops on
            // background tasks; register it so the host can re-subscribe tokens.
            let handle = crate::feed::spawn_feed(host.clone(), initial_tokens, config.spot_assets).await;
            host.install_subscription_control(Arc::new(handle)).await;
            Ok(())
        })
    }
}

// ── Discovery ────────────────────────────────────────────────────────────────

struct PolymarketDiscovery;

static POLY_DISCOVERY: PolymarketDiscovery = PolymarketDiscovery;

impl MarketDiscovery for PolymarketDiscovery {
    fn name(&self) -> &str {
        "polymarket-gamma"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: DiscoveryConfig,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            crate::discovery::spawn(host, config.assets, config.round_duration_sec, config.poll_sec);
            Ok(())
        })
    }
}

// ── Order executor ───────────────────────────────────────────────────────────

struct PolymarketExecutor;

static POLY_EXECUTOR: PolymarketExecutor = PolymarketExecutor;

impl OrderExecutor for PolymarketExecutor {
    fn name(&self) -> &str {
        "polymarket-clob"
    }

    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: ExecutorConfig,
    ) -> BoxFuture<'a, CoreResult<()>> {
        Box::pin(async move {
            // The bridge reads POLYMARKET_* from the environment itself and stays
            // inert when they are absent (dry runs, or live without keys).
            match crate::live::spawn_if_configured(host, config.markets).await {
                Ok(Some(_handle)) => Ok(()),
                Ok(None) => {
                    eprintln!("polymarket-extension: live bridge inert (no credentials)");
                    Ok(())
                }
                Err(e) => Err(CoreError::new(
                    CoreErrorCode::VenueError,
                    format!("live bridge failed to start: {e}"),
                )),
            }
        })
    }
}
