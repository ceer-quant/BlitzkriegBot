//! `CoreHost` — the core's implementation of `MarketHost`.
//!
//! A market plugin can only reach the core through this object. Every method is
//! a thin, logic-free forward onto the corresponding `Core` method, so the
//! plugin boundary carries no trading behaviour of its own.

use crate::model as m;
use crate::service::Core;
use blitzkrieg_market_api as api;
use blitzkrieg_market_api::net::now_ms;
use blitzkrieg_market_api::{BoxFuture, MarketHost};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

// ── RFC-661 M2: the feed_source reuse fan-out ────────────────────────────────

/// One declared data-topology edge: CONSUMER's market data eats SOURCE's
/// upstream feed/discovery stream (`MarketPlugin::feed_source`). A list of
/// these — not a flat venue list — is the fan-out's fact shape: two
/// independent consumer→source pairs must never re-register each other's
/// batches (a flat list keyed only on "is the upstream in the set" would
/// fan source A's batch out to consumer B, a venue whose token space A
/// shares nothing with).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedReuseEdge {
    pub consumer: String,
    pub source: String,
}

/// The multiplexed data plane's shared bookkeeping (RFC-661 M2). The host's
/// `on_round_markets` consults it to re-register an upstream round batch under
/// every CONSUMING venue's own identity, so a parasitic plugin gains its own
/// scanner rows / books / position venue stamps without the OME ever seeing a
/// mirror fact. `edges` is the declared consumer→source list (boot actives +
/// runtime binds); `mounts` counts UPSTREAM connection mounts the assembly
/// attempted for reused streams — one mount serves every consumer of that
/// stream (the reuse pipeline's falsifiable claim: the count stays 1 per
/// source stream, not 1-per-venue).
///
/// Field lock discipline: each cell is an independent short critical section,
/// never held across an await — the same shape as `active_plugin`/`poll_admission`.
#[derive(Clone, Default)]
pub struct FeedReuse {
    /// Who eats whose stream, from the plugins' own declarations (the session
    /// assembly resolves them once; runtime binds append through
    /// [`FeedReuse::add_edge`]). Order = registration order.
    pub edges: std::sync::Arc<std::sync::RwLock<Vec<FeedReuseEdge>>>,
    /// Every attempt to mount a SOURCE's upstream components (feed and
    /// discovery separately) through the REUSE pipeline. A consumer-only
    /// enablement mounts the source's data plane through this path too
    /// (data-plane self-sufficiency), so this counter is the reuse pipeline's
    /// observable: N venues enabled ⇒ still 1 per source stream.
    pub mounts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FeedReuse {
    /// Append a declared edge (idempotent — the same declaration re-bound is
    /// a no-op). Returns true when the edge was newly added.
    pub fn add_edge(&self, consumer: &str, source: &str) -> bool {
        let mut edges = self.edges.write().expect("feed reuse edges lock poisoned");
        if edges
            .iter()
            .any(|e| e.consumer == consumer && e.source == source)
        {
            return false;
        }
        edges.push(FeedReuseEdge {
            consumer: consumer.to_string(),
            source: source.to_string(),
        });
        true
    }

    /// The consumers whose declaration names `source` (registration order).
    /// Empty when none — the caller forwards once, the historical shape.
    pub fn consumers_of(&self, source: &str) -> Vec<String> {
        self.edges
            .read()
            .expect("feed reuse edges lock poisoned")
            .iter()
            .filter(|e| e.source == source)
            .map(|e| e.consumer.clone())
            .collect()
    }

    /// Whether `consumer`'s declaration names `source`.
    pub fn consumes(&self, consumer: &str, source: &str) -> bool {
        self.edges
            .read()
            .expect("feed reuse edges lock poisoned")
            .iter()
            .any(|e| e.consumer == consumer && e.source == source)
    }
}

// ── DTO ↔ core-model conversions ─────────────────────────────────────────────

pub fn market_from_api(d: &api::MarketDescriptor) -> m::CryptoMarket {
    m::CryptoMarket {
        asset: d.asset.clone(),
        condition_id: d.condition_id.clone(),
        question_id: d.question_id.clone(),
        up_token_id: d.up_token_id.clone(),
        down_token_id: d.down_token_id.clone(),
        up_price: d.up_price,
        down_price: d.down_price,
        expires_at_ms: d.expires_at_ms,
        round_slot: d.round_slot,
        // Live markets ride the configured grid (the scanner slots off
        // `round_duration_sec`), and the API descriptor declares no duration —
        // `0` keeps the #354 cadence gate silent on the live path, where the
        // grid IS the assumption.
        round_duration_sec: 0,
        // #377 (治 #2): a live market has no archive verdict — the venue
        // answers settlement queries itself.
        archive_verdict: false,
        // #427: the plugin's own identity rides through as data.
        venue: d.venue.clone(),
        neg_risk: d.neg_risk,
        question: d.question.clone(),
    }
}

pub fn error_from_api(e: &api::CoreError) -> m::CoreError {
    let mut out = m::CoreError::new(error_code_from_api(e.code), e.message.clone());
    out.raw = e.raw.clone();
    out
}

fn error_code_from_api(c: api::CoreErrorCode) -> m::CoreErrorCode {
    use api::CoreErrorCode as A;
    use m::CoreErrorCode as C;
    match c {
        A::InvalidParams => C::InvalidParams,
        A::UnknownOrder => C::UnknownOrder,
        A::InvalidOrderState => C::InvalidOrderState,
        A::WouldCross => C::WouldCross,
        A::InvalidTickSize => C::InvalidTickSize,
        A::InvalidSize => C::InvalidSize,
        A::InsufficientFunds => C::InsufficientFunds,
        A::RiskRejected => C::RiskRejected,
        A::AccountLimit => C::AccountLimit,
        A::KillSwitchActive => C::KillSwitchActive,
        A::MarketHalted => C::MarketHalted,
        A::NotAuthenticated => C::NotAuthenticated,
        A::VenueError => C::VenueError,
        A::Timeout => C::Timeout,
        A::Internal => C::Internal,
    }
}

fn pending_to_api(o: &m::TrackedOrder) -> api::PendingOrder {
    api::PendingOrder {
        core_order_id: o.order_id.clone(),
        token_id: o.token_id.clone(),
        condition_id: o.condition_id.clone(),
        side: o.side,
        fill_policy: o.mode,
        price: o.price,
        size: o.size,
        internal_key: o.internal_key.clone(),
        strategy: o.strategy.clone(),
        asset: o.asset.clone(),
        direction: o.direction.clone(),
        round_slot: o.round_slot,
    }
}

fn fill_from_api(f: &api::MarketFill) -> m::Fill {
    m::Fill {
        order_id: f.order_id.clone(),
        trade_id: f.trade_id.clone(),
        token_id: f.token_id.clone(),
        side: f.side,
        price: f.price,
        size: f.size,
        status: f.status,
        ts_ms: f.ts_ms,
        tx_hash: f.tx_hash.clone(),
        // The venue's own maker/taker report rides straight through to the OME.
        maker: f.maker,
        // E28: `api::MarketFill` carries no account yet — the market-plugin
        // ABI addition is deferred (disclosed in the E28 PR), so a
        // venue-routed fill lands in the `default` book. Core-internal fills
        // (dry matcher, reconciliation) copy the account from their tracked
        // order instead, so multi-account routing does not depend on this hop.
        account_id: m::default_account_id(),
    }
}

fn snapshot_from_api(s: &api::ReconcileSnapshot) -> crate::reconcile::VenueSnapshot {
    crate::reconcile::VenueSnapshot {
        open_order_ids: s.open_order_ids.clone(),
        trades: s
            .trades
            .iter()
            .map(|t| crate::reconcile::VenueTrade {
                venue_order_id: t.venue_order_id.clone(),
                trade_id: t.trade_id.clone(),
                token_id: t.token_id.clone(),
                side: t.side,
                size: t.size,
                price: t.price,
                ts_ms: t.ts_ms,
                tx_hash: t.tx_hash.clone(),
                // The venue's role report rides through to the gap-fill synthesis.
                maker: t.maker,
            })
            .collect(),
        now_ms: s.now_ms,
    }
}

// ── Host ─────────────────────────────────────────────────────────────────────

/// The poll-admission gate's type: a sync predicate over one discovery-poll
/// batch, verdict = keep/drop. Shared as `Arc` so the host stays `Clone`
/// without cloning the cell.
pub type PollAdmissionGate =
    std::sync::Arc<dyn Fn(Vec<api::MarketDescriptor>) -> bool + Send + Sync>;

/// Implements [`MarketHost`] over the shared `Core`. Cloneable so each plugin
/// component can hold its own handle (hand-written `Clone`: the poll-admission
/// cell is a `Mutex<Option<Arc<_>>>`, and `Arc` already shares the gate — a
/// clone re-sees the installed one).
#[derive(Clone)]
pub struct CoreHost {
    core: Arc<AsyncMutex<Core>>,
    /// This handle's OWN plugin identity (set only by [`CoreHost::for_plugin`];
    /// the shared data-plane handle carries `""`). Scoped drains may compare
    /// against it — the settlement queues are venue-bucketed and carry no
    /// caller name across the frozen [`MarketHost`] ABI, so they need the
    /// handle to know itself.
    own_plugin: String,
    /// The SHARED "who owns untagged orders" cell, resolved once at assembly
    /// ([`CoreHost::set_active_plugin`]). Untagged round lists (legacy corpora,
    /// hosts that do not label markets) route their subscription here, and an
    /// order whose execution venue resolves to nothing drains to this plugin's
    /// executor — the single-plugin fallback. Crucially a per-plugin handle
    /// does NOT overwrite this: every executor registering itself into the
    /// shared cell would make every executor believe it owns every untagged
    /// order (issue #775).
    active_plugin: std::sync::Arc<std::sync::RwLock<String>>,
    /// B2 (轮次多族 软卸): an optional gate the session assembly installs —
    /// `None` keeps `on_round_markets` byte-identical with the historical
    /// forward (every in-process test, backtest replay and legacy host path).
    /// When present, every discovery-poll batch passes through it BEFORE the
    /// engine sees anything: a batch whose (venue, duration) family has been
    /// soft-unloaded (removed from every account's active family set) is
    /// dropped here — the instance keeps polling, its output never lands.
    /// The closure carries the session's `DataPlaneRuntime` (the active-family
    /// computation and the gauge live there); the host stays data-plane-blind.
    /// `Arc` inside the Mutex makes the whole cell trivially shareable.
    poll_admission: std::sync::Arc<std::sync::Mutex<Option<PollAdmissionGate>>>,
    /// RFC-661 M2: the `feed_source` reuse fan-out. `None` (the never-called
    /// default) keeps every host path byte-identical with the historical
    /// single-registration shape — in-process tests, replays and backtests.
    /// When the session assembly installs it (an active or bound plugin
    /// declares a `feed_source`), each upstream round batch is re-registered
    /// under every consuming venue's own identity, and each upstream book /
    /// top-of-book update is applied once per venue token copy (the re-arm
    /// `on_round_markets` ran the venue re-tags through the engine's same-slot
    /// merge, so the consumer's token copies are engine-resident and each
    /// application lands on its own row).
    feed_reuse: std::sync::Arc<std::sync::Mutex<Option<FeedReuse>>>,
}

impl CoreHost {
    pub fn new(core: Arc<AsyncMutex<Core>>) -> Self {
        Self {
            core,
            own_plugin: String::new(),
            active_plugin: std::sync::Arc::new(std::sync::RwLock::new(String::new())),
            poll_admission: std::sync::Arc::new(std::sync::Mutex::new(None)),
            feed_reuse: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Assembly sets the active plugin name after selection (empty string =
    /// unknown: callers keep the historical fallback).
    pub fn set_active_plugin(&self, name: &str) {
        *self
            .active_plugin
            .write()
            .expect("core host active plugin lock poisoned") = name.to_string();
    }

    /// RFC-661 M2: install the `feed_source` fan-out. Called once by the
    /// session assembly AFTER it resolved the enablement graph (boot actives
    /// seed the declared edges; any later runtime bind appends consumers
    /// through [`FeedReuse::add_edge`] on the SAME shared cell). The default
    /// (never called) is None — every path stays the historical
    /// single-registration shape.
    pub fn set_feed_reuse(&self, reuse: FeedReuse) {
        *self
            .feed_reuse
            .lock()
            .expect("core host feed reuse lock poisoned") = Some(reuse);
    }

    /// The fan-out cell for callers that must append a runtime-declared edge
    /// (`enable_venue_at_runtime`'s consumer arm). `None` when the assembly
    /// never installed one — a session with no reuse plane keeps the
    /// historical mounts and registers nothing.
    pub fn feed_reuse(&self) -> Option<FeedReuse> {
        self.feed_reuse
            .lock()
            .expect("core host feed reuse lock poisoned")
            .clone()
    }

    /// The consumer venues whose declaration names `source`'s stream — the
    /// list one upstream round batch re-registers under. Empty when the reuse
    /// cell is absent or nothing consumes `source`: the caller forwards once,
    /// the historical shape.
    fn feed_fanout_for(&self, source: &str) -> Vec<String> {
        self.feed_reuse()
            .map(|r| r.consumers_of(source))
            .unwrap_or_default()
    }

    /// #647 / #775: a handle naming ONE plugin for the executor's drains.
    ///
    /// - `own_plugin` is set to `name`: the settlement queues are
    ///   venue-bucketed inside the host and the trait's
    ///   `take_pending_redemptions`/`take_pending_merges` carry no caller name
    ///   (frozen ABI), so the handle must know itself to pull only its own
    ///   venue's claims.
    /// - `active_plugin` is SHARED (`Arc`) and deliberately NOT touched: that
    ///   cell answers "who owns untagged orders" for the whole host. A
    ///   per-handle overwrite made every executor compare itself equal to the
    ///   active owner, so every executor drained every untagged order and the
    ///   same order was submitted to multiple venues (issue #775).
    ///
    /// Data-plane handles (`clone`) keep `own_plugin == ""` and the shared
    /// active-plugin answer, exactly as before.
    pub fn for_plugin(&self, name: &str) -> Self {
        Self {
            core: Arc::clone(&self.core),
            own_plugin: name.to_string(),
            active_plugin: Arc::clone(&self.active_plugin),
            poll_admission: std::sync::Arc::clone(&self.poll_admission),
            feed_reuse: std::sync::Arc::clone(&self.feed_reuse),
        }
    }

    /// B2 (轮次多族 软卸): install the discovery poll-output gate. Called once
    /// by the session assembly AFTER the data plane exists; the default
    /// (never called) is admit-everything — the historical shape.
    pub fn set_poll_admission(&self, gate: PollAdmissionGate) {
        *self
            .poll_admission
            .lock()
            .expect("core host poll admission lock poisoned") = Some(gate);
    }
}

impl MarketHost for CoreHost {
    fn on_book(&self, update: api::BookUpdate) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let now = now_ms();
            // RFC-661 M2: an upstream book applies once per venue identity.
            // The source's token row and each consumer's re-registered copy
            // (the fan-out `on_round_markets` armed them) all live in the
            // engine's book map under the SAME byte token — one application
            // per event is the single-connection claim; the engine's per-venue
            // rows read the same book. No extra apply is needed: the fan-out
            // is a ROW re-registration, not a book duplication (books stay
            // keyed by token, M1's dual-key rows both point at it).
            let mut c = self.core.lock().await;
            c.engine_on_data(
                crate::engine::DataEvent::Book {
                    token_id: update.token_id,
                    bids: update.bids,
                    asks: update.asks,
                    now_ms: update.ts_ms,
                },
                now,
            );
        })
    }

