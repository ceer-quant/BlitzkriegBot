//! Engine — orchestrates market data → strategies → signals → orders → exits.
//!
//! Rust port of the evaluate loop in `src/strategies/crypto-hft/index.ts`.
//! The engine owns the shared market state and gates:
//!   - per-token local L2 books and per-asset Binance spot buffers
//!   - the round scanner (timing gates + token discovery)
//!   - the entry gates (one entry per token, timing, spot momentum, sizing)
//!   - near-miss telemetry for blocked entries
//!
//! Strategies are hosted behind [`crate::strategies::EngineStrategy`] (P-1.1):
//! each turns the host state into entry *candidates*, tagged with its own name;
//! the engine applies the shared gates and emits `OrderRequest`s whose
//! `strategy` field carries the emitting strategy. The builtin `spread_arb` is
//! registered first; user-layer strategies (frozen C ABI) are appended
//! disabled and must be explicitly enabled.
//!
//! It is driven by `DataEvent`s (book / spot / round) and, on each evaluation,
//! asks the `Core` to place orders. Everything here is deterministic given the
//! events and an injected clock, so it is unit-testable without network.

use crate::marketdata::{is_fresh, LocalBook};
use crate::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
use crate::scanner::{Scanner, ScannerConfig};
use crate::signal::{PriceBuffer, SpreadArbConfig, TradeSignal, TrendConfig};
use crate::strategies::{
    spread_arb::SpreadArbBuiltin, EngineStrategy, StrategyCtx, StrategyExitIntent,
};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub scanner: ScannerConfig,
    pub trend: TrendConfig,
    pub spread_arb: SpreadArbConfig,
    /// Max orderbook staleness before we refuse to price off it.
    pub max_orderbook_stale_ms: i64,
    /// Spot momentum window (sec) used by the alignment filter.
    pub momentum_window_sec: i64,
    /// Tolerance for spot moving against the entry direction (%).
    pub momentum_tol_pct: Decimal,
    /// Internal-key base so the OME dedups repeats within a round.
    pub size_usd: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            scanner: ScannerConfig::default(),
            trend: TrendConfig::default(),
            spread_arb: SpreadArbConfig::default(),
            max_orderbook_stale_ms: 8000,
            momentum_window_sec: 30,
            momentum_tol_pct: Decimal::new(3, 2), // 0.03%
            size_usd: Decimal::new(25, 1),        // 2.5
            min_shares: Decimal::from(10),
            max_shares: Decimal::from(10),
        }
    }
}

/// Why a valid signal did not become an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// The round timing gate (min age / min time left) rejected it.
    Timing,
    /// The spot momentum filter rejected it.
    Momentum,
}

/// A signal the evaluator produced that a gate blocked (near-miss telemetry).
#[derive(Debug, Clone)]
pub struct BlockedCandidate {
    pub token_id: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub price: Decimal,
    pub reason: BlockReason,
    pub time_left_sec: i64,
}

/// Market-data events the engine consumes (produced by the feed layer or, in
/// tests, driven directly).
#[derive(Debug, Clone)]
pub enum DataEvent {
    /// Full or incremental orderbook update for a token.
    Book { token_id: String, bids: Vec<(Decimal, Decimal)>, asks: Vec<(Decimal, Decimal)>, now_ms: i64 },
    /// Top-of-book only update (price_change / best_bid_ask).
    TopOfBook { token_id: String, best_bid: Option<Decimal>, best_ask: Option<Decimal>, now_ms: i64 },
    /// Binance spot price for an asset (e.g. "BTC").
    Spot { asset: String, price: Decimal, now_ms: i64 },
    /// Fresh round markets discovered by the scanner.
    RoundMarkets { markets: Vec<CryptoMarket>, now_ms: i64 },
}

/// A strategy registered with the engine, plus its enablement and provenance.
pub struct HostedStrategy {
    pub strategy: Box<dyn EngineStrategy>,
    pub enabled: bool,
    /// "builtin" or "dylib:<path>" / "test".
    pub source: String,
}

