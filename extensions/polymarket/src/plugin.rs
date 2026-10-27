//! Polymarket market plugin.
//!
//! Bundles the three components (orderbook feed, Gamma discovery, CLOB executor)
//! behind the generic `MarketPlugin` contract. Everything the core sees is the
//! market-api boundary; all Polymarket/SDK knowledge lives in this crate.

use blitzkrieg_market_api::{
    BoxFuture, CoreError, CoreErrorCode, CoreResult, DataFeed, DataFeedConfig, DiscoveryConfig,
    ExecutorConfig, MarketCapabilities, MarketDiscovery, MarketHost, MarketMode, MarketPlugin,
    MarketStructure, MarketType, NetCheckReport, OrderExecutor, TokenId,
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
    fn required_credential_env(&self) -> &[&str] {
        // The executor reads these once at start (`exec.start`); the kernel
        // surfaces only their NAMES (issue 648 declaration, was a hardcoded
        // core-side match).
        &["POLYMARKET_PRIVATE_KEY", "POLYMARKET_FUNDER_ADDRESS"]
    }
    fn market_type(&self) -> MarketType {
        MarketType::Prediction
    }
    /// The market modes this plugin actually serves (§7.3): one mode —
    /// prediction on the binary outcome wheel (one settlement per round, two
    /// complementary tokens). Capabilities are declared from what THIS
    /// extension exercises, not what the venue advertises (§7.2: the design
    /// doc does not guess the bitmap for us):
    ///   - `websocket_feed`: market data reaches the core as PUSHED callbacks
    ///     — the feed task drives `on_book`/`on_spot` from its own connection
    ///     and a strategy never polls. The venue's market WS channel is
    ///     deliberately NOT used (see `feed.rs` for the bandwidth verdict);
    ///     the bit describes the delivery contract at the seam, not the venue
    ///     transport.
    ///   - `level2_snapshot`: `POST /books` returns full depth per token.
    ///   - `post_only`: the executor's maker path posts GTC limits through the
    ///     SDK's `post_only` flag (`venue.rs`).
    fn declare_modes(&self) -> Vec<MarketMode> {
        vec![MarketMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
            capabilities: MarketCapabilities::WEBSOCKET_FEED
                | MarketCapabilities::LEVEL2_SNAPSHOT
                | MarketCapabilities::POST_ONLY,
        }]
    }
    /// The Gamma slug families this plugin routes (`crate::gamma`:
    /// `duration_label` maps exactly these five; the epoch family for
    /// 5m/15m/4h, the ET clock-wall family for 1h/daily). Declared here so
    /// the panel's rounds editor offers only durations that discover.
    fn supported_round_secs(&self) -> Option<Vec<i64>> {
        Some(vec![300, 900, 3600, 14400, 86_400])
    }
    /// The Gamma slug labels for those families — the plugin's own naming
    /// vocabulary (`crate::gamma::duration_label` spells the same table when
    /// it builds discovery slugs; this declaration is the contract face of
    /// the same knowledge). Verbatim the historical kernel table so the
    /// `round_duration.get` readout keeps rendering identical labels.
    fn round_labels(&self) -> &[(i64, &'static str)] {
        &[
            (300, "5m"),
            (900, "15m"),
            (3600, "1h"),
            (14400, "4h"),
            (86400, "daily"),
        ]
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
    /// The four paths this venue needs — CLOB REST, Gamma, Binance spot and the
    /// authenticated fill stream — probed resolver-first. Read-only and
    /// credential-free; `crate::net_check` documents the shape and the ordering.
    fn net_check(&self) -> BoxFuture<'_, NetCheckReport> {
        Box::pin(crate::net_check::probe())
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
            let handle =
                crate::feed::spawn_feed(host.clone(), initial_tokens, config.spot_assets).await;
            host.install_subscription_control("polymarket", Arc::new(handle))
                .await;
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
            crate::discovery::spawn(
                host,
                config.assets,
                config.round_duration_sec,
                config.poll_sec,
            );
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

#[cfg(test)]
mod round_labels_tests {
    use super::*;

    /// The anti-drift pin (issue 648): this plugin's declared label table is
    /// VERBATIM the historical kernel table (`scanner::duration_label_for`,
    /// retired by this change) — the values the `round_duration.get` readout
    /// rendered before the labels moved to the declaration face. A drift here
    /// is a UI-visible rename of an existing family, not a declaration
    /// detail: 300→"5m", 900→"15m", 3600→"1h", 14400→"4h", 86400→"daily".
    /// `crate::gamma::duration_label` (the slug builder's own copy) must
    /// agree with this table — the two faces spell one venue vocabulary.
    #[test]
    fn declared_round_labels_are_verbatim_the_historical_kernel_table() {
        let declared = PolymarketPlugin.round_labels();
        assert_eq!(
            declared,
            &[
                (300, "5m"),
                (900, "15m"),
                (3600, "1h"),
                (14400, "4h"),
                (86400, "daily"),
            ],
            "the declaration must stay verbatim the historical kernel table"
        );
        // The slug vocabulary and the declaration face spell the SAME table —
        // a rename on either side alone would split the venue's own names.
        for (sec, label) in declared {
            assert_eq!(crate::gamma::duration_label(*sec), Some(*label));
        }
        assert_eq!(crate::gamma::duration_label(60), None);
    }
}
