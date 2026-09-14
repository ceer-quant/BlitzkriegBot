//! Strategy host seam — how the self-driving engine runs strategies (P-1.1).
//!
//! The engine owns the market state (books, spot, round timing) and the shared
//! entry gates (one-entry-per-token, round timing, spot momentum, sizing). A
//! hosted strategy only turns that state into *candidates*: it may never place,
//! size or risk-check anything itself — the host does, so every strategy is
//! subject to the same kernel-side gates.
//!
//! Two implementations:
//!  - [`spread_arb::SpreadArbBuiltin`] — the proven builtin. P-1.1 moved it
//!    behind this seam with its behaviour kept bit-for-bit (the engine parity
//!    harnesses are the gate).
//!  - [`user_adapter::UserStrategyAdapter`] — bridges user-layer strategies
//!    loaded through the frozen `strategy_engine::Strategy` C ABI onto the
//!    round-based dispatch.

pub mod spread_arb;
pub mod user_adapter;

use crate::model::{CryptoMarket, OrderbookSnapshot};
use crate::signal::{SpreadArbConfig, TradeSignal, TrendConfig};
use rust_decimal::Decimal;
use std::collections::HashSet;

/// Read-only host market view handed to a strategy for one evaluation cycle.
pub struct StrategyCtx<'a> {
    markets: &'a [CryptoMarket],
    round_slot: i64,
    time_left_sec: i64,
    now_ms: i64,
    fresh_book: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
}

impl<'a> StrategyCtx<'a> {
    pub fn new(
        markets: &'a [CryptoMarket],
        round_slot: i64,
        time_left_sec: i64,
        now_ms: i64,
        fresh_book: &'a dyn Fn(&str) -> Option<OrderbookSnapshot>,
    ) -> Self {
        Self { markets, round_slot, time_left_sec, now_ms, fresh_book }
    }

    /// Markets of the current round.
    pub fn markets(&self) -> &[CryptoMarket] {
        self.markets
    }
    pub fn round_slot(&self) -> i64 {
        self.round_slot
    }
    pub fn time_left_sec(&self) -> i64 {
        self.time_left_sec
    }
    pub fn now_ms(&self) -> i64 {
        self.now_ms
    }
    /// Token book, but only while it is fresh enough to price off.
    pub fn fresh_book(&self, token_id: &str) -> Option<OrderbookSnapshot> {
        (self.fresh_book)(token_id)
    }
}

/// A strategy hosted by the engine.
///
/// Lifecycle: `on_config` (on engine config changes) → `on_round` + `on_book`
/// as data arrives → `find_candidates` once per evaluation. The host then runs
/// the shared gates and places the surviving orders.
pub trait EngineStrategy: Send + Sync {
    fn name(&self) -> &str;

    /// Observe a book / top-of-book update (own trend/price state).
    fn on_book(&mut self, token_id: &str, snap: &OrderbookSnapshot, now_ms: i64);

    /// A new round started (per-round state reset).
    fn on_round(&mut self, slot: i64);

    /// Tokens whose setup broke since the last call; the host cancels their
    /// resting entry bids. Drains: each break is returned exactly once.
    fn take_breaks(&mut self) -> Vec<(String, Decimal)> {
        Vec::new()
    }

    /// Tokens this strategy currently considers entry-eligible. Feeds the shadow
    /// observation and diagnostics; the host unions it across strategies.
    fn confirmed_tokens(&self) -> HashSet<String> {
        HashSet::new()
    }

    /// Entry candidates for this cycle. The host applies the shared gates
    /// (round timing, spot momentum, one-entry-per-token) and sizing afterwards,
    /// so strategies cannot bypass them.
    fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal>;

    /// Optional diagnostics payload (surfaced per strategy by `engine.stats`).
    fn diagnostics(&self, _ctx: &StrategyCtx<'_>) -> Vec<serde_json::Value> {
        Vec::new()
    }

    /// Shadow-Evolution hot parameters (spread_arb-shaped; default ignores).
    fn set_hot_params(
        &mut self,
        _handle: std::sync::Arc<arc_swap::ArcSwap<crate::shadow_evolution::MutableParams>>,
    ) {
    }

    /// The spread_arb parameters currently in force (observability for the
    /// builtin; other strategies return None).
    fn spread_arb_view(&self) -> Option<SpreadArbConfig> {
        None
    }

    /// The host config changed (trend / spread_arb knobs).
    fn on_config(&mut self, _trend: &TrendConfig, _spread_arb: &SpreadArbConfig) {}
}
