//! PolySimulator market plugin (#454) — parasitic form.
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
//! Parasitic shape: this plugin registers NO data feed and NO round
//! discovery of its own. Market data and rounds come from the polymarket
//! plugin's feed and discovery (this venue mirrors Polymarket's token ids
//! byte for byte — `mirror_of = Some("polymarket")` is the engine's
//! admission/routing evidence); what remains here is the order executor plus
//! the settlement sweep. The executor is inert without credentials, exactly
//! like the sibling plugins: [`crate::executor::spawn_if_configured`] returns
//! `Ok(None)` when the environment carries no `POLYSIM_API_KEY`, so dry-run
//! and read-only builds stay honest.
//!
//! Capabilities declared from what THIS extension exercises, not what the
//! venue advertises (§7.2: the declaration IS the transport contract at the
//! seam): the only remaining channel is the executor's REST polling, so the
//! plugin declares `SIMULATED` and nothing else.

use blitzkrieg_market_api::{
    BoxFuture, CoreResult, ExecutorConfig, MarketCapabilities, MarketHost, MarketMode,
    MarketPlugin, MarketStructure, MarketType, NetCheckReport, OrderExecutor,
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

impl MarketPlugin for PolySimulatorPlugin {
    fn name(&self) -> &str {
        "polysimulator"
    }
    fn required_credential_env(&self) -> &[&str] {
        &["POLYSIM_API_KEY"]
    }
    fn market_type(&self) -> MarketType {
        MarketType::Prediction
    }
    /// One mode: prediction on the binary outcome wheel, on a SIMULATED
    /// venue. The `simulated` bit is what lets any surface label this plugin
    /// "模拟盘" without a venue-named branch.
    fn declare_modes(&self) -> Vec<MarketMode> {
        vec![MarketMode {
            market_type: MarketType::Prediction,
            structure: Some(MarketStructure::BinaryOutcomeWheel),
            capabilities: MarketCapabilities::SIMULATED,
        }]
    }
    /// Undeclared on purpose: this plugin feeds no rounds of its own (the
    /// polymarket plugin's discovery owns the table rows), so no duration is
    /// wrong to offer. `None` = the panel filters nothing.
    fn supported_round_secs(&self) -> Option<Vec<i64>> {
        None
    }
    /// Constructive fact of this venue's identity: PolySimulator IS a
    /// Polymarket simulator — its token ids are Polymarket's byte for byte.
    /// The engine reads this as the mirror admission/routing evidence that
    /// does not depend on this plugin declaring any rounds of its own (the
    /// parasitic shape: Polymarket's feed owns the table rows, this plugin
    /// trades through them).
    fn mirror_of(&self) -> Option<&str> {
        Some("polymarket")
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