    /// 盘口批次原子化：整批 book 在同一次 `core.lock()` 区间里应用完。
    /// 撕裂的机理正是旧路径的「每本一把锁」—— 两次锁的间隙里维护 tick
    /// （`engine_evaluate`）可以插进来，读到 up 腿是本批、down 腿还是上一批。
    /// 批内共享一个批次时间戳（`ts_ms`），feed 层保证它来自同一次拉取。
    fn on_books<'a>(&'a self, updates: Vec<api::BookUpdate>, ts_ms: i64) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut c = self.core.lock().await;
            c.engine_on_books(
                updates
                    .into_iter()
                    .map(|u| (u.token_id, u.bids, u.asks))
                    .collect(),
                ts_ms,
            );
        })
    }

    fn on_top_of_book(&self, update: api::TopOfBookUpdate) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let now = now_ms();
            let mut c = self.core.lock().await;
            c.engine_on_data(
                crate::engine::DataEvent::TopOfBook {
                    token_id: update.token_id,
                    best_bid: update.best_bid,
                    best_ask: update.best_ask,
                    now_ms: update.ts_ms,
                },
                now,
            );
        })
    }

    fn on_spot(&self, update: api::SpotUpdate) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let now = now_ms();
            let mut c = self.core.lock().await;
            c.engine_on_data(
                crate::engine::DataEvent::Spot {
                    asset: update.asset,
                    price: update.price,
                    now_ms: update.ts_ms,
                },
                now,
            );
        })
    }

    fn on_round_markets(&self, markets: Vec<api::MarketDescriptor>) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // B2 (轮次多族 软卸): the poll-output gate, when the assembly
            // installed one. A soft-unloaded family's batch is dropped
            // BEFORE any state moves — no engine event, no subscription.
            // Default (no gate): admit-everything, the historical shape.
            if let Some(gate) = self
                .poll_admission
                .lock()
                .expect("core host poll admission lock poisoned")
                .as_ref()
                .cloned()
            {
                let keep = gate(markets.clone());
                if !keep {
                    return;
                }
            }
            let now = now_ms();
            let models: Vec<m::CryptoMarket> = markets.iter().map(market_from_api).collect();
            let tokens: Vec<String> = models
                .iter()
                .flat_map(|mk| [mk.up_token_id.clone(), mk.down_token_id.clone()])
                .collect();
            // The plugin's own name: multi-plugin data planes key feeds (and
            // the #498 top-up predicate) on the venue, so the subscription for
            // this batch lands on the CALLING plugin's control — never on a
            // sibling venue's. Untagged lists (legacy corpora, hosts that do
            // not label markets) fall back to the registry's active plugin —
            // the single-plugin behaviour; an empty fallback keeps "" (the
            // legacy feed bucket) rather than guessing a venue.
            let fallback = self
                .active_plugin
                .read()
                .expect("core host active plugin lock poisoned")
                .clone();
            let plugin = markets
                .first()
                .map(|m| m.venue.as_str())
                .filter(|v| !v.is_empty())
                .unwrap_or(fallback.as_str());
            // Single lock: register the round, then subscribe its tokens (matches
            // the historical ordering exactly).
            let mut c = self.core.lock().await;
            c.engine_on_data(
                crate::engine::DataEvent::RoundMarkets {
                    markets: models,
                    now_ms: now,
                },
                now,
            );
            drop(c);
            self.subscribe_tokens(plugin, tokens).await;
            // RFC-661 M2 (feed_source 复用管道): one upstream round batch feeds
            // every consumer venue. The source's own registration above is the
            // historical forward; each consumer re-registers the SAME batch
            // under ITS OWN venue tag so the engine's scanner holds the
            // consumer's own `(venue, token)` rows (M1's dual-key table). The
            // consumer registration lands AFTER the source's — the engine's
            // same-slot merge then admits the consumer's batch as fresh rows
            // (its venue tag differs, its tokens as byte strings are already
            // known but its venue identity is its own). The consumer's tokens
            // subscribe on ITS OWN feed control when it somehow carries one
            // (an inert no-op for the parasitic shape, which mounts none —
            // the subscription rides the source's upstream); the source's
            // control is deliberately NOT re-poked here: its set_tokens is
            // REPLACE semantics, and a consumer's token copy would evict the
            // source's own set (one upstream connection serving one union —
            // the source re-subscribes on its own rollovers).
            let fanout = self.feed_fanout_for(plugin);
            for consumer in fanout {
                let mut re_tagged: Vec<api::MarketDescriptor> = markets.clone();
                for d in re_tagged.iter_mut() {
                    d.venue = consumer.clone();
                }
                let consumer_tokens: Vec<String> = re_tagged
                    .iter()
                    .flat_map(|d| [d.up_token_id.clone(), d.down_token_id.clone()])
                    .collect();
                self.core.lock().await.engine_on_data(
                    crate::engine::DataEvent::RoundMarkets {
                        markets: re_tagged.iter().map(market_from_api).collect(),
                        now_ms: now,
                    },
                    now,
                );
                self.subscribe_tokens(&consumer, consumer_tokens).await;
            }
        })
    }

    fn subscribe_tokens<'a>(&'a self, plugin: &str, tokens: Vec<String>) -> BoxFuture<'a, ()> {
        let plugin = plugin.to_string();
        Box::pin(async move {
            self.core
                .lock()
                .await
                .subscribe_feed_tokens(&plugin, tokens)
                .await;
        })
    }

    fn install_subscription_control<'a>(
        &'a self,
        plugin: &str,
        control: Arc<dyn api::SubscriptionControl>,
    ) -> BoxFuture<'a, ()> {
        let plugin = plugin.to_string();
        Box::pin(async move {
            self.core.lock().await.set_subscription(&plugin, control);
        })
    }

    fn take_pending_orders<'a>(&'a self, plugin: &str) -> BoxFuture<'a, Vec<api::PendingOrder>> {
        let plugin = plugin.to_string();
        Box::pin(async move {
            let c = self.core.lock().await;
            // Per-venue executor drain (multi-plugin): each executor pulls only
            // the orders whose EXECUTION venue names ITS plugin — the placing
            // account's binding resolves its OWN (venue, token) dual-key row
            // (RFC-661 M1: a shared token carries one row per venue, so the
            // account dimension is what keeps the poly account's order off
            // the REAL venue's executor, and the reverse). An order whose
            // execution venue resolves to nothing is UNTAGGED and routes to
            // the ACTIVE plugin's drain — the single-plugin fallback the
            // task pins. `active` is the SHARED assembly cell (issue #775):
            // a per-handle copy would make every executor believe it is the
            // active owner, and every untagged order would be submitted by
            // every executor. When no active plugin was ever set (hosts that
            // skipped `set_active_plugin`, most in-process tests), the
            // legacy "" bucket takes ALL orders: the exact pre-routing
            // behaviour. Single pass over `live_orders()` order, so the
            // drain order the historical bridge saw is untouched.
            let active = self
                .active_plugin
                .read()
                .expect("core host active plugin lock poisoned")
                .clone();
            let legacy_all = active.is_empty();
            c.pending_unbound()
                .iter()
                .filter(|o| {
                    let venue = c.execution_venue_of(o.account_id.as_str(), &o.token_id);
                    legacy_all || venue == plugin || (venue.is_empty() && plugin == active)
                })
                .map(pending_to_api)
                .collect()
        })
    }

    fn take_pending_cancels<'a>(&'a self, plugin: &str) -> BoxFuture<'a, Vec<String>> {
        let plugin = plugin.to_string();
        Box::pin(async move {
            let mut c = self.core.lock().await;
            // Per-venue cancel drain: the queue is take-once, so with multiple
            // executors each must pull only ITS venue's ids — a sibling
            // swallowing them first would strand the cancel forever (the
            // resting orphan). Untagged ids (order placed before the round
            // list carried a venue label) fall back to the ACTIVE plugin's
            // drain, matching the orders-drain fallback; hosts that never set
            // an active plugin get the whole queue under the legacy "" bucket
            // — the pre-routing behaviour.
            let active = self
                .active_plugin
                .read()
                .expect("core host active plugin lock poisoned")
                .clone();
            let untagged_fallback = plugin == active && !active.is_empty();
            let legacy_all = active.is_empty();
            if legacy_all {
                c.take_pending_venue_cancels()
            } else {
                c.take_pending_venue_cancels_for_venue(&plugin, untagged_fallback)
            }
        })
    }

    fn on_order_accepted(&self, core_order_id: &str, venue_order_id: &str) -> BoxFuture<'_, ()> {
        let (core_id, venue_id) = (core_order_id.to_string(), venue_order_id.to_string());
        Box::pin(async move {
            let mut c = self.core.lock().await;
            if let Err(e) = c.bind_venue(&core_id, venue_id, now_ms()) {
                c.emit_error(e);
            }
        })
    }

    fn on_order_rejected(
        &self,
        core_order_id: &str,
        error: Option<api::CoreError>,
    ) -> BoxFuture<'_, ()> {
        let core_id = core_order_id.to_string();
        Box::pin(async move {
            let err = error.map(|e| error_from_api(&e));
            let mut c = self.core.lock().await;
            if let Err(e) = c.reject_live_result(&core_id, err, now_ms()) {
                c.emit_error(e);
            }
        })
    }

    fn on_place_timeout(&self, core_order_id: &str) -> BoxFuture<'_, ()> {
        let core_id = core_order_id.to_string();
        Box::pin(async move {
            let mut c = self.core.lock().await;
            c.note_place_timeout(&core_id);
        })
    }

    fn on_fill(&self, fill: api::MarketFill) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            eprintln!(
                "core: ws fill: order={} size={} price={} maker={:?}",
                fill.order_id, fill.size, fill.price, fill.maker
            );
            let mut c = self.core.lock().await;
            if let Err(e) = c.ingest_fill(fill_from_api(&fill), now_ms()) {
                c.emit_error(e);
            }
        })
    }

    fn on_order_live(&self, venue_order_id: &str) -> BoxFuture<'_, ()> {
        let venue_id = venue_order_id.to_string();
        Box::pin(async move {
            let mut c = self.core.lock().await;
            if let Some(core_id) = c.core_id_for_venue(&venue_id) {
                let _ = c.confirm_live(&core_id, now_ms());
            }
        })
    }

    fn on_order_cancelled(&self, venue_order_id: &str) -> BoxFuture<'_, ()> {
        let venue_id = venue_order_id.to_string();
        Box::pin(async move {
            let mut c = self.core.lock().await;
            if let Some(core_id) = c.core_id_for_venue(&venue_id) {
                let _ = c.cancel(&core_id, now_ms());
            }
        })
    }

    fn on_reconcile(&self, snapshot: api::ReconcileSnapshot) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // This handle is the executor's own bucket (`for_plugin`), so the
            // name it carries IS the venue that fetched this snapshot — the
            // sweep is authoritative for that venue only.
            let venue = self
                .active_plugin
                .read()
                .expect("core host active plugin lock poisoned")
                .clone();
            let mut c = self.core.lock().await;
            // Trades for KNOWN orders go through the OME reconcile; trades for
            // orders the core never issued (manual closes on the venue) ride
            // through as well — the core folds them into the position book by
            // token id instead of dropping them (the ghost-position bug).
            let snap = snapshot_from_api(&snapshot);
            let tracked = snap
                .trades
                .iter()
                .filter(|t| c.core_id_for_venue(&t.venue_order_id).is_some())
                .count();
            eprintln!(
                "core: reconcile: trades={} tracked={} untracked/manual={}",
                snap.trades.len(),
                tracked,
                snap.trades.len() - tracked
            );
            match c.reconcile_for_venue(snap, &venue) {
                Ok(report) => {
                    for a in &report.actions {
                        eprintln!("core: reconcile action: {a:?}");
                    }
                    if !report.suspect_ghost_ids.is_empty() {
                        eprintln!("core: reconcile ghosts: {:?}", report.suspect_ghost_ids);
                    }
                }
                Err(e) => c.emit_error(e),
            }
        })
    }

    fn on_reconcile_failed<'a>(
        &'a self,
        venue: &'a str,
        error: api::CoreError,
    ) -> BoxFuture<'a, ()> {
        let e = error_from_api(&error);
        Box::pin(async move {
            let mut c = self.core.lock().await;
            c.on_venue_reconcile_failed(venue, e, now_ms());
        })
    }

    fn on_self_check<'a>(
        &'a self,
        venue: &'a str,
        report: api::SelfCheckReport,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.core.lock().await.on_venue_self_check(venue, report);
        })
    }

    fn seed_balance(&self, venue: &str, balance: rust_decimal::Decimal) -> BoxFuture<'_, ()> {
        let venue = venue.to_string();
        Box::pin(async move {
            self.core.lock().await.realign_venue_cash(&venue, balance);
        })
    }

    fn venue_free_balance(&self, venue: &str, free: rust_decimal::Decimal) -> BoxFuture<'_, ()> {
        let venue = venue.to_string();
        Box::pin(async move {
            // The reserved-aware fold (drift killer / drift surface) lives on
            // Core and routes by the venue's own bound account (#532).
            self.core.lock().await.fold_venue_free(&venue, free);
        })
    }

    fn known_venue_order_ids(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move {
            self.core
                .lock()
                .await
                .known_venue_ids()
                .into_iter()
                .collect()
        })
    }

    fn note_orphan_cancelled(&self, venue_order_id: &str) -> BoxFuture<'_, ()> {
        let id = venue_order_id.to_string();
        Box::pin(async move {
            self.core.lock().await.note_orphan_cancelled(&id);
        })
    }

    fn report_error(&self, error: api::CoreError) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.core.lock().await.emit_error(error_from_api(&error));
        })
    }

    // ── Settlement & redemption (issue #175) ─────────────────────────────────

    fn take_settlement_queries(&self) -> BoxFuture<'_, Vec<api::SettlementQuery>> {
        Box::pin(async move {
            let now = now_ms();
            let mut c = self.core.lock().await;
            c.take_settlement_queries(now)
        })
    }

    fn on_market_resolution(&self, resolution: api::MarketResolution) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let now = now_ms();
            let mut c = self.core.lock().await;
            c.on_market_resolution(resolution, now);
        })
    }

    fn take_pending_redemptions(&self) -> BoxFuture<'_, Vec<api::RedemptionRequest>> {
        // #647: the request now carries its own venue, and this drain hands
        // out only ITS bucket — the handle's OWN plugin name (`for_plugin`
        // sets it at assembly; the shared data-plane handle carries ""). It
        // is deliberately NOT the shared `active_plugin` cell (issue #775):
        // that cell is host-wide, and every executor reading it would drain
        // the same requests. Legacy hosts that never set a bucket name take
        // ALL claims: the pre-routing single-consumer behaviour,
        // byte-identical. A sibling executor can therefore never redeem a
        // foreign venue's claim (the old global-queue cross-post burned only
        // a retry — the bytes32 parse or zero-payout audit refused it — but
        // the retry-and-alert churn is exactly what this split ends).
        Box::pin(async move {
            let now = now_ms();
            let bucket = self.own_plugin.clone();
            let mut c = self.core.lock().await;
            if bucket.is_empty() {
                c.take_pending_redemptions(now)
            } else {
                // The handle's own bucket is also the untagged fallback: a
                // claim stamped before the venue labels existed (legacy
                // journal) drains to the single active plugin, as before.
                c.take_pending_redemptions_for_venue(&bucket, true, now)
            }
        })
    }

    fn on_redemption_result(&self, result: api::RedemptionResult) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let now = now_ms();
            let mut c = self.core.lock().await;
            c.on_redemption_result(result, now);
        })
    }

    fn take_pending_merges(&self) -> BoxFuture<'_, Vec<api::MergeRequest>> {
        // #647: same verdict as `take_pending_redemptions` — the request
        // carries its own venue, this drain hands out only its bucket (the
        // handle's OWN plugin name from `for_plugin`, never the shared
        // assembly-wide `active_plugin` cell; empty = legacy all-take), and a
        // foreign condition_id merge (burned nothing, reverts no-op) can no
        // longer be handed to a sibling venue's bridge at all.
        Box::pin(async move {
            let now = now_ms();
            let bucket = self.own_plugin.clone();
            let mut c = self.core.lock().await;
            if bucket.is_empty() {
                c.take_pending_merges(now)
            } else {
                c.take_pending_merges_for_venue(&bucket, true, now)
            }
        })
    }

    fn on_merge_result(&self, result: api::MergeResult) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let now = now_ms();
            let mut c = self.core.lock().await;
            c.on_merge_result(result, now);
        })
    }
}

#[cfg(test)]
mod tests;
