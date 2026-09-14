//! Adapter from the user-layer `strategy_engine::Strategy` contract (one tick
//! in, one intent out) onto the engine's round-based dispatch.
//!
//! A user strategy loaded through the frozen C ABI is driven once per round
//! market token on every evaluation cycle. Its `Buy` intent becomes a normal
//! candidate and therefore passes the same kernel gates (signal validation,
//! then the host's timing/momentum/sizing gates) as the builtin. A `Sell`
//! intent is not an entry candidate: position exits stay owned by the kernel's
//! exit policy, so it is ignored here (documented limitation, P-1.1).

use super::{EngineStrategy, StrategyCtx};
use crate::model::{OrderbookSnapshot, SignalDirection};
use crate::signal::TradeSignal;
use crate::strategy_engine::{validate_signal, MarketTick, Signal};

pub struct UserStrategyAdapter {
    inner: Box<dyn crate::strategy_engine::Strategy>,
    source: String,
}

impl UserStrategyAdapter {
    pub fn new(inner: Box<dyn crate::strategy_engine::Strategy>, source: String) -> Self {
        Self { inner, source }
    }

    /// Where this strategy came from (e.g. `dylib:<path>`), for diagnostics.
    pub fn source(&self) -> &str {
        &self.source
    }
}

impl EngineStrategy for UserStrategyAdapter {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn on_book(&mut self, _token_id: &str, _snap: &OrderbookSnapshot, _now_ms: i64) {
        // The user-layer contract only sees full ticks, delivered from
        // `find_candidates` when the host asks for this cycle's candidates.
    }

    fn on_round(&mut self, slot: i64) {
        self.inner.on_round(slot);
    }

    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
        let mut out = Vec::new();
        for market in ctx.markets() {
            for (token_id, direction) in [
                (&market.up_token_id, SignalDirection::Up),
                (&market.down_token_id, SignalDirection::Down),
            ] {
                let Some(book) = ctx.fresh_book(token_id) else { continue };
                let tick = MarketTick::from_book(token_id, &market.asset, &book);
                let Some(sig) = self.inner.on_tick(&tick) else { continue };
                // The kernel's signal gate runs BEFORE any order work: an
                // out-of-range price or mismatched symbol is dropped here and
                // never becomes an order.
                if validate_signal(&sig, &tick).is_err() {
                    continue;
                }
                if let Signal::Buy { price, .. } = sig {
                    out.push(TradeSignal {
                        strategy: self.inner.name().to_string(),
                        asset: market.asset.clone(),
                        direction,
                        token_id: token_id.clone(),
                        condition_id: market.condition_id.clone(),
                        price,
                        reason: format!("user strategy {} buy signal", self.inner.name()),
                    });
                }
            }
        }
        out
    }
}
