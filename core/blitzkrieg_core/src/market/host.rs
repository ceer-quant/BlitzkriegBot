//! `CoreHost` — the core's implementation of `MarketHost`.
//!
//! A market plugin can only reach the core through this object. Every method is
//! a thin, logic-free forward onto the corresponding `Core` method, so the
//! plugin boundary carries no trading behaviour of its own.

use crate::ipc::server::now_ms;
use crate::model as m;
use crate::service::Core;
use blitzkrieg_market_api as api;
use blitzkrieg_market_api::{BoxFuture, MarketHost};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

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
        neg_risk: d.neg_risk,
        question: d.question.clone(),
    }
}

pub fn market_to_api(m: &m::CryptoMarket) -> api::MarketDescriptor {
    api::MarketDescriptor {
        asset: m.asset.clone(),
        condition_id: m.condition_id.clone(),
        question_id: m.question_id.clone(),
        up_token_id: m.up_token_id.clone(),
        down_token_id: m.down_token_id.clone(),
        up_price: m.up_price,
        down_price: m.down_price,
        expires_at_ms: m.expires_at_ms,
        round_slot: m.round_slot,
        neg_risk: m.neg_risk,
        question: m.question.clone(),
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
        A::WouldCross => C::WouldCross,
        A::InvalidTickSize => C::InvalidTickSize,
        A::InvalidSize => C::InvalidSize,
        A::InsufficientFunds => C::InsufficientFunds,
        A::RiskRejected => C::RiskRejected,
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

/// Implements [`MarketHost`] over the shared `Core`. Cloneable so each plugin
/// component can hold its own handle.
#[derive(Clone)]
pub struct CoreHost {
    core: Arc<AsyncMutex<Core>>,
}

impl CoreHost {
    pub fn new(core: Arc<AsyncMutex<Core>>) -> Self {
        Self { core }
    }
}

impl MarketHost for CoreHost {
    fn on_book(&self, update: api::BookUpdate) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let now = now_ms();
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
            let now = now_ms();
            let models: Vec<m::CryptoMarket> = markets.iter().map(market_from_api).collect();
            let tokens: Vec<String> = models
                .iter()
                .flat_map(|mk| [mk.up_token_id.clone(), mk.down_token_id.clone()])
                .collect();
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
            self.subscribe_tokens(tokens).await;
        })
    }

    fn subscribe_tokens(&self, tokens: Vec<String>) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.core.lock().await.subscribe_feed_tokens(tokens).await;
        })
    }

    fn install_subscription_control(
        &self,
        control: Arc<dyn api::SubscriptionControl>,
    ) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.core.lock().await.set_subscription(control);
        })
    }

    fn take_pending_orders(&self) -> BoxFuture<'_, Vec<api::PendingOrder>> {
        Box::pin(async move {
            let c = self.core.lock().await;
            c.pending_unbound().iter().map(pending_to_api).collect()
        })
    }

    fn take_pending_cancels(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move {
            let mut c = self.core.lock().await;
            c.take_pending_venue_cancels()
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

    fn on_reconcile_failed(&self, error: api::CoreError) -> BoxFuture<'_, ()> {
        let e = error_from_api(&error);
        Box::pin(async move {
            let mut c = self.core.lock().await;
            c.on_reconcile_failed(e, now_ms());
        })
    }

    fn on_self_check(&self, report: api::SelfCheckReport) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.core.lock().await.on_self_check(report);
        })
    }

    fn seed_balance(&self, balance: rust_decimal::Decimal) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.core.lock().await.set_balance(balance);
        })
    }

    fn venue_free_balance(&self, free: rust_decimal::Decimal) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut c = self.core.lock().await;
            let ledger = c.ledger();
            let (balance, reserved) = (ledger.balance(), ledger.reserved());
            if reserved == rust_decimal::Decimal::ZERO {
                // No resting commitments, so the venue's free cash IS our total
                // cash in either reporting convention. A fill the bot missed
                // shows up here as cash that left with no fill event — this is
                // the drift killer for exactly that failure.
                if balance != free {
                    c.set_balance(free);
                    eprintln!("core: venue cash reconcile: ledger {balance} -> venue free {free}");
                }
            } else {
                let expected_free = balance - reserved;
                if (expected_free - free).abs() > rust_decimal::Decimal::new(2, 2) {
                    // With resting orders the venue may or may not net their
                    // commitments out of the reported balance; auto-correcting
                    // here could double-count. Surface it instead.
                    eprintln!(
                        "core: venue cash drift: ledger={balance} reserved={reserved} venue free={free} (resting orders; not auto-corrected)"
                    );
                }
            }
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
        Box::pin(async move {
            let now = now_ms();
            let mut c = self.core.lock().await;
            c.take_pending_redemptions(now)
        })
    }

    fn on_redemption_result(&self, result: api::RedemptionResult) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let now = now_ms();
            let mut c = self.core.lock().await;
            c.on_redemption_result(result, now);
        })
    }
}
