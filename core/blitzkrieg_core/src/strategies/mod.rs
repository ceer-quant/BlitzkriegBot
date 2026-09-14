//! Strategy host seam — how the self-driving engine runs strategies.
//!
//! The engine owns the market state (books, spot, round timing) and the shared
//! entry gates (one-entry-per-token, round timing, spot momentum, sizing). A
//! hosted strategy only turns that state into *candidates*: it may never place,
//! size or risk-check anything itself — the host does, so every strategy is
//! subject to the same kernel-side gates.
//!
//! There is exactly ONE full-featured contract: [`EngineStrategy`]. The proven
//! in-tree [`spread_arb::SpreadArbBuiltin`] and an external dylib loaded through
//! C ABI v2 ([`foreign::ForeignStrategy`]) both implement it. Being external is
//! only a loading difference — an external strategy sees every book callback,
//! the full depth ladder, round/market context, and can express entries, exits,
//! breaks, confirmation, diagnostics, config and hot parameters. The old
//! best-only reduced trait/adapter (which dropped `Sell` and no-op'd `on_book`)
//! was removed in E7 (#38) so the capability gap cannot reopen.

#[cfg(feature = "strategy-loading")]
pub mod foreign;
pub mod spread_arb;

use crate::model::{CryptoMarket, OrderbookSnapshot};
use crate::signal::{SpreadArbConfig, TradeSignal, TrendConfig};
use rust_decimal::Decimal;
use std::collections::HashSet;

/// A strategy's wish to CLOSE an open position. Like an entry candidate it is
/// only an intent: the host resolves it against a live position, prices it off
/// the current book, runs the same risk/dedup/sizing path and submits the sell.
/// `reason` is a short strategy-owned tag for tracing (e.g. "tp"); the kernel
/// records the close under [`crate::model::ExitReason::StrategySignal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyExitIntent {
    pub token_id: String,
    pub reason: String,
}

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

    /// Close intents accumulated since the last drain. The host resolves each
    /// token to a live position and routes it through the SAME exit submission
    /// path as an automated/policy exit (live-sell dedup, `sell_shares`, risk +
    /// ledger + sign in `place`); an intent on a token with no open position is
    /// dropped. Drains: each intent is returned at most once.
    fn take_exit_intents(&mut self) -> Vec<StrategyExitIntent> {
        Vec::new()
    }

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
