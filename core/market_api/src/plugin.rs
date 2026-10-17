//! The plugin contracts: `MarketHost` (what the core exposes) and the three
//! component traits a market plugin implements, bundled by `MarketPlugin`.
//!
//! Async dispatch uses boxed futures (the same pattern as `MarketAdapter`), so
//! every trait stays object-safe and this crate needs no async-trait dependency.

use crate::modes::{MarketCapabilities, MarketMode};
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
    /// 盘口批次原子化：一次请求拿到多本盘口的批式拉取（polymarket
    /// `POST /books` 一类）改走这里 —— 整批 book + 单一批次时间戳，一次
    /// 应用、引擎统一看到同一批快照。逐 token 发射会在主机两次持锁的间隙
    /// 让策略读到「一半新一半旧」的撕裂状态（跨腿策略据此对一对不存在的
    /// 折价开火）。实现方必须保证：批内所有 book 在同一次锁区间内应用完，
    /// 批内时间戳一致；策略评估点不可能插入批次中间。
    ///
    /// 单 book 场景（WebSocket 增量、逐 token REST）继续走 [`MarketHost::on_book`]
    /// —— 那里没有批可撕。默认实现退化为逐本 [`MarketHost::on_book`]：现有
    /// 的 trait 实现者零破坏，但只能拿到「逐本」的原子性，撕裂守卫只在
    /// 覆写了本方法的宿主上成立。
    fn on_books<'a>(&'a self, updates: Vec<BookUpdate>, ts_ms: i64) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for u in updates {
                self.on_book(u).await;
            }
            let _ = ts_ms;
        })
    }
    fn on_top_of_book(&self, update: TopOfBookUpdate) -> BoxFuture<'_, ()>;
    fn on_spot(&self, update: SpotUpdate) -> BoxFuture<'_, ()>;
    fn on_round_markets(&self, markets: Vec<MarketDescriptor>) -> BoxFuture<'_, ()>;
    /// Replace the live orderbook subscription set (new round tokens) on the
    /// NAMED plugin's feed. `plugin` is the market-plugin name (`self.name()`
    /// of the caller's own plugin): with multiple data plugins running
    /// concurrently, each plugin's tokens must reach only ITS feed — a
    /// Polymarket rollover must never replace the Kalshi subscription set.
    /// Single-plugin sessions route by the one registered name.
    fn subscribe_tokens<'a>(&'a self, plugin: &str, tokens: Vec<TokenId>) -> BoxFuture<'a, ()>;
    /// A [`DataFeed`] of the NAMED plugin registers its control handle here at
    /// start-up so later `subscribe_tokens` calls reach it (and only it).
    /// `plugin` is the market-plugin name; one control slot per name.
    fn install_subscription_control<'a>(
        &'a self,
        plugin: &str,
        control: Arc<dyn SubscriptionControl>,
    ) -> BoxFuture<'a, ()>;

    // ── Execution egress ────────────────────────────────────────────────────
    /// Orders accepted locally with no venue id yet. `plugin` is the calling
    /// executor's market-plugin name: with multiple executors running
    /// concurrently, each must drain only ITS venue's orders — a Polymarket
    /// executor must never POST a Kalshi order. Untagged orders (hosts that
    /// do not label markets, legacy corpora) fall back to the host's active
    /// plugin, which keeps the single-plugin behaviour byte-identical.
    fn take_pending_orders<'a>(&'a self, plugin: &str) -> BoxFuture<'a, Vec<PendingOrder>>;
    /// Venue order ids the core has retired locally (regime pull, round
    /// cleanup, escalation, explicit cancel) but the venue may still hold
    /// resting. The executor must forward each as a real venue cancel — a
    /// local-only retire that never reaches the venue is exactly how resting
    /// orphans are born. `plugin` is the calling executor's market-plugin
    /// name: the queue is take-once, so with multiple executors running each
    /// must drain only ITS venue's ids — a sibling draining them first would
    /// swallow the cancel forever (the orphan the rule above forbids).
    fn take_pending_cancels<'a>(&'a self, plugin: &str) -> BoxFuture<'a, Vec<String>>;
    fn on_order_accepted(&self, core_order_id: &str, venue_order_id: &str) -> BoxFuture<'_, ()>;
    /// The venue refused a locally-accepted order. `error` carries the venue's
    /// own failure text when the POST returned one, so the core can classify
    /// it (cooldowns, panel visibility, consecutive-failure freeze).
    fn on_order_rejected(&self, core_order_id: &str, error: Option<CoreError>)
    -> BoxFuture<'_, ()>;
    /// The venue POST timed out — the venue MAY have accepted the order
    /// (#462). The core must NOT release the reservation or mark the order
    /// Rejected: it keeps the order Pending and the periodic reconciliation
    /// sweep decides (resting → adopt; filled → gap-fill; absent → retire).
    fn on_place_timeout(&self, core_order_id: &str) -> BoxFuture<'_, ()>;
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

    // ── Settlement / redemption (issue #175) ─────────────────────────────────
    /// Markets the core holds a position in and whose resolution it does not
    /// know yet. The plugin answers each one with
    /// [`MarketHost::on_market_resolution`] — including "not resolved yet", so
    /// the core can tell a venue that answered from one that is not running.
    fn take_settlement_queries(&self) -> BoxFuture<'_, Vec<SettlementQuery>>;
    /// The venue's verdict on a market (or `resolved: false` while it is still
    /// open). The core books the settlement from this and nothing else.
    fn on_market_resolution(&self, resolution: MarketResolution) -> BoxFuture<'_, ()>;
    /// Settled positions whose collateral has not been redeemed on-chain yet,
    /// with any retry backoff already elapsed. The plugin performs the redeem
    /// and reports back through [`MarketHost::on_redemption_result`].
    fn take_pending_redemptions(&self) -> BoxFuture<'_, Vec<RedemptionRequest>>;
    /// The outcome of one redemption: a mined tx hash + block, or a failure.
    fn on_redemption_result(&self, result: RedemptionResult) -> BoxFuture<'_, ()>;
    /// #461: complete share-pair merges the core has already burned from its
    /// books — the venue executes the CTF merge and reports back through
    /// [`MarketHost::on_merge_result`]. Dispatch semantics mirror the
    /// redemption path (the core stamps each dispatch's backoff).
    fn take_pending_merges(&self) -> BoxFuture<'_, Vec<MergeRequest>>;
    /// The outcome of one merge: a mined tx hash + block, or a failure.
    fn on_merge_result(&self, result: MergeResult) -> BoxFuture<'_, ()>;
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
        let modes = self.declare_modes();
        // Summary of the declaration (§8.3): capabilities OR across every
        // declared mode; structure only when every mode names the SAME
        // concrete structure — a plugin spanning structures cannot be summed
        // up as one, and `None` here reads as "unspecified", which is exactly
        // how the handshake treats it.
        let capabilities = modes
            .iter()
            .fold(MarketCapabilities::NONE, |acc, m| acc | m.capabilities);
        let structure = modes
            .first()
            .and_then(|m| m.structure)
            .filter(|first| modes.iter().all(|m| m.structure == Some(*first)));
        PluginInfo {
            name: self.name().to_string(),
            market_type: self.market_type(),
            structure,
            capabilities,
            has_data_feed: self.data_feed().is_some(),
            has_discovery: self.discovery().is_some(),
            has_executor: self.executor().is_some(),
            enabled: false,
            active: false,
        }
    }

    /// The market modes this plugin declares (§7.3). Empty = undeclared: the
    /// handshake then treats the plugin as ONE implicit market_type-only mode
    /// (its `market_type()`, no structure, no capabilities — the server
    /// assembly applies that fallback before the judge runs). The default
    /// keeps every existing and third-party plugin compiling and loading
    /// strategies unchanged.
    fn declare_modes(&self) -> Vec<MarketMode> {
        Vec::new()
    }

    /// Probe the network paths this venue needs (`net.check` / `--net-check` /
    /// `blitzkrieg net-check`): resolver, TCP, TLS and one cheap request per
    /// endpoint, read-only and credential-free so it is safe to run while the
    /// bot trades — which is exactly when an operator reaches for it.
    ///
    /// The default answer is an explicit `unsupported` item, never an empty
    /// success: a plugin that has no probe must not be able to read as healthy.
    fn net_check(&self) -> BoxFuture<'_, NetCheckReport> {
        Box::pin(async move {
            let proxy = crate::net::proxy_env_names();
            let plugin = self.name().to_string();
            let report = NetCheckReport {
                ok: false,
                ts_ms: crate::net::now_ms(),
                hint_code: String::new(),
                hint: String::new(),
                proxy_env: Vec::new(),
                items: vec![NetCheckItem {
                    name: "venue".to_string(),
                    target: plugin.clone(),
                    ok: false,
                    status: crate::net::UNSUPPORTED.to_string(),
                    addrs: Vec::new(),
                    fake_ip: false,
                    ms: 0,
                    detail: format!("market plugin `{plugin}` implements no network probe"),
                }],
            };
            crate::net::finish(report, proxy)
        })
    }
}
