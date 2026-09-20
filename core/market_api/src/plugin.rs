//! The plugin contracts: `MarketHost` (what the core exposes) and the three
//! component traits a market plugin implements, bundled by `MarketPlugin`.
//!
//! Async dispatch uses boxed futures (the same pattern as `MarketAdapter`), so
//! every trait stays object-safe and this crate needs no async-trait dependency.

use crate::types::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Boxed future used across the plugin boundary for object safety.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Handle returned by a running [`DataFeed`] that the host uses to change the
/// live subscription set (e.g. when a new round's tokens are discovered).
pub trait SubscriptionControl: Send + Sync {
    fn set_tokens(&self, tokens: Vec<TokenId>);
}

/// The core's inward-facing API, implemented by the host. This is the ONLY
/// surface a market plugin may call: it can push market data in and pull work
/// out, but has no access to the OME, ledger, risk gate, credentials or socket.
pub trait MarketHost: Send + Sync {
    // ── Data ingress ────────────────────────────────────────────────────────
    fn on_book(&self, update: BookUpdate) -> BoxFuture<'_, ()>;
    fn on_top_of_book(&self, update: TopOfBookUpdate) -> BoxFuture<'_, ()>;
    fn on_spot(&self, update: SpotUpdate) -> BoxFuture<'_, ()>;
    fn on_round_markets(&self, markets: Vec<MarketDescriptor>) -> BoxFuture<'_, ()>;
    /// Replace the live orderbook subscription set (new round tokens).
    fn subscribe_tokens(&self, tokens: Vec<TokenId>) -> BoxFuture<'_, ()>;
    /// A [`DataFeed`] registers its control handle here at start-up so later
    /// `subscribe_tokens` calls (and the `engine.markets` bridge) reach it.
    fn install_subscription_control(
        &self,
        control: Arc<dyn SubscriptionControl>,
    ) -> BoxFuture<'_, ()>;

    // ── Execution egress ────────────────────────────────────────────────────
    /// Orders accepted locally with no venue id yet.
    fn take_pending_orders(&self) -> BoxFuture<'_, Vec<PendingOrder>>;
    /// Venue order ids the core has retired locally (regime pull, round
    /// cleanup, escalation, explicit cancel) but the venue may still hold
    /// resting. The executor must forward each as a real venue cancel — a
    /// local-only retire that never reaches the venue is exactly how resting
    /// orphans are born.
    fn take_pending_cancels(&self) -> BoxFuture<'_, Vec<String>>;
    fn on_order_accepted(&self, core_order_id: &str, venue_order_id: &str) -> BoxFuture<'_, ()>;
    /// The venue refused a locally-accepted order. `error` carries the venue's
    /// own failure text when the POST returned one, so the core can classify
    /// it (cooldowns, panel visibility, consecutive-failure freeze).
    fn on_order_rejected(&self, core_order_id: &str, error: Option<CoreError>)
    -> BoxFuture<'_, ()>;
    fn on_fill(&self, fill: MarketFill) -> BoxFuture<'_, ()>;
    fn on_order_live(&self, venue_order_id: &str) -> BoxFuture<'_, ()>;
    fn on_order_cancelled(&self, venue_order_id: &str) -> BoxFuture<'_, ()>;
    fn on_reconcile(&self, snapshot: ReconcileSnapshot) -> BoxFuture<'_, ()>;
    /// The executor's periodic reconciliation sweep failed three times in a
    /// row: the safety net is blind (auth, venue down or a dead sweep
    /// transport). The core freezes trading on this — unreconciled drift is
    /// how ghost positions and orphans accumulate (E31-b).
    fn on_reconcile_failed(&self, error: CoreError) -> BoxFuture<'_, ()>;
    /// Report of a trading-capability self-check run by the executor. A failed
    /// report freezes trading (kill switch) — the core decides once, here.
    fn on_self_check(&self, report: SelfCheckReport) -> BoxFuture<'_, ()>;
    /// Seed the core's cash ledger from the venue's reported balance at startup.
    fn seed_balance(&self, balance: rust_decimal::Decimal) -> BoxFuture<'_, ()>;
    /// Periodic venue-cash sync: the venue's FREE collateral view. The core
    /// decides how to fold it in (with its own reserved amount) so resting-order
    /// commitments are never double-counted.
    fn venue_free_balance(&self, free: rust_decimal::Decimal) -> BoxFuture<'_, ()>;
    /// Venue order ids the core believes are resting. The executor's startup
    /// sweep cancels any venue-open id NOT in this set (orphans left by a crash
    /// or a previous process).
    fn known_venue_order_ids(&self) -> BoxFuture<'_, Vec<String>>;
    /// Report that an orphan venue order was discovered and cancelled.
    fn note_orphan_cancelled(&self, venue_order_id: &str) -> BoxFuture<'_, ()>;
    /// Surface a venue/plugin error to the core (never swallowed).
    fn report_error(&self, error: CoreError) -> BoxFuture<'_, ()>;
}

/// The market-data component: owns a long-lived push connection (orderbook /
/// spot) and forwards every update to the host.
pub trait DataFeed: Send + Sync {
    fn name(&self) -> &str;
    /// Start streaming. `initial_tokens` seeds the subscription; further tokens
    /// arrive via `MarketHost::subscribe_tokens`. The host is passed as an owned
    /// `Arc` so the feed can move it into its background tasks.
    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: DataFeedConfig,
        initial_tokens: Vec<TokenId>,
    ) -> BoxFuture<'a, Result<(), CoreError>>;
}

/// The discovery component: polls the venue for the current round's markets and
/// registers them with the host on each rollover.
pub trait MarketDiscovery: Send + Sync {
    fn name(&self) -> &str;
    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: DiscoveryConfig,
    ) -> BoxFuture<'a, Result<(), CoreError>>;
}

/// The execution component: a request/response actor that places locally-accepted
/// orders, ingests authoritative fills and reconciles on a cadence.
pub trait OrderExecutor: Send + Sync {
    fn name(&self) -> &str;
    fn start<'a>(
        &'a self,
        host: Arc<dyn MarketHost>,
        config: ExecutorConfig,
    ) -> BoxFuture<'a, Result<(), CoreError>>;
}

/// A complete market plugin: the three components. Any component may be absent
/// (e.g. a venue with no discovery endpoint).
pub trait MarketPlugin: Send + Sync {
    fn name(&self) -> &str;
    fn market_type(&self) -> MarketType;
    fn data_feed(&self) -> Option<&dyn DataFeed> {
        None
    }
    fn discovery(&self) -> Option<&dyn MarketDiscovery> {
        None
    }
    fn executor(&self) -> Option<&dyn OrderExecutor> {
        None
    }
    /// Static capability report for `market.list`.
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: self.name().to_string(),
            market_type: self.market_type(),
            has_data_feed: self.data_feed().is_some(),
            has_discovery: self.discovery().is_some(),
            has_executor: self.executor().is_some(),
            enabled: false,
            active: false,
        }
    }
}
