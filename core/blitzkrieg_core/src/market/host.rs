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
    /// The registry's active plugin name (the executor's plugin), resolved at
    /// assembly. Untagged round lists (legacy corpora, hosts that do not
    /// label markets) route their subscription here — the single-plugin
    /// behaviour — instead of a hardcoded plugin name.
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

    /// #647: a handle naming ONE plugin for the settlement drains. The
    /// settlement queues are venue-bucketed inside the host, and the trait's
    /// `take_pending_redemptions`/`take_pending_merges` carry no caller name
    /// (frozen ABI — the signature stays), so the session assembly hands each
    /// executor a handle whose bucket cell names ITS OWN plugin: the executor
    /// drains only the claims whose request venue names that plugin (plus the
    /// untagged ones, the single-plugin fallback the orders drain uses). The
    /// cell is per-handle (a fresh `RwLock`), unlike the shared cell a plain
    /// `clone` re-sees, so sibling executors never inherit each other's
    /// bucket; data-plane handles (`clone`, the assembly default) keep the
    /// assembly's active-plugin answer exactly as before.
    pub fn for_plugin(&self, name: &str) -> Self {
        Self {
            core: Arc::clone(&self.core),
            active_plugin: std::sync::Arc::new(std::sync::RwLock::new(name.to_string())),
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
            // task pins. When no active plugin was ever set (hosts that
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
            match c.reconcile(snap) {
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
        // out only ITS bucket — the handle's plugin name (set at assembly:
        // each executor gets a `for_plugin` handle naming itself, the shared
        // data-plane handle names the active plugin). Legacy hosts that never
        // set a bucket name take ALL claims: the pre-routing single-consumer
        // behaviour, byte-identical. A sibling executor can therefore never
        // redeem a foreign venue's claim (the old global-queue cross-post
        // burned only a retry — the bytes32 parse or zero-payout audit refused
        // it — but the retry-and-alert churn is exactly what this split ends).
        Box::pin(async move {
            let now = now_ms();
            let bucket = self
                .active_plugin
                .read()
                .expect("core host active plugin lock poisoned")
                .clone();
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
        // handle's plugin name; empty = legacy all-take), and a foreign
        // condition_id merge (burned nothing, reverts no-op) can no longer be
        // handed to a sibling venue's bridge at all.
        Box::pin(async move {
            let now = now_ms();
            let bucket = self
                .active_plugin
                .read()
                .expect("core host active plugin lock poisoned")
                .clone();
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
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    use crate::model::{Mode, OrderRequest};
    use crate::risk::RiskConfig;
    use crate::service::{Core, CoreConfig};
    use blitzkrieg_market_api::{FillPolicy, MarketType};

    /// A LIVE core with the engine installed and TWO labelled round markets —
    /// `poly-up`/`poly-down` on venue `polymarket` and `kal-up`/`kal-down` on
    /// venue `kalshi` — so a pending order's token carries a venue to route
    /// by. Books are seeded for both tokens so the entries can rest.
    fn two_venue_live_core() -> Core {
        let mut c = Core::new(CoreConfig {
            mode: Mode::Live,
            risk: RiskConfig {
                max_order_notional: dec!(3),
                ..Default::default()
            },
            dry_seed_balance: dec!(10_000),
            round_duration_sec: 900,
            // These tests PLACE live orders: the default config's relative
            // data/ log paths would append into the process cwd (parallel
            // tests move it), re-dirtying the reset tests' self-check. A
            // fixture that trades carries no durable logs.
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            ..Default::default()
        });
        c.set_balance(dec!(10));
        c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig {
            scanner: crate::scanner::ScannerConfig {
                assets: vec!["BTC".into()],
                round_duration_sec: 900,
                min_round_age_sec: 0,
                min_time_left_sec: 0,
            },
            ..Default::default()
        }));
        let now: i64 = 1_000_000;
        let slot = now / 1000 / 900;
        let market = |venue: &str, up: &str, down: &str| crate::model::CryptoMarket {
            asset: "BTC".into(),
            condition_id: format!("cond-{venue}"),
            question_id: "q".into(),
            up_token_id: up.into(),
            down_token_id: down.into(),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: (slot + 1) * 900 * 1000,
            round_slot: slot,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: venue.into(),
            neg_risk: false,
            question: "?".into(),
        };
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![
                    market("polymarket", "poly-up", "poly-down"),
                    market("kalshi", "kal-up", "kal-down"),
                ],
                now_ms: now,
            },
            now,
        );
        c
    }

    fn live_order(token: &str, key: &str) -> OrderRequest {
        OrderRequest {
            token_id: token.into(),
            condition_id: "cond".into(),
            side: crate::model::Side::Buy,
            mode: FillPolicy::Maker,
            price: dec!(0.40),
            size: dec!(1),
            internal_key: key.into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
            account_id: crate::model::default_account_id(),
        }
    }

    #[tokio::test]
    async fn pending_orders_route_by_the_token_round_venue() {
        let mut c = two_venue_live_core();
        // One maker per venue — both rest Pending→Live... (LIVE mode keeps the
        // order Pending until the venue acks, which is exactly the unbound
        // queue the executors drain).
        let (poly_id, _) = c.place(live_order("poly-up", "k1"), 0, 1).unwrap();
        let (kal_id, _) = c.place(live_order("kal-up", "k2"), 0, 2).unwrap();

        let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
        host.set_active_plugin("polymarket");

        // Each executor drains ONLY its own venue's orders.
        let poly_drain = host.take_pending_orders("polymarket").await;
        assert_eq!(
            poly_drain
                .iter()
                .map(|o| o.core_order_id.as_str())
                .collect::<Vec<_>>(),
            vec![poly_id.as_str()],
            "the polymarket executor must take only the polymarket order"
        );
        let kal_drain = host.take_pending_orders("kalshi").await;
        assert_eq!(
            kal_drain
                .iter()
                .map(|o| o.core_order_id.as_str())
                .collect::<Vec<_>>(),
            vec![kal_id.as_str()],
            "the kalshi executor must take only the kalshi order — cross-venue \
             orders must never leak into the sibling drain"
        );
        // The drain is a per-tick RE-READ, not take-once: an order leaves the
        // pending set only when its venue id binds (`on_order_accepted`). So
        // re-draining returns the SAME order — the routing property is who
        // sees what, never a consume-once partition.
        let again = host.take_pending_orders("polymarket").await;
        assert_eq!(
            again
                .iter()
                .map(|o| o.core_order_id.as_str())
                .collect::<Vec<_>>(),
            vec![poly_id.as_str()],
            "re-draining before the venue acks re-offers the same order"
        );
        assert_eq!(
            host.take_pending_orders("kalshi")
                .await
                .iter()
                .map(|o| o.core_order_id.as_str())
                .collect::<Vec<_>>(),
            vec![kal_id.as_str()]
        );
    }

    /// Dual-key routing at the production drain seam: a byte-identical token
    /// carries one row per venue, and the EXECUTOR that must pull the order
    /// is the placing account's own venue — a polysimulator account's
    /// poly-up order can never land on the real polymarket drain (nor the
    /// reverse).
    #[tokio::test]
    async fn mirror_tokens_drain_to_the_placing_account_venue() {
        let mut c = two_venue_live_core();
        // The simulator's batch declares the SAME tokens polymarket already
        // listed: a same-slot merge whose fresh set is the sim venue's OWN
        // tokens — the rows land under their own (venue, token) keys
        // (RFC-661 M1). The drain keys on the placing binding's own row; no
        // declaration participates.
        let now: i64 = 1_000_001;
        let slot = now / 1000 / 900;
        let sim = crate::model::CryptoMarket {
            asset: "BTC".into(),
            condition_id: "cond-polymarket".into(),
            question_id: "q".into(),
            up_token_id: "poly-up".into(),
            down_token_id: "poly-down".into(),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: (slot + 1) * 900 * 1000,
            round_slot: slot,
            round_duration_sec: 900,
            archive_verdict: false,
            venue: "polysimulator".into(),
            neg_risk: false,
            question: "?".into(),
        };
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![sim],
                now_ms: now,
            },
            now,
        );
        // A second account bound to the mirror venue places on poly-up.
        let book = crate::account::Account {
            id: crate::account::AccountId::from("sim"),
            name: "sim".into(),
            market_type: blitzkrieg_market_api::MarketType::Prediction,
            status: crate::account::AccountStatus::Active,
            credential_keys: crate::account::CredentialKeys::default(),
            venue: "polysimulator".into(),
            trading_enabled: true,
            strategies: Vec::new(),
            rounds: Vec::new(),
            updated_at_ms: 0,
        };
        c.accounts_mut().insert(book).unwrap();
        c.accounts_mut()
            .get_mut(&crate::account::AccountId::from("sim"))
            .unwrap()
            .set_balance(dec!(10));
        c.engine_evaluate(now + 1);
        let mut order = live_order("poly-up", "sim-1");
        order.account_id = crate::account::AccountId::from("sim");
        let (sim_id, _) = c.place(order, 0, 2).unwrap();

        let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
        host.set_active_plugin("polysimulator");

        // The simulator executor takes its account's order; the REAL venue's
        // executor does not — this is the live-money misroute the table-row
        // drain would have made.
        assert_eq!(
            host.take_pending_orders("polysimulator")
                .await
                .iter()
                .map(|o| o.core_order_id.as_str())
                .collect::<Vec<_>>(),
            vec![sim_id.as_str()],
            "the mirror account's order drains to polysimulator, not the real venue"
        );
    }

    #[tokio::test]
    async fn untagged_orders_fall_back_to_the_active_plugin_drain() {
        let mut c = two_venue_live_core();
        // A token that belongs to NO labelled round market (the legacy /
        // unlabelled-host case): the venue lookup comes back empty.
        let (id, _) = c.place(live_order("ghost-token", "k1"), 0, 1).unwrap();

        let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
        host.set_active_plugin("polymarket");

        // The ACTIVE plugin's drain takes the untagged order (the
        // single-plugin fallback); the sibling does not.
        let poly_drain = host.take_pending_orders("polymarket").await;
        assert_eq!(
            poly_drain
                .iter()
                .map(|o| o.core_order_id.as_str())
                .collect::<Vec<_>>(),
            vec![id.as_str()],
            "untagged orders route to the active plugin — the single-plugin \
             behaviour preserved"
        );
        assert!(
            host.take_pending_orders("kalshi").await.is_empty(),
            "a sibling executor must not take the untagged order"
        );
    }

    #[tokio::test]
    async fn legacy_host_without_active_plugin_takes_all_orders() {
        let mut c = two_venue_live_core();
        let (poly_id, _) = c.place(live_order("poly-up", "k1"), 0, 1).unwrap();
        let (kal_id, _) = c.place(live_order("kal-up", "k2"), 0, 2).unwrap();

        // No `set_active_plugin` call — hosts that never told the core who is
        // active (most in-process tests). The legacy "" bucket takes ALL
        // orders: the exact pre-routing behaviour.
        let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
        let drain = host.take_pending_orders("").await;
        let mut got: Vec<&str> = drain.iter().map(|o| o.core_order_id.as_str()).collect();
        got.sort();
        let mut want: Vec<&str> = vec![poly_id.as_str(), kal_id.as_str()];
        want.sort();
        assert_eq!(
            got, want,
            "no active plugin = the historical one-drain-takes-all behaviour"
        );
    }

    #[tokio::test]
    async fn venue_cancels_route_to_the_owning_executor() {
        let mut c = two_venue_live_core();
        // Bind venue ids so a cancel queues a REAL venue cancel.
        let (poly_id, _) = c.place(live_order("poly-up", "k1"), 0, 1).unwrap();
        c.bind_venue(&poly_id, "poly-v1".into(), 2).unwrap();
        let (kal_id, _) = c.place(live_order("kal-up", "k2"), 0, 3).unwrap();
        c.bind_venue(&kal_id, "kal-v1".into(), 4).unwrap();
        c.cancel(&poly_id, 5).unwrap();
        c.cancel(&kal_id, 6).unwrap();

        let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
        host.set_active_plugin("polymarket");

        // Each executor cancels only ITS venue's ids: the queue is take-once,
        // so a sibling swallowing the other's id would strand the cancel.
        assert_eq!(
            host.take_pending_cancels("polymarket").await,
            vec!["poly-v1".to_string()]
        );
        assert_eq!(
            host.take_pending_cancels("kalshi").await,
            vec!["kal-v1".to_string()]
        );
        assert!(host.take_pending_cancels("polymarket").await.is_empty());
        assert!(host.take_pending_cancels("kalshi").await.is_empty());
    }

    /// #647: two plugins hold settlement claims at once (each on its OWN
    /// venue's collateral), and each executor's `for_plugin` handle drains
    /// ONLY its own claims — the redemption and merge queues can no longer
    /// hand a claim to a foreign venue's bridge (the cross-post the old
    /// global queue made: harmless there, but it burned retries and alerts).
    /// A handle with no bucket name (the legacy shape) keeps the all-take.
    #[tokio::test]
    async fn settlement_queues_drain_per_venue_bucket() {
        let mut c = two_venue_live_core();
        let now: i64 = 1_000_000;
        // One filled entry per venue: the fill's position stamps the entry
        // market's venue (the same `execution_venue_of` read the order drain
        // routes by), so each settled claim belongs to its own executor. Each
        // order rides its own market's condition — two claims, never one folded.
        let mut poly_order = live_order("poly-up", "s1");
        poly_order.condition_id = "cond-polymarket".into();
        let mut kal_order = live_order("kal-up", "s2");
        kal_order.condition_id = "cond-kalshi".into();
        let (poly_id, _) = c.place(poly_order, 0, now + 1).unwrap();
        let (kal_id, _) = c.place(kal_order, 0, now + 2).unwrap();
        let fill = |order_id: &str, token: &str, trade: &str| crate::model::Fill {
            order_id: order_id.into(),
            trade_id: Some(trade.into()),
            token_id: token.into(),
            side: crate::model::Side::Buy,
            price: dec!(0.40),
            size: dec!(2),
            status: crate::model::FillStatus::Confirmed,
            ts_ms: now + 3,
            tx_hash: None,
            maker: Some(true),
            account_id: crate::model::default_account_id(),
        };
        c.ingest_fill(fill(&poly_id, "poly-up", "t1"), now + 4)
            .unwrap();
        c.ingest_fill(fill(&kal_id, "kal-up", "t2"), now + 5)
            .unwrap();
        assert_eq!(c.positions().open_positions().len(), 2);
        assert_eq!(
            c.positions()
                .open_positions()
                .iter()
                .find(|p| p.token_id == "poly-up")
                .unwrap()
                .venue,
            "polymarket"
        );
        // Round end passes; the core watches both markets for a verdict.
        c.on_market_resolution(resolution_for("cond-polymarket", "poly-up"), now + 4_000);
        c.on_market_resolution(resolution_for("cond-kalshi", "kal-up"), now + 4_000);
        assert_eq!(c.take_pending_redemptions(now + 4_500).len(), 2);

        // Each executor's own handle takes exactly ITS venue's request.
        let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
        let poly_exec = host.for_plugin("polymarket");
        let kal_exec = host.for_plugin("kalshi");
        let poly_due = poly_exec.take_pending_redemptions().await;
        assert_eq!(
            poly_due.len(),
            1,
            "the polymarket executor takes only its claim"
        );
        assert_eq!(poly_due[0].venue, "polymarket");
        let kal_due = kal_exec.take_pending_redemptions().await;
        assert_eq!(kal_due.len(), 1, "the kalshi executor takes only its claim");
        assert_eq!(kal_due[0].venue, "kalshi");
        // Take-once per dispatch stamp: the re-drain is empty, and neither
        // handle ever re-sees the sibling's request.
        assert!(poly_exec.take_pending_redemptions().await.is_empty());
        assert!(kal_exec.take_pending_redemptions().await.is_empty());
        // The legacy shape (no bucket name — the pre-#647 hosts) still takes
        // everything in one drain.
        let legacy = host.take_pending_redemptions().await;
        assert!(legacy.is_empty(), "both were dispatched above");
    }

    /// The resolution a venue would report for one of the two test markets:
    /// the named token wins at 1.00/share.
    fn resolution_for(condition: &str, winner: &str) -> blitzkrieg_market_api::MarketResolution {
        blitzkrieg_market_api::MarketResolution {
            condition_id: condition.into(),
            resolved: true,
            payouts: vec![(winner.to_string(), dec!(1)), ("x-never".into(), dec!(0))],
            neg_risk: false,
            source: "test".into(),
        }
    }

    /// Pins the drain's market-lookup wiring: a token whose round market
    /// carries a venue reaches exactly that venue's drain, through the SAME
    /// `venue_of` read the panels' Venue column uses.
    #[test]
    fn the_token_venue_lookup_reads_the_round_list() {
        let c = two_venue_live_core();
        assert_eq!(c.venue_of_token("poly-up"), "polymarket");
        assert_eq!(c.venue_of_token("kal-down"), "kalshi");
        assert_eq!(c.venue_of_token("ghost-token"), "");
    }

    /// The handshake judge stays single-plugin; this pins its wire shape so a
    /// signature change here is a deliberate act, not a drive-by.
    #[test]
    fn compat_judge_keeps_its_single_plugin_signature() {
        let p = crate::market::compat::PluginModes::for_plugin(&BarePlugin);
        let modes = vec![blitzkrieg_strategy_api::StrategyMode {
            market_type: MarketType::Prediction,
            structure: None,
        }];
        assert!(crate::market::compat::check_compatibility("s", &modes, &p).is_ok());
    }

    // ── 盘口批次原子化 ─────────────────────────────────────────────────────

    /// 批次观测探针：记录每次 on_book 的 (token, 时间戳, best_bid)。
    struct BatchProbe {
        seen: Arc<std::sync::Mutex<Vec<(String, i64, i64)>>>,
    }
    impl crate::strategies::EngineStrategy for BatchProbe {
        fn name(&self) -> &str {
            "batch_probe_host"
        }
        fn on_book(&mut self, t: &str, s: &crate::model::OrderbookSnapshot, _n: i64) {
            self.seen.lock().unwrap().push((
                t.to_string(),
                s.timestamp,
                s.best_bid.mantissa() as i64,
            ));
        }
        fn on_round(&mut self, _slot: i64, _left: i64, _now: i64) {}
        fn find_candidates(
            &mut self,
            _ctx: &crate::strategies::StrategyCtx<'_>,
        ) -> Vec<crate::signal::TradeSignal> {
            Vec::new()
        }
    }

    /// 红→绿（撕裂窗口本体）：`on_books` 的整批应用必须对策略呈现为一次
    /// 原子跃迁 —— 同一调用里的两条 on_book 观测共享同一批次时间戳，up 和
    /// down 都是新批价格；绝不允许「up 已是 20_000 的新价、down 还停在
    /// 10_000 的旧价」的中间态。变异验证：把 `CoreHost::on_books` 退化为
    /// 逐本 `engine_on_data`（两次独立锁）后，本测试变红。
    #[tokio::test]
    async fn on_books_applies_the_whole_batch_under_one_lock() {
        let mut c = two_venue_live_core();
        // 追加两腿市场（up/down），书先给个旧批（与修复测试同构）。
        let now: i64 = 1_000_000;
        let slot = now / 1000 / 900;
        let end = (slot + 1) * 900 * 1000;
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![crate::model::CryptoMarket {
                    asset: "BTC".into(),
                    condition_id: "cond-batch".into(),
                    question_id: "q".into(),
                    up_token_id: "up".into(),
                    down_token_id: "down".into(),
                    up_price: rust_decimal_macros::dec!(0.6),
                    down_price: rust_decimal_macros::dec!(0.4),
                    expires_at_ms: end,
                    round_slot: slot,
                    round_duration_sec: 900,
                    archive_verdict: false,
                    venue: "polymarket".into(),
                    neg_risk: false,
                    question: "?".into(),
                }],
                now_ms: now,
            },
            now,
        );
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let engine = c.engine.as_mut().expect("engine present");
            engine
                .register_user_strategy(Box::new(BatchProbe { seen: seen.clone() }), "test".into())
                .expect("probe registers");
            engine.set_strategy_enabled("batch_probe_host", true);
        }

        let host = CoreHost::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)));
        let mk = |token: &str, bid: rust_decimal::Decimal| blitzkrieg_market_api::BookUpdate {
            token_id: token.to_string(),
            bids: vec![(bid, dec!(10))],
            asks: vec![(rust_decimal_macros::dec!(0.60), dec!(10))],
            ts_ms: 20_000,
        };
        host.on_books(
            vec![
                mk("up", rust_decimal_macros::dec!(0.50)),
                mk("down", rust_decimal_macros::dec!(0.55)),
            ],
            20_000,
        )
        .await;

        let obs = seen.lock().unwrap().clone();
        assert_eq!(obs.len(), 2, "both legs observed exactly once each");
        assert!(
            obs.iter().all(|(_, ts, _)| *ts == 20_000),
            "one batch = one timestamp for every token; torn: {obs:?}"
        );
        let up = obs.iter().find(|(t, _, _)| t == "up").expect("up");
        let down = obs.iter().find(|(t, _, _)| t == "down").expect("down");
        assert_eq!(up.2, 50, "up carries the new batch price; {obs:?}");
        assert_eq!(down.2, 55, "down carries the new batch price; {obs:?}");
    }

    struct BarePlugin;
    impl blitzkrieg_market_api::MarketPlugin for BarePlugin {
        fn name(&self) -> &str {
            "bare"
        }
        fn market_type(&self) -> MarketType {
            MarketType::Prediction
        }
    }

    // ── RFC-661 M2: the feed_source reuse fan-out (the host half) ───────────

    /// One upstream round batch from `source`, tagged for the wire exactly as
    /// a discovery hands it over. Tokens are the SAME byte strings every
    /// consumer re-registers — the reuse premise (shared token space). The
    /// expiry rides the REAL clock's current 900s window: the scanner's clock
    /// guard refuses a declaration more than two rounds from `now_ms()`, so a
    /// fixed fixture timestamp would silently open no round clock.
    fn reuse_batch(
        source: &str,
        up: &str,
        down: &str,
    ) -> Vec<blitzkrieg_market_api::MarketDescriptor> {
        let slot = blitzkrieg_market_api::net::now_ms() / 1000 / 900;
        vec![blitzkrieg_market_api::MarketDescriptor {
            asset: "BTC".to_string(),
            condition_id: format!("cond-{up}"),
            question_id: "q".to_string(),
            up_token_id: up.to_string(),
            down_token_id: down.to_string(),
            up_price: dec!(0.55),
            down_price: dec!(0.45),
            expires_at_ms: (slot + 1) * 900 * 1000,
            round_slot: slot,
            venue: source.to_string(),
            neg_risk: false,
            question: "reuse batch".to_string(),
        }]
    }

    /// The scanner rows a token holds, as (venue, up_token) pairs — the
    /// dual-key read the fan-out assertions walk.
    fn rows_for(c: &Core, token: &str) -> Vec<(String, String)> {
        c.engine
            .as_ref()
            .map(|e| {
                e.scanner()
                    .markets()
                    .iter()
                    .filter(|m| m.up_token_id == token || m.down_token_id == token)
                    .map(|m| (m.venue.clone(), m.up_token_id.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// M2 §4.1: ONE upstream round batch re-registers under every declared
    /// consumer's OWN venue identity — the source keeps its row, the consumer
    /// gains its own `(venue, token)` row (M1's dual-key table), and the
    /// engine reads both as native.
    #[tokio::test]
    async fn a_feed_source_consumer_re_registers_the_source_batch_under_its_own_venue() {
        let c = two_venue_live_core();
        let core = std::sync::Arc::new(tokio::sync::Mutex::new(c));
        let host = CoreHost::new(core.clone());
        host.set_active_plugin("polymarket");
        let reuse = FeedReuse::default();
        reuse.add_edge("polymarket", "polymarket"); // the source speaks too
        reuse.add_edge("simspace", "polymarket"); // the declared consumer
        host.set_feed_reuse(reuse);

        host.on_round_markets(reuse_batch("polymarket", "share-up", "share-down"))
            .await;

        let c = core.lock().await;
        let rows = rows_for(&c, "share-up");
        assert_eq!(
            rows.len(),
            2,
            "the byte-identical token holds TWO rows after the fan-out: {rows:?}"
        );
        let mut venues: Vec<&str> = rows.iter().map(|(v, _)| v.as_str()).collect();
        venues.sort_unstable();
        assert_eq!(
            venues,
            vec!["polymarket", "simspace"],
            "each venue's row carries ITS OWN identity: {rows:?}"
        );
        // The M1 read-back: each venue resolves the token to ITS OWN row, and
        // the token-only legacy lookup refuses as ambiguous (never first-hit).
        assert_eq!(c.venue_of_token("share-down"), "");
        let scanner = c.engine.as_ref().expect("engine").scanner();
        assert!(
            scanner
                .market_for_token_on_venue("simspace", "share-up")
                .is_some()
        );
        assert!(
            scanner
                .market_for_token_on_venue("polymarket", "share-up")
                .is_some()
        );
        assert!(
            scanner.market_for_token("share-up").is_none(),
            "two venues hold the token: the token-only lookup must refuse, not first-hit"
        );
        // The consumer's own round clock opened (its row spoke its own expiry).
        let clocks = c
            .engine
            .as_ref()
            .expect("engine")
            .scanner()
            .observed_venue_keys();
        assert!(
            clocks.iter().any(|v| v == "simspace"),
            "the consumer's re-registered row opens its own round clock: {clocks:?}"
        );
    }

    /// M2 §4.1 (fan-out shape): two INDEPENDENT consumer→source edges never
    /// re-register each other's batches — source A's batch fans out ONLY to
    /// A's consumers, source B's only to B's (a flat venue list would cross
    /// the two token spaces). All declared, none hardcoded: the edges are the
    /// plugins' declarations replayed through the host.
    #[tokio::test]
    async fn two_consumer_source_edges_never_re_register_each_others_batches() {
        let c = two_venue_live_core();
        let core = std::sync::Arc::new(tokio::sync::Mutex::new(c));
        let host = CoreHost::new(core.clone());
        host.set_active_plugin("polymarket");
        let reuse = FeedReuse::default();
        reuse.add_edge("polymarket", "polymarket");
        reuse.add_edge("cons_a", "src_a");
        reuse.add_edge("cons_b", "src_b");
        host.set_feed_reuse(reuse);

        // Source A's batch: only cons_a may re-register it.
        host.on_round_markets(reuse_batch("src_a", "a-up", "a-down"))
            .await;
        // Source B's batch: only cons_b may re-register it.
        host.on_round_markets(reuse_batch("src_b", "b-up", "b-down"))
            .await;

        let c = core.lock().await;
        let a_rows = rows_for(&c, "a-up");
        let a_venues: Vec<&str> = a_rows.iter().map(|(v, _)| v.as_str()).collect();
        assert!(
            a_venues.contains(&"cons_a") && !a_venues.contains(&"cons_b"),
            "A's batch re-registers under cons_a ONLY: {a_rows:?}"
        );
        let b_rows = rows_for(&c, "b-up");
        let b_venues: Vec<&str> = b_rows.iter().map(|(v, _)| v.as_str()).collect();
        assert!(
            b_venues.contains(&"cons_b") && !b_venues.contains(&"cons_a"),
            "B's batch re-registers under cons_b ONLY: {b_rows:?}"
        );
    }

    /// M2 (数据面归属): the consumer's row is a REAL row — a follow-up book
    /// on the shared token prices the consumer's OWN market read, and the
    /// upstream repricing lands on both rows (one connection, two venues'
    /// views). Books stay token-keyed (one upstream event applies once); the
    /// per-venue rows read the same book.
    #[tokio::test]
    async fn consumer_rows_read_the_reused_stream_and_stay_venue_distinct() {
        let c = two_venue_live_core();
        let core = std::sync::Arc::new(tokio::sync::Mutex::new(c));
        let host = CoreHost::new(core.clone());
        host.set_active_plugin("polymarket");
        let reuse = FeedReuse::default();
        reuse.add_edge("polymarket", "polymarket");
        reuse.add_edge("simspace", "polymarket");
        host.set_feed_reuse(reuse);

        host.on_round_markets(reuse_batch("polymarket", "flow-up", "flow-down"))
            .await;
        // The upstream book, once (ONE connection feeding both venues).
        host.on_book(blitzkrieg_market_api::BookUpdate {
            token_id: "flow-up".to_string(),
            bids: vec![(dec!(0.51), dec!(10))],
            asks: vec![(dec!(0.55), dec!(10))],
            ts_ms: 20_000,
        })
        .await;

        let c = core.lock().await;
        // Both rows survived the book (a merge would have dropped one).
        let rows = rows_for(&c, "flow-up");
        assert_eq!(
            rows.len(),
            2,
            "the book application must keep both rows: {rows:?}"
        );
        // The book snapshot is readable THROUGH each venue's own row: same
        // bytes, two owners — the OME cannot tell the consumer from a native.
        for venue in ["polymarket", "simspace"] {
            let m = c
                .engine
                .as_ref()
                .expect("engine")
                .scanner()
                .market_for_token_on_venue(venue, "flow-up")
                .expect("the venue's own row");
            assert_eq!(m.venue, venue, "each read resolves to the venue's own row");
        }
        assert_eq!(c.venue_of_token("flow-up"), "");
    }
}