pub struct Engine {
    cfg: EngineConfig,
    scanner: Scanner,
    books: HashMap<String, LocalBook>,
    spot: HashMap<String, PriceBuffer>,
    /// Tokens with a live entry order this round, to avoid re-signalling.
    pending_tokens: HashSet<String>,
    /// Near-miss signals from the most recent evaluate().
    last_blocked: Vec<BlockedCandidate>,
    /// Records blocked signals + their subsequent price path (for offline
    /// evaluation of relaxing the entry gate). Observation-only.
    near_miss: crate::shadow::NearMissRecorder,
    blocked_timing_total: u64,
    blocked_momentum_total: u64,
    /// Optional hot-swap handle for Shadow Evolution. When present, every
    /// evaluate() reads the CURRENT mutable parameters (lock-free) so an
    /// evolution takes effect on the next tick with no restart.
    hot_params: Option<std::sync::Arc<arc_swap::ArcSwap<crate::shadow_evolution::MutableParams>>>,
    /// Registered strategies. The builtin `spread_arb` is first; user-layer
    /// strategies are appended (disabled until explicitly enabled).
    strategies: Vec<HostedStrategy>,
    /// Strategy close intents gathered during the latest evaluate(), drained by
    /// the host (`Core`) into its shared exit-submission path.
    strategy_exits: Vec<StrategyExitIntent>,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        let strategies = vec![HostedStrategy {
            strategy: Box::new(SpreadArbBuiltin::new(cfg.trend.clone(), cfg.spread_arb.clone())),
            enabled: true,
            source: "builtin".to_string(),
        }];
        Self {
            scanner: Scanner::new(cfg.scanner.clone()),
            books: HashMap::new(),
            spot: HashMap::new(),
            pending_tokens: HashSet::new(),
            last_blocked: Vec::new(),
            near_miss: crate::shadow::NearMissRecorder::new(500, 300_000),
            blocked_timing_total: 0,
            blocked_momentum_total: 0,
            hot_params: None,
            strategies,
            strategy_exits: Vec::new(),
            cfg,
        }
    }

    pub fn scanner(&self) -> &Scanner {
        &self.scanner
    }
    pub fn scanner_mut(&mut self) -> &mut Scanner {
        &mut self.scanner
    }

    pub fn set_config(&mut self, cfg: EngineConfig) {
        self.scanner.set_config(cfg.scanner.clone());
        for s in &mut self.strategies {
            s.strategy.on_config(&cfg.trend, &cfg.spread_arb);
        }
        self.cfg = cfg;
    }

    /// Tokens whose trend is currently confirmed (diagnostics/tests). The union
    /// over registered strategies.
    pub fn confirmed_tokens(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for s in &self.strategies {
            out.extend(s.strategy.confirmed_tokens());
        }
        out
    }

    /// Diagnostics: for each confirmed token, its current mid and whether it
    /// currently satisfies the entry band (per strategy).
    pub fn confirmed_diagnostics(&self, now_ms: i64) -> Vec<serde_json::Value> {
        let round = self.scanner.round_state(now_ms);
        let fresh = |token: &str| {
            fresh_book(&self.books, token, now_ms, self.cfg.max_orderbook_stale_ms)
        };
        let ctx = StrategyCtx::new(
            self.scanner.markets(),
            round.slot,
            round.time_left_sec,
            now_ms,
            &fresh,
        );
        self.strategies.iter().flat_map(|s| s.strategy.diagnostics(&ctx)).collect()
    }

    pub fn book_snapshot(&self, token_id: &str) -> Option<OrderbookSnapshot> {
        self.books.get(token_id).filter(|b| !b.is_empty()).map(|b| b.snapshot(token_id))
    }

    /// Feed one market-data event. Returns tokens whose confirmed trend broke so
    /// the caller can cancel their resting bids.
    pub fn on_data(&mut self, ev: DataEvent) -> Vec<(String, Decimal)> {
        match ev {
            DataEvent::Book { token_id, bids, asks, now_ms } => {
                let b = self.books.entry(token_id.clone()).or_default();
                b.apply_snapshot(&bids, &asks, now_ms);
                let snap = b.snapshot(&token_id);
                for s in &mut self.strategies {
                    s.strategy.on_book(&token_id, &snap, now_ms);
                }
                self.near_miss.on_book(&token_id, &snap, now_ms);
            }
            DataEvent::TopOfBook { token_id, best_bid, best_ask, now_ms } => {
                let b = self.books.entry(token_id.clone()).or_default();
                b.update_top(best_bid, best_ask, now_ms);
                let snap = b.snapshot(&token_id);
                for s in &mut self.strategies {
                    s.strategy.on_book(&token_id, &snap, now_ms);
                }
                self.near_miss.on_book(&token_id, &snap, now_ms);
            }
            DataEvent::Spot { asset, price, now_ms } => {
                self.spot.entry(asset).or_insert_with(|| PriceBuffer::new(180)).push(price, now_ms);
                return Vec::new();
            }
            DataEvent::RoundMarkets { markets, now_ms } => {
                let slot = markets.first().map(|m| m.round_slot).unwrap_or(0);
                for s in &mut self.strategies {
                    s.strategy.on_round(slot);
                }
                self.pending_tokens.clear();
                self.scanner.set_markets(markets);
                // Seed trend with current mids so confirmation starts immediately.
                let now = now_ms;
                let tokens: Vec<String> = self
                    .scanner
                    .markets()
                    .iter()
                    .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                    .collect();
                for t in tokens {
                    if let Some(snap) = self.book_snapshot(&t) {
                        for s in &mut self.strategies {
                            s.strategy.on_book(&t, &snap, now);
                        }
                    }
                }
            }
        }
        self.drain_breaks()
    }

    /// Collect trend breaks from every strategy (each break is cancelled once).
    fn drain_breaks(&mut self) -> Vec<(String, Decimal)> {
        let mut out = Vec::new();
        for s in &mut self.strategies {
            out.extend(s.strategy.take_breaks());
        }
        out
    }

    /// Spot momentum alignment filter: reject when spot is moving against the bet.
    fn momentum_ok(&self, asset: &str, dir: SignalDirection, now_ms: i64) -> bool {
        let Some(buf) = self.spot.get(asset) else { return true };
        let move_pct = buf.move_pct(self.cfg.momentum_window_sec, now_ms);
        let against = match dir {
            SignalDirection::Up => move_pct < -self.cfg.momentum_tol_pct,
            SignalDirection::Down => move_pct > self.cfg.momentum_tol_pct,
        };
        !against
    }

    /// Evaluate all enabled strategies against the current round and return the
    /// entry orders that should be placed. Pure w.r.t. the core: the caller
    /// places them and reports back tokens that got an order so repeats are
    /// suppressed this round.
    pub fn evaluate(&mut self, now_ms: i64) -> Vec<crate::model::OrderRequest> {
        self.last_blocked.clear();

        let round = self.scanner.round_state(now_ms);
        let tradeable = self.scanner.can_trade(now_ms).is_ok();

        // Candidates are computed regardless of the timing gate so we can record
        // near-misses (a valid dip that the timing gate blocked). This is the data
        // the shadow log lacks and the --replay entry analysis needs.
        let mut candidates: Vec<TradeSignal> = Vec::new();
        {
            let fresh = |token: &str| {
                fresh_book(&self.books, token, now_ms, self.cfg.max_orderbook_stale_ms)
            };
            let ctx = StrategyCtx::new(
                self.scanner.markets(),
                round.slot,
                round.time_left_sec,
                now_ms,
                &fresh,
            );
            for s in self.strategies.iter_mut() {
                if s.enabled {
                    candidates.extend(s.strategy.find_candidates(&ctx));
                    // Close intents accumulate regardless of the entry timing
                    // gate: an open position's exit is never round-timing-gated.
                    self.strategy_exits.extend(s.strategy.take_exit_intents());
                }
            }
        }

        let mut orders = Vec::new();
        // At most one entry per token per cycle: the first candidate (in
        // registration order) wins, matching the one-live-entry-per-token rule
        // the caller enforces across cycles via `pending_tokens`.
        let mut emitted: HashSet<String> = HashSet::new();
        for sig in candidates {
            if self.pending_tokens.contains(&sig.token_id) {
                continue;
            }
            if !emitted.insert(sig.token_id.clone()) {
                continue;
            }
            if !tradeable {
                let mid = self
                    .book_snapshot(&sig.token_id)
                    .map(|b| b.mid_price)
                    .unwrap_or(Decimal::ZERO);
                self.near_miss.on_blocked(
                    &sig.token_id,
                    &sig.asset,
                    sig.direction.as_str(),
                    sig.price,
                    mid,
                    crate::shadow::NearMissReason::Timing,
                    round.time_left_sec,
                    round.slot,
                    now_ms,
                );
                self.last_blocked.push(BlockedCandidate {
                    token_id: sig.token_id,
                    asset: sig.asset,
                    direction: sig.direction,
                    price: sig.price,
                    reason: BlockReason::Timing,
                    time_left_sec: round.time_left_sec,
                });
                continue;
            }
            if !self.momentum_ok(&sig.asset, sig.direction, now_ms) {
                let mid = self
                    .book_snapshot(&sig.token_id)
                    .map(|b| b.mid_price)
                    .unwrap_or(Decimal::ZERO);
                self.near_miss.on_blocked(
                    &sig.token_id,
                    &sig.asset,
                    sig.direction.as_str(),
                    sig.price,
                    mid,
                    crate::shadow::NearMissReason::Momentum,
                    round.time_left_sec,
                    round.slot,
                    now_ms,
                );
                self.last_blocked.push(BlockedCandidate {
                    token_id: sig.token_id,
                    asset: sig.asset,
                    direction: sig.direction,
                    price: sig.price,
                    reason: BlockReason::Momentum,
                    time_left_sec: round.time_left_sec,
                });
                continue;
            }

            let size = self.compute_shares(sig.price);
            orders.push(crate::model::OrderRequest {
                token_id: sig.token_id.clone(),
                condition_id: sig.condition_id,
                side: crate::model::Side::Buy,
                mode: crate::model::FillPolicy::MakerThenTaker,
                price: sig.price,
                size,
                internal_key: format!(
                    "{}:{}:{}:{}",
                    sig.strategy,
                    sig.asset,
                    sig.direction.as_str(),
                    round.slot
                ),
                strategy: sig.strategy,
                asset: sig.asset.clone(),
                direction: sig.direction.as_str().to_string(),
                round_slot: round.slot,
            });
        }
        orders
    }

    /// Drain finalized near-miss records for persistence.
    pub fn take_near_misses(&mut self, now_ms: i64) -> Vec<crate::shadow::NearMissRecord> {
        self.near_miss.tick(now_ms)
    }

    /// Force-finalize every pending near-miss (used on shutdown so records are
    /// not lost when the process stops before a window elapses).
    pub fn flush_near_misses(&mut self) -> Vec<crate::shadow::NearMissRecord> {
        self.near_miss.flush()
    }

    /// Near-miss signals blocked on the most recent evaluation.
    pub fn last_blocked(&self) -> &[BlockedCandidate] {
        &self.last_blocked
    }
    pub fn blocked_timing_count(&self) -> u64 {
        self.blocked_timing_total
    }
    pub fn blocked_momentum_count(&self) -> u64 {
        self.blocked_momentum_total
    }

    /// Accumulate blocked counters (called by the caller after evaluate).
    pub fn tally_blocked(&mut self) {
        for b in &self.last_blocked {
            match b.reason {
                BlockReason::Timing => self.blocked_timing_total += 1,
                BlockReason::Momentum => self.blocked_momentum_total += 1,
            }
        }
    }

    /// Attach the Shadow Evolution hot-swap handle. Called once when evolution is
    /// (re)configured; idempotent. Forwarded to every registered strategy.
    pub fn set_hot_params(
        &mut self,
        handle: std::sync::Arc<arc_swap::ArcSwap<crate::shadow_evolution::MutableParams>>,
    ) {
        self.hot_params = Some(handle.clone());
        for s in &mut self.strategies {
            s.strategy.set_hot_params(handle.clone());
        }
    }

    pub fn has_hot_params(&self) -> bool {
        self.hot_params.is_some()
    }

    /// The spread_arb parameters currently in force (base overlaid with any
    /// hot-swapped mutable params). Read-only view for tests/observability.
    pub fn current_spread_arb(&self) -> SpreadArbConfig {
        self.strategies
            .iter()
            .find_map(|s| s.strategy.spread_arb_view())
            .unwrap_or_default()
    }

    /// Strategy names the engine knows about, in registration order (the
    /// builtin `spread_arb` first, then user-layer strategies).
    pub fn supported_strategies(&self) -> Vec<String> {
        self.strategies.iter().map(|s| s.strategy.name().to_string()).collect()
    }
    /// Enabled strategy names.
    pub fn enabled_strategies(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .strategies
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.strategy.name().to_string())
            .collect();
        v.sort();
        v
    }
    /// Toggle a strategy; returns false if the name is unknown to the engine.
    pub fn set_strategy_enabled(&mut self, name: &str, enabled: bool) -> bool {
        for s in &mut self.strategies {
            if s.strategy.name() == name {
                s.enabled = enabled;
                return true;
            }
        }
        false
    }

    /// Register a strategy into the live dispatch.
    ///
    /// The strategy implements the SAME full [`EngineStrategy`] contract whether
    /// it is in-tree or an external v2 dylib — external is only a loading
    /// difference. It starts DISABLED: an explicit enable is required before it
    /// can place orders. Returns the registered name, or an error when the name
    /// is taken.
    pub fn register_user_strategy(
        &mut self,
        strategy: Box<dyn EngineStrategy>,
        source: String,
    ) -> Result<String, String> {
        let name = strategy.name().to_string();
        if self.strategies.iter().any(|s| s.strategy.name() == name) {
            return Err(format!("strategy name already registered: {name}"));
        }
        let mut hosted = HostedStrategy { strategy, enabled: false, source };
        // Registered after Shadow Evolution was configured? Forward the handle
        // so a later evolution still reaches this strategy.
        if let Some(h) = &self.hot_params {
            hosted.strategy.set_hot_params(h.clone());
        }
        self.strategies.push(hosted);
        Ok(name)
    }

    /// Where a registered strategy came from (`builtin`, `dylib:<path>`, ...).
    pub fn strategy_source(&self, name: &str) -> Option<&str> {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .map(|s| s.source.as_str())
    }

    /// Mark a token as having a live entry order this round (suppresses repeats).
    pub fn note_order_placed(&mut self, token_id: &str) {
        self.pending_tokens.insert(token_id.to_string());
    }

    /// Drain strategy close intents gathered during the latest evaluate(). The
    /// host routes these into the same exit-submission path as policy exits.
    pub fn drain_strategy_exits(&mut self) -> Vec<StrategyExitIntent> {
        std::mem::take(&mut self.strategy_exits)
    }

    fn compute_shares(&self, price: Decimal) -> Decimal {
        if price <= Decimal::ZERO {
            return self.cfg.min_shares;
        }
        let raw = (self.cfg.size_usd / price).round();
        raw.max(self.cfg.min_shares).min(self.cfg.max_shares)
    }
}

