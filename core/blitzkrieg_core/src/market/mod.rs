//! Market seam — how the market-agnostic core talks to a concrete venue.
//!
//! The core (OME, risk, ledger, strategy engine) speaks only market-api boundary
//! types. A `MarketPlugin` supplies a `DataFeed`, `MarketDiscovery` and
//! `OrderExecutor`, all driving the core through `MarketHost`. Concrete venues
//! live in their own crates (see `extensions/*`); this module only hosts the
//! registry and the feature-gated registration seam.

pub mod host;
pub mod registry;

/// Register the market plugins compiled into this build.
///
/// This is the ONE place the core names a concrete market. Each entry is behind
/// a feature so the core can be built market-free (`--no-default-features`) and
/// so a different market is a Cargo feature rather than a code change.
pub fn register_builtin_markets(reg: &registry::MarketPluginRegistry) {
    #[cfg(feature = "polymarket")]
    reg.register(Box::new(polymarket_extension::PolymarketPlugin::new()));
    #[cfg(not(feature = "polymarket"))]
    let _ = reg; // market-free build: nothing to register
}

/// Select the operative market plugin for this process.
///
/// `preferred` is the configured plugin name (`--market-plugin`). When it is
/// absent or not registered we fall back to the first registered plugin, so a
/// stray flag can never brick startup. The chosen plugin is marked active in the
/// registry (surfaced by `market.list`).
pub fn active_market_plugin(
    registry: &registry::MarketPluginRegistry,
    preferred: Option<&str>,
) -> std::sync::Arc<dyn blitzkrieg_market_api::MarketPlugin> {
    if let Some(name) = preferred {
        if let Some(p) = registry.get(name) {
            registry.set_enabled(name, true);
            registry.set_active(name);
            return p;
        }
        eprintln!(
            "blitzkrieg-core: requested market plugin '{name}' not registered — using first available"
        );
    }
    for name in registry.names() {
        if let Some(p) = registry.get(&name) {
            registry.set_enabled(&name, true);
            registry.set_active(&name);
            return p;
        }
    }
    // No plugin registered: a do-nothing placeholder keeps the server booting
    // (dry runs with market data pushed over IPC still work).
    std::sync::Arc::new(NoopMarketPlugin)
}

/// Fallback plugin used when no market is compiled in / registered.
struct NoopMarketPlugin;

impl blitzkrieg_market_api::MarketPlugin for NoopMarketPlugin {
    fn name(&self) -> &str {
        "none"
    }
    fn market_type(&self) -> blitzkrieg_market_api::MarketType {
        blitzkrieg_market_api::MarketType::Prediction
    }
}