/// Book snapshot for pricing, but only while it is fresh enough. Mirrors the
/// engine's historical freshness rule (non-empty book + `max_orderbook_stale_ms`).
fn fresh_book(
    books: &HashMap<String, LocalBook>,
    token_id: &str,
    now_ms: i64,
    max_stale_ms: i64,
) -> Option<OrderbookSnapshot> {
    let snap = books.get(token_id).filter(|b| !b.is_empty()).map(|b| b.snapshot(token_id))?;
    let source = books.get(token_id)?;
    if !is_fresh(&source.snapshot(token_id), now_ms, max_stale_ms) {
        return None;
    }
    Some(snap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::RiskConfig;
    use crate::service::{Core, CoreConfig};
    use rust_decimal::prelude::FromPrimitive;
    use rust_decimal_macros::dec;

    fn cfg() -> EngineConfig {
        EngineConfig {
            scanner: ScannerConfig {
                assets: vec!["BTC".into()],
                round_duration_sec: 900,
                min_round_age_sec: 0,
                min_time_left_sec: 0,
            },
            trend: TrendConfig { confirm_sec: 5, ratio: dec!(0.5), min_price: dec!(0.5), broken_price: dec!(0.35), window_floor_ms: 0 },
            spread_arb: SpreadArbConfig { trend_max_entry_price: dec!(0.45), ..Default::default() },
            max_orderbook_stale_ms: 8000,
            momentum_window_sec: 30,
            momentum_tol_pct: dec!(0.03),
            size_usd: dec!(2.5),
            min_shares: dec!(10),
            max_shares: dec!(10),
        }
    }

    fn market(end_ms: i64) -> CryptoMarket {
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "cond".into(),
            question_id: "q".into(),
            up_token_id: "up".into(),
            down_token_id: "down".into(),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / 900,
            neg_risk: true,
            question: "BTC up or down".into(),
        }
    }

    fn core() -> Core {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig { max_order_notional: dec!(5), ..Default::default() },
            dry_seed_balance: dec!(1000),
            ..Default::default()
        });
        // Keep the exit engine out of the way for signal tests.
        let mut cc = c.config_mut();
        cc.auto_exits_enabled = false;
        c
    }

    #[test]
    fn engine_places_a_trend_confirmed_dip_buy() {
        let mut e = Engine::new(cfg());
        let mut c = core();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });

        // Confirm the UP trend: several books with mid >= 0.5 within the window.
        for i in 0..12 {
            let t = now + i * 1000;
            e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.55), dec!(100))], asks: vec![(dec!(0.57), dec!(100))], now_ms: t });
        }
        // Dip: mid 0.44 (bid 0.43) → entry 0.43 ≤ 0.45 → signal.
        e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.43), dec!(100))], asks: vec![(dec!(0.45), dec!(100))], now_ms: now + 12_000 });
        // Spot calm.
        e.on_data(DataEvent::Spot { asset: "BTC".into(), price: dec!(60000), now_ms: now + 12_000 });

        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "expected one spread_arb entry");
        // Placing through the core succeeds (funds seeded) and fills on the book.
        let req = orders.into_iter().next().unwrap();
        let (id, _) = c.place(req, 5000, now + 12_000).unwrap();
        assert!(!id.is_empty());
    }

    #[test]
    fn engine_skips_without_confirmed_trend() {
        let mut e = Engine::new(cfg());
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });
        // Only one dip book; no confirm window yet.
        e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.43), dec!(100))], asks: vec![(dec!(0.45), dec!(100))], now_ms: now });
        assert!(e.evaluate(now).is_empty());
    }

    #[test]
    fn engine_blocks_entry_when_spot_moves_against() {
        let mut e = Engine::new(cfg());
        let mut c = core();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });
        for i in 0..12 {
            e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.55), dec!(100))], asks: vec![(dec!(0.57), dec!(100))], now_ms: now + i * 1000 });
        }
        e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.43), dec!(100))], asks: vec![(dec!(0.45), dec!(100))], now_ms: now + 12_000 });
        // Spot falls hard over the momentum window → UP entry blocked.
        e.on_data(DataEvent::Spot { asset: "BTC".into(), price: dec!(60000), now_ms: now + 1000 });
        e.on_data(DataEvent::Spot { asset: "BTC".into(), price: dec!(59000), now_ms: now + 12_000 });
        assert!(e.evaluate(now + 12_000).is_empty(), "spot moving against should block the entry");
    }

    #[test]
    fn trend_break_is_reported_for_bid_cancellation() {
        let mut e = Engine::new(cfg());
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });
        for i in 0..12 {
            e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.55), dec!(100))], asks: vec![(dec!(0.57), dec!(100))], now_ms: now + i * 1000 });
        }
        let broken = e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.30), dec!(100))], asks: vec![(dec!(0.32), dec!(100))], now_ms: now + 12_000 });
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].0, "up");
    }

    #[test]
    fn records_near_miss_when_timing_gate_blocks_a_valid_dip() {
        // A valid trend-confirmed dip that the timing gate rejects must be
        // counted (this is the near-miss telemetry the shadow log lacks).
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000; // force the timing gate to fail
        let mut e = Engine::new(cfg);
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });
        for i in 0..12 {
            e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.55), dec!(100))], asks: vec![(dec!(0.57), dec!(100))], now_ms: now + i * 1000 });
        }
        e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.43), dec!(100))], asks: vec![(dec!(0.45), dec!(100))], now_ms: now + 12_000 });

        let orders = e.evaluate(now + 12_000);
        assert!(orders.is_empty(), "timing gate must block the order");
        assert_eq!(e.last_blocked().len(), 1, "the blocked dip should be recorded");
        assert_eq!(e.last_blocked()[0].reason, BlockReason::Timing);
        e.tally_blocked();
        assert_eq!(e.blocked_timing_count(), 1);
        assert_eq!(e.blocked_momentum_count(), 0);
    }

    #[test]
    fn compute_shares_clamps_to_bounds() {
        let e = Engine::new(cfg());
        // 2.5 / 0.25 = 10 shares within [10,10].
        assert_eq!(e.compute_shares(dec!(0.25)), dec!(10));
        // Very low price would exceed max → clamped to 10.
        assert_eq!(e.compute_shares(dec!(0.01)), dec!(10));
    }

    #[test]
    fn compute_shares_honours_custom_bounds() {
        // Small-balance live sizing: a 4-share fixed lot (2 positions on ~4.8u).
        let mut c = cfg();
        c.size_usd = dec!(2.5);
        c.min_shares = dec!(4);
        c.max_shares = dec!(4);
        let e = Engine::new(c);
        // 2.5 / 0.45 ≈ 5.56 → rounds to 6, then clamped down to the 4-share lot.
        assert_eq!(e.compute_shares(dec!(0.45)), dec!(4));
        // A wide band lets the nominal size win: 2.5 / 0.25 = 10 within [2,20].
        let mut c2 = cfg();
        c2.min_shares = dec!(2);
        c2.max_shares = dec!(20);
        let e2 = Engine::new(c2);
        assert_eq!(e2.compute_shares(dec!(0.25)), dec!(10));
        assert_eq!(e2.compute_shares(dec!(1.25)), dec!(2));
    }

    // ── P-1.1 strategy dispatch ─────────────────────────────────────────────

    /// A trivial hosted strategy: buy either outcome token when its mid dips to
    /// or below a level. Implements the SAME full EngineStrategy contract an
    /// external v2 dylib does (external is only a loading difference).
    struct DipBuyer {
        name: String,
        buy_below: Decimal,
    }
    impl EngineStrategy for DipBuyer {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _token_id: &str, _snap: &crate::model::OrderbookSnapshot, _now_ms: i64) {}
        fn on_round(&mut self, _slot: i64) {}
        fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            let mut out = Vec::new();
            for market in ctx.markets() {
                for (token_id, direction) in [
                    (&market.up_token_id, SignalDirection::Up),
                    (&market.down_token_id, SignalDirection::Down),
                ] {
                    let Some(book) = ctx.fresh_book(token_id) else { continue };
                    if book.mid_price <= self.buy_below {
                        out.push(TradeSignal {
                            strategy: self.name.clone(),
                            asset: market.asset.clone(),
                            direction,
                            token_id: token_id.clone(),
                            condition_id: market.condition_id.clone(),
                            price: book.mid_price,
                            reason: format!("{} dip", self.name),
                        });
                    }
                }
            }
            out
        }
    }

    #[test]
    fn registry_starts_with_the_builtin_only_and_rejects_unknown_toggles() {
        let mut e = Engine::new(cfg());
        assert_eq!(e.supported_strategies(), vec!["spread_arb".to_string()]);
        assert_eq!(e.enabled_strategies(), vec!["spread_arb".to_string()]);
        assert!(!e.set_strategy_enabled("nope", true), "unknown name must not toggle");
    }

    #[test]
    fn user_strategy_registers_disabled_and_is_tagged_once_enabled() {
        let mut e = Engine::new(cfg());
        let name = e
            .register_user_strategy(
                Box::new(DipBuyer { name: "dip_buyer".into(), buy_below: dec!(0.6) }),
                "test".into(),
            )
            .unwrap();
        assert_eq!(name, "dip_buyer");
        assert_eq!(e.supported_strategies(), vec!["spread_arb".to_string(), "dip_buyer".to_string()]);
        assert_eq!(e.enabled_strategies(), vec!["spread_arb".to_string()], "starts disabled");
        assert_eq!(e.strategy_source("dip_buyer"), Some("test"));
        // Duplicate names are rejected outright.
        assert!(e
            .register_user_strategy(
                Box::new(DipBuyer { name: "dip_buyer".into(), buy_below: dec!(0.6) }),
                "test".into()
            )
            .is_err());

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.55), dec!(100))],
            asks: vec![(dec!(0.57), dec!(100))],
            now_ms: now + 1_000,
        });
        // Disabled → no order even though the mid satisfies the strategy.
        assert!(e.evaluate(now + 1_000).is_empty());

        assert!(e.set_strategy_enabled("dip_buyer", true));
        let orders = e.evaluate(now + 2_000);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(orders[0].strategy, "dip_buyer");
        assert_eq!(orders[0].asset, "BTC");
        assert!(orders[0].internal_key.starts_with("dip_buyer:BTC:"), "{}", orders[0].internal_key);
    }

    #[test]
    fn two_strategies_run_side_by_side_with_separate_tags() {
        let mut e = Engine::new(cfg());
        e.register_user_strategy(
            Box::new(DipBuyer { name: "dip_buyer".into(), buy_below: dec!(0.45) }),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("dip_buyer", true));

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });
        // Confirm the UP trend over 12 samples (spread_arb needs it).
        for i in 0..12 {
            e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.55), dec!(100))], asks: vec![(dec!(0.57), dec!(100))], now_ms: now + i * 1000 });
        }
        // UP dips → spread_arb candidate (and dip_buyer's too, on the same token).
        e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.43), dec!(100))], asks: vec![(dec!(0.45), dec!(100))], now_ms: now + 12_000 });
        // DOWN is cheap → only dip_buyer signals there (no trend confirmation).
        e.on_data(DataEvent::Book { token_id: "down".into(), bids: vec![(dec!(0.41), dec!(100))], asks: vec![(dec!(0.43), dec!(100))], now_ms: now + 12_000 });

        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 2, "one entry per token per cycle: {orders:?}");
        assert!(orders.iter().any(|o| o.token_id == "up" && o.strategy == "spread_arb"), "{orders:?}");
        assert!(orders.iter().any(|o| o.token_id == "down" && o.strategy == "dip_buyer"), "{orders:?}");
    }

    #[test]
    fn disabling_spread_arb_stops_its_entries() {
        let mut e = Engine::new(cfg());
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets { markets: vec![market(1_800_000)], now_ms: now });
        for i in 0..12 {
            e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.55), dec!(100))], asks: vec![(dec!(0.57), dec!(100))], now_ms: now + i * 1000 });
        }
        e.on_data(DataEvent::Book { token_id: "up".into(), bids: vec![(dec!(0.43), dec!(100))], asks: vec![(dec!(0.45), dec!(100))], now_ms: now + 12_000 });
        assert_eq!(e.evaluate(now + 12_000).len(), 1);
        assert!(e.set_strategy_enabled("spread_arb", false));
        // Same round, no order noted as pending: the empty result must come from
        // the disable, not from suppression.
        assert!(e.evaluate(now + 12_001).is_empty(), "disabled strategy must not emit");
        assert!(e.set_strategy_enabled("spread_arb", true));
        assert_eq!(e.evaluate(now + 12_002).len(), 1, "re-enabling restores entries");
    }
}
