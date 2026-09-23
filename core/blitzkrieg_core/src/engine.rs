//! Engine — orchestrates market data → strategies → signals → orders → exits.
//!
//! Rust port of the evaluate loop in `src/strategies/crypto-hft/index.ts`
//! (the TS tree is gone with the Node source layer, `62b16c88`).
//! The engine owns the shared market state and gates:
//!   - per-token local L2 books and per-asset Binance spot buffers
//!   - the round scanner (timing gates + token discovery)
//!   - the entry gates (one entry per token, timing, spot momentum, sizing)
//!   - near-miss telemetry for blocked entries
//!
//! Strategies are hosted behind [`crate::strategies::EngineStrategy`] (P-1.1):
//! each turns the host state into entry *candidates*, tagged with its own name;
//! the engine applies the shared gates and emits `OrderRequest`s whose
//! `strategy` field carries the emitting strategy. The kernel ships ZERO
//! strategies (PR-B): the registry starts EMPTY and every strategy — the shipped
//! examples included — enters through the C ABI v2 dlopen path
//! (`Core::load_strategy_lib`), so nothing about strategy identity is baked in.
//!
//! It is driven by `DataEvent`s (book / spot / round) and, on each evaluation,
//! asks the `Core` to place orders. Everything here is deterministic given the
//! events and an injected clock, so it is unit-testable without network.

use crate::marketdata::LocalBook;
use crate::model::{CryptoMarket, OrderbookSnapshot, SignalDirection};
use crate::scanner::{Scanner, ScannerConfig};
use crate::signal::{PriceBuffer, SpreadArbConfig, TradeSignal, TrendConfig};
use crate::strategies::{EngineStrategy, GateExemptions, StrategyCtx, StrategyExitIntent};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};

/// Compiled default for [`EngineConfig::max_orderbook_stale_ms`] (#205): the
/// value every deployment has run with since the freshness rule existed. It is
/// the DEFAULT of the runtime knob, not the knob itself — an unconfigured kernel
/// keeps this behaviour exactly.
pub const DEFAULT_MAX_ORDERBOOK_STALE_MS: i64 = 8_000;

/// Upper bound accepted for `--max-orderbook-stale-ms` (#205).
///
/// Ten minutes is already far past any usable freshness budget (the venue's
/// rounds are minutes long), so a value above it is a typo — `800000` for
/// `80000` — rather than a setting. It is refused loudly at startup instead of
/// being silently clamped: an operator who means "never treat a book as stale"
/// has `0` for that, spelled out in the usage text.
pub const MAX_ORDERBOOK_STALE_MS_CEILING: i64 = 600_000;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub scanner: ScannerConfig,
    pub trend: TrendConfig,
    pub spread_arb: SpreadArbConfig,
    /// Max orderbook staleness before we refuse to price off it (#205).
    ///
    /// The knob that decides "how long after the feed goes quiet do we stop
    /// trading", so it is a runtime setting, not a compiled one: it comes from
    /// `--max-orderbook-stale-ms` / `BK_MAX_ORDERBOOK_STALE_MS` through
    /// [`crate::service::CoreConfig::max_orderbook_stale_ms`]. `0` = the check is
    /// OFF (any age is accepted); the compiled default is
    /// [`DEFAULT_MAX_ORDERBOOK_STALE_MS`], so an unconfigured kernel behaves
    /// exactly as it did before the knob existed.
    pub max_orderbook_stale_ms: i64,
    /// Spot momentum window (sec) used by the alignment filter.
    pub momentum_window_sec: i64,
    /// Tolerance for spot moving against the entry direction (%).
    pub momentum_tol_pct: Decimal,
    /// Internal-key base so the OME dedups repeats within a round.
    pub size_usd: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
    /// P0 #202 — the per-entry budget as a PERCENTAGE of the account's cash
    /// equity, instead of the absolute `size_usd`. 0 = OFF (the shipped
    /// default): the absolute path below runs unchanged, byte for byte.
    ///
    /// An absolute budget is not a risk statement: `size_usd = 2.5` with the
    /// default share band [10, 10] sends 10 shares whatever the price, which on
    /// the live 4.8 USDC account is 4.00 USD — 83% of it — and the band, not
    /// the budget, is what decided that. A percentage is the same statement on
    /// every account size: 20% is 0.96 USD on a 4.8 book and 96 on a 480 one.
    ///
    /// When > 0 this REPLACES `size_usd` (an absolute budget left in place
    /// would cap a grown account at the old size, which is the failure this
    /// knob exists to remove). See `compute_shares` for the exact rules.
    pub size_pct: Decimal,
    /// Per-strategy sizing overrides (E2-a). Absent strategies use the globals;
    /// a present override is always clamped so it can never exceed the global
    /// risk values — global is both the fallback and the ceiling.
    pub strategy_sizes: HashMap<String, StrategySize>,
}

/// Optional per-strategy sizing knobs (E2-a). Every field is optional; `None`
/// falls back to the matching global `EngineConfig` value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrategySize {
    pub size_usd: Option<Decimal>,
    pub min_shares: Option<Decimal>,
    pub max_shares: Option<Decimal>,
    /// Relative allocation weight (E16/#98): scales this leg's per-entry
    /// notional (own override or global × weight). A weight can only shrink
    /// the budget — anything above 1 is clamped back to the global cap.
    pub size_weight: Option<Decimal>,
    /// This leg's own equity percentage (#202). `None` = the global
    /// `size_pct`. Clamped to the global when the global is armed (> 0), and
    /// free when the global is off — a per-strategy percentage is then the
    /// only one in force, never a zero.
    pub size_pct: Option<Decimal>,
}

impl StrategySize {
    /// True when at least one sizing dimension overrides the global values (a
    /// `StrategyLimit` that configures only caps reports false).
    pub fn overrides_anything(&self) -> bool {
        self.size_usd.is_some()
            || self.min_shares.is_some()
            || self.max_shares.is_some()
            || self.size_weight.is_some()
            || self.size_pct.is_some()
    }
}

/// The sizing knobs actually in force for one strategy after clamping.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveSizing {
    pub size_usd: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
    /// The equity percentage in force for this leg (#202); 0 = absolute path.
    pub size_pct: Decimal,
    /// Whether a per-strategy override set any of these values.
    pub strategy_scoped: bool,
    /// The weight in force (`None` = unweighted).
    pub size_weight: Option<Decimal>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            scanner: ScannerConfig::default(),
            trend: TrendConfig::default(),
            spread_arb: SpreadArbConfig::default(),
            max_orderbook_stale_ms: DEFAULT_MAX_ORDERBOOK_STALE_MS,
            momentum_window_sec: 30,
            momentum_tol_pct: Decimal::new(3, 2), // 0.03%
            size_usd: Decimal::new(25, 1),        // 2.5
            min_shares: Decimal::from(10),
            max_shares: Decimal::from(10),
            // Off by default: the absolute path is what every deployment runs
            // today, and arming this would change their order sizes silently.
            size_pct: Decimal::ZERO,
            strategy_sizes: HashMap::new(),
        }
    }
}

/// Per-strategy gate bookkeeping (E2-b / #27): how often a strategy's own
/// candidates were stopped by a gate, and how often a declared exemption let one
/// through anyway. Both halves are reported per strategy so an opt-out is
/// never invisible next to an un-exempted strategy's rejection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrategyGateTally {
    pub blocked_timing: u64,
    pub blocked_momentum: u64,
    pub exempted_timing: u64,
    pub exempted_momentum: u64,
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
    /// The strategy whose candidate was blocked — so `blocked.timing` /
    /// `blocked.momentum` are attributable per strategy (E2-b / #27).
    pub strategy: String,
    pub token_id: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub price: Decimal,
    pub reason: BlockReason,
    pub time_left_sec: i64,
}

/// One honoured per-strategy gate exemption (E2-b / #27): the strategy declared
/// it did not need a shared entry gate and the host let its candidate through.
/// Recorded so an opt-out is always auditable — the log line is the Chinese
/// sentence «本单因策略 X 豁免门禁 Y», and the record carries the numeric detail
/// of what the gate would have said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateExemptionRecord {
    pub strategy: String,
    /// "timing" | "momentum".
    pub gate: &'static str,
    pub token_id: String,
    pub asset: String,
    /// The block the gate would have produced (e.g. `"Too close to expiry
    /// (120s < 180s)"` or `"spot BTC -1.20% in 30s vs Up (tol 0.03%)"`).
    pub detail: String,
    pub time_left_sec: i64,
}

impl GateExemptionRecord {
    /// The auditable one-line sentence for logs/diagnostics.
    pub fn audit_line(&self) -> String {
        format!(
            "本单因策略 {} 豁免门禁 {}（{}，token={}）",
            self.strategy, self.gate, self.detail, self.token_id
        )
    }
}

/// Market-data events the engine consumes (produced by the feed layer or, in
/// tests, driven directly).
#[derive(Debug, Clone)]
pub enum DataEvent {
    /// Full or incremental orderbook update for a token.
    Book {
        token_id: String,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        now_ms: i64,
    },
    /// Top-of-book only update (price_change / best_bid_ask).
    TopOfBook {
        token_id: String,
        best_bid: Option<Decimal>,
        best_ask: Option<Decimal>,
        now_ms: i64,
    },
    /// Binance spot price for an asset (e.g. "BTC").
    Spot {
        asset: String,
        price: Decimal,
        now_ms: i64,
    },
    /// Fresh round markets discovered by the scanner.
    RoundMarkets {
        markets: Vec<CryptoMarket>,
        now_ms: i64,
    },
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
    /// Per-strategy gate exemptions HONOURED during the most recent evaluate()
    /// (E2-b / #27) — the auditable "本单因策略 X 豁免门禁 Y" trail.
    last_exemptions: Vec<GateExemptionRecord>,
    /// Records blocked signals + their subsequent price path (for offline
    /// evaluation of relaxing the entry gate). Observation-only.
    near_miss: crate::shadow::NearMissRecorder,
    blocked_timing_total: u64,
    blocked_momentum_total: u64,
    /// Session per-strategy gate counts (blocked vs exempted), E2-b / #27.
    gate_tally: HashMap<String, StrategyGateTally>,
    /// Optional per-strategy hot-parameter registry for Shadow Evolution
    /// (E2-c / #28). When present, each strategy resolves its OWN cell through it
    /// and reads that cell lock-free on the hot path, so an evolution takes effect
    /// on the next tick with no restart — and one strategy's parameters can never
    /// be read by another.
    hot_params: Option<std::sync::Arc<crate::shadow_evolution::ParamRegistry>>,
    /// Registered strategies (PR-B: starts EMPTY — the kernel ships none). Every
    /// strategy arrives through `register_user_strategy`, whether it was built
    /// in-tree in a test or dlopened from a cdylib.
    strategies: Vec<HostedStrategy>,
    /// Strategy close intents gathered during the latest evaluate(), drained by
    /// the host (`Core`) into its shared exit-submission path.
    strategy_exits: Vec<StrategyExitIntent>,
    /// The account's cash equity, pushed by the host before each evaluation
    /// (#202). ZERO = not (yet) supplied. Only the equity-relative sizing reads
    /// it: with `size_pct = 0` the engine's tickets do not depend on this at
    /// all, so an unconfigured deployment cannot be affected by how (or
    /// whether) the host feeds it.
    equity_usd: Decimal,
    /// Signals the equity-relative sizing produced NO ticket for (#202) —
    /// a budget that cannot buy one whole share. Counted rather than silent:
    /// "why is nothing trading?" must be answerable from the stats snapshot.
    size_pct_skipped: u64,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        // ZERO strategies: the kernel has no builtins (PR-B). Anything that
        // trades is registered by the host through the C ABI v2 loader.
        let strategies = Vec::new();
        Self {
            scanner: Scanner::new(cfg.scanner.clone()),
            books: HashMap::new(),
            spot: HashMap::new(),
            pending_tokens: HashSet::new(),
            last_blocked: Vec::new(),
            last_exemptions: Vec::new(),
            near_miss: crate::shadow::NearMissRecorder::new(500, 300_000),
            blocked_timing_total: 0,
            blocked_momentum_total: 0,
            gate_tally: HashMap::new(),
            hot_params: None,
            strategies,
            strategy_exits: Vec::new(),
            cfg,
            equity_usd: Decimal::ZERO,
            size_pct_skipped: 0,
        }
    }

    /// The account equity the equity-relative sizing (#202) is a percentage of
    /// — the host pushes it from the ledger before each evaluation, so the
    /// ticket follows the account it is actually trading. A non-positive value
    /// means "unknown": a leg with `size_pct > 0` then emits nothing, because
    /// sizing against an unknown account has no safe reading, and guessing the
    /// absolute budget back in would silently restore the fixed lot #202 is
    /// about.
    pub fn set_equity_usd(&mut self, equity: Decimal) {
        self.equity_usd = equity;
    }

    pub fn equity_usd(&self) -> Decimal {
        self.equity_usd
    }

    /// Signals skipped because an equity-relative budget could not buy one
    /// whole share (#202).
    pub fn size_pct_skip_count(&self) -> u64 {
        self.size_pct_skipped
    }

    /// The per-entry budget in USD that is actually in force for one strategy:
    /// `equity × pct%` scaled by the leg's weight when the relative mode is
    /// armed, else the absolute `size_usd`. Reported (panel + the sizing gate)
    /// so "what will this account commit per entry?" is a number, not a
    /// reading of the source.
    pub fn entry_budget_usd(&self, strategy: &str) -> Decimal {
        let sizing = self.effective_sizing(strategy);
        if sizing.size_pct <= Decimal::ZERO {
            return sizing.size_usd;
        }
        let raw = self.equity_usd.max(Decimal::ZERO) * sizing.size_pct / Decimal::ONE_HUNDRED;
        match sizing.size_weight {
            Some(w) => (raw * w).max(Decimal::ZERO).min(raw),
            None => raw,
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

    /// Move the per-entry share band, in memory, without a restart (#191).
    ///
    /// Deliberately NOT `set_config`: that one re-notifies every hosted strategy
    /// through `on_config`, a side effect no sizing change asked for. The band is
    /// read live out of `cfg` on every ticket (`compute_shares` →
    /// `effective_sizing` → `global_sizing`), so writing these two fields is the
    /// whole change — and it remains the global ceiling that per-strategy
    /// overrides are clamped by, which is what makes it safe to move at runtime.
    pub fn set_share_band(&mut self, min_shares: Decimal, max_shares: Decimal) {
        self.cfg.min_shares = min_shares;
        self.cfg.max_shares = max_shares;
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
        let round = self.scanner.round_timing(now_ms);
        let fresh =
            |token: &str| fresh_book(&self.books, token, now_ms, self.cfg.max_orderbook_stale_ms);
        let ctx = StrategyCtx::new(
            self.scanner.markets(),
            round.slot,
            round.time_left_sec,
            now_ms,
            &fresh,
        );
        self.strategies
            .iter()
            .flat_map(|s| s.strategy.diagnostics(&ctx))
            .collect()
    }

    pub fn book_snapshot(&self, token_id: &str) -> Option<OrderbookSnapshot> {
        self.books
            .get(token_id)
            .filter(|b| !b.is_empty())
            .map(|b| b.snapshot(token_id))
    }

    /// Feed one market-data event. Returns tokens whose confirmed trend broke so
    /// the caller can cancel their resting bids.
    pub fn on_data(&mut self, ev: DataEvent) -> Vec<(String, Decimal)> {
        match ev {
            DataEvent::Book {
                token_id,
                bids,
                asks,
                now_ms,
            } => {
                // The book for an in-flight token almost always exists already
                // (markets arrive before their books); a get_mut hit avoids the
                // owned-key String that `entry` would allocate per event.
                let b = match self.books.get_mut(&token_id) {
                    Some(b) => b,
                    None => self.books.entry(token_id.clone()).or_default(),
                };
                b.apply_snapshot(&bids, &asks, now_ms);
                let snap = b.snapshot(&token_id);
                for s in &mut self.strategies {
                    s.strategy.on_book(&token_id, &snap, now_ms);
                }
                self.near_miss.on_book(&token_id, &snap, now_ms);
            }
            DataEvent::TopOfBook {
                token_id,
                best_bid,
                best_ask,
                now_ms,
            } => {
                let b = self.books.entry(token_id.clone()).or_default();
                b.update_top(best_bid, best_ask, now_ms);
                let snap = b.snapshot(&token_id);
                for s in &mut self.strategies {
                    s.strategy.on_book(&token_id, &snap, now_ms);
                }
                self.near_miss.on_book(&token_id, &snap, now_ms);
            }
            DataEvent::Spot {
                asset,
                price,
                now_ms,
            } => {
                self.spot
                    .entry(asset)
                    .or_insert_with(|| PriceBuffer::new(180))
                    .push(price, now_ms);
                return Vec::new();
            }
            DataEvent::RoundMarkets { markets, now_ms } => {
                // The venue's own round declaration beats the local slot grid:
                // `expires_at_ms` is the exchange's actual round end, and
                // `time_left_sec` (the exit policy's and the D-31 floor's
                // clock) must track it, not the host's wall clock. Without
                // this, a misaligned or shifted venue schedule silently
                // drifts every timing decision. `observe_end_time` ignores
                // non-positive declarations and derives the clock offset.
                if let Some(m) = markets.first() {
                    self.scanner.observe_end_time(m.expires_at_ms, now_ms);
                }
                let slot = markets.first().map(|m| m.round_slot).unwrap_or(0);
                // Real round timing for the transition: how much of the round
                // was left at this instant, straight from the scanner.
                let time_left_sec = self.scanner.round_timing(now_ms).time_left_sec;
                for s in &mut self.strategies {
                    s.strategy.on_round(slot, time_left_sec, now_ms);
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
    /// Returns the reason it rejected, so an exempting strategy's audit record can
    /// state exactly what was waived.
    fn momentum_ok(&self, asset: &str, dir: SignalDirection, now_ms: i64) -> Result<(), String> {
        let Some(buf) = self.spot.get(asset) else {
            return Ok(());
        };
        let move_pct = buf.move_pct(self.cfg.momentum_window_sec, now_ms);
        let against = match dir {
            SignalDirection::Up => move_pct < -self.cfg.momentum_tol_pct,
            SignalDirection::Down => move_pct > self.cfg.momentum_tol_pct,
        };
        if against {
            return Err(format!(
                "spot {asset} {move_pct:+.2}% in {}s vs {} (tol {}%)",
                self.cfg.momentum_window_sec,
                dir.as_str(),
                self.cfg.momentum_tol_pct
            ));
        }
        Ok(())
    }

    /// Evaluate all enabled strategies against the current round and return the
    /// entry orders that should be placed. Pure w.r.t. the core: the caller
    /// places them and reports back tokens that got an order so repeats are
    /// suppressed this round.
    pub fn evaluate(&mut self, now_ms: i64) -> Vec<crate::model::OrderRequest> {
        self.last_blocked.clear();
        self.last_exemptions.clear();

        let round = self.scanner.round_timing(now_ms);
        let timing = self.scanner.can_trade_reason(now_ms).err();

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
            if let Some(block) = timing {
                // E2-b: a strategy may declare the timing WINDOW gates unnecessary
                // for its own candidates. The structural precondition (no market
                // this round) is not waivable, and everything downstream in
                // `Core` (risk gate, kill switch, daily loss, quotas, sizing) is
                // untouched by this path.
                //
                // D-31: the exemption is honoured only while `time_left_sec` is at
                // or above the strategy's declared floor (default: the scanner's
                // own `min_time_left_sec`). Before this, an exempt strategy could
                // enter inside the force-exit window, where the exit policy fires
                // on the very next tick — a correctly-priced but guaranteed
                // zero-hold exit that only put noise into the ledger. A "too young"
                // round has a large `time_left_sec`, so it stays waived; the block
                // this actually stops is "too close to expiry", which by definition
                // sits below the floor.
                let waived = block.exemptible()
                    && self
                        .strategy_gate_exemptions(&sig.strategy)
                        .is_some_and(|e| {
                            e.timing
                                && round.time_left_sec
                                    >= e.timing_floor_sec(self.cfg.scanner.min_time_left_sec)
                        });
                if !waived {
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
                        strategy: sig.strategy,
                        token_id: sig.token_id,
                        asset: sig.asset,
                        direction: sig.direction,
                        price: sig.price,
                        reason: BlockReason::Timing,
                        time_left_sec: round.time_left_sec,
                    });
                    continue;
                }
                self.last_exemptions.push(GateExemptionRecord {
                    strategy: sig.strategy.clone(),
                    gate: "timing",
                    token_id: sig.token_id.clone(),
                    asset: sig.asset.clone(),
                    detail: block.to_string(),
                    time_left_sec: round.time_left_sec,
                });
            }
            if let Err(detail) = self.momentum_ok(&sig.asset, sig.direction, now_ms) {
                let waived = self
                    .strategy_gate_exemptions(&sig.strategy)
                    .is_some_and(|e| e.momentum);
                if !waived {
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
                        strategy: sig.strategy,
                        token_id: sig.token_id,
                        asset: sig.asset,
                        direction: sig.direction,
                        price: sig.price,
                        reason: BlockReason::Momentum,
                        time_left_sec: round.time_left_sec,
                    });
                    continue;
                }
                self.last_exemptions.push(GateExemptionRecord {
                    strategy: sig.strategy.clone(),
                    gate: "momentum",
                    token_id: sig.token_id.clone(),
                    asset: sig.asset.clone(),
                    detail,
                    time_left_sec: round.time_left_sec,
                });
            }

            // A strategy-declared share count (pair legs must match in SHARES,
            // not in notional) is honoured only inside the kernel's risk band:
            // capped at the same `max_shares` ceiling the notional path obeys,
            // so a declaration can never oversize. A declared count below one
            // share buys NOTHING — falling back to notional sizing would open
            // the naked leg of a pair — and `None` keeps notional sizing.
            let size = match sig.shares {
                Some(n) => {
                    if n < Decimal::ONE {
                        continue;
                    }
                    n.min(self.effective_sizing(&sig.strategy).max_shares)
                }
                // #202: an equity-relative budget too small for one whole share
                // produces NO order (counted, not emitted as a 0-size rejection).
                None => {
                    let Some(size) = self.entry_ticket(sig.price, &sig.strategy) else {
                        self.size_pct_skipped += 1;
                        continue;
                    };
                    size
                }
            };
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

    /// The per-strategy gate exemptions registered strategies declared
    /// (E2-b / #27). None for an unknown strategy.
    pub fn strategy_gate_exemptions(&self, name: &str) -> Option<GateExemptions> {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .map(|s| s.strategy.gate_exemptions())
    }

    /// Every declared exemption, as `(strategy, exemptions)` in registration
    /// order — the audit view of who opted out of what.
    pub fn declared_gate_exemptions(&self) -> Vec<(String, GateExemptions)> {
        self.strategies
            .iter()
            .map(|s| (s.strategy.name().to_string(), s.strategy.gate_exemptions()))
            .filter(|(_, e)| e.any())
            .collect()
    }

    /// Whether the named strategy declared hold-to-settlement semantics: its
    /// positions are meant to be redeemed at expiry, not sold on the exit
    /// ladder (a complete-set pair pays $1 at settlement regardless of which
    /// side wins — selling a leg before expiry would destroy that payoff).
    /// Read by the host's exit checks; `false` for an unknown strategy.
    pub fn strategy_holds_to_settlement(&self, name: &str) -> bool {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .is_some_and(|s| s.strategy.holds_to_settlement())
    }

    /// Gate exemptions honoured on the most recent evaluation, in the order the
    /// candidates were evaluated. Cleared at the start of every `evaluate`.
    pub fn last_exemptions(&self) -> &[GateExemptionRecord] {
        &self.last_exemptions
    }

    /// Drain the honoured-exemption records (called by the host after evaluate)
    /// and fold them into the per-strategy tallies.
    pub fn take_exemptions(&mut self) -> Vec<GateExemptionRecord> {
        let out = std::mem::take(&mut self.last_exemptions);
        for r in &out {
            let t = self.gate_tally.entry(r.strategy.clone()).or_default();
            match r.gate {
                "timing" => t.exempted_timing += 1,
                _ => t.exempted_momentum += 1,
            }
        }
        out
    }

    /// Blocked / exempted counts for one strategy (E2-b / #27).
    pub fn gate_tally(&self, name: &str) -> StrategyGateTally {
        self.gate_tally.get(name).copied().unwrap_or_default()
    }

    /// Accumulate blocked counters (called by the caller after evaluate).
    pub fn tally_blocked(&mut self) {
        for b in &self.last_blocked {
            match b.reason {
                BlockReason::Timing => self.blocked_timing_total += 1,
                BlockReason::Momentum => self.blocked_momentum_total += 1,
            }
            let t = self.gate_tally.entry(b.strategy.clone()).or_default();
            match b.reason {
                BlockReason::Timing => t.blocked_timing += 1,
                BlockReason::Momentum => t.blocked_momentum += 1,
            }
        }
    }

    /// Attach (or detach) the Shadow Evolution per-strategy parameter registry.
    /// Called when evolution is (re)configured; idempotent. Forwarded to every
    /// registered strategy, which resolves its OWN cell from it (E2-c / #28).
    ///
    /// `None` DETACHES: strategies fall back to the config pushed via
    /// `on_config`, which is what makes "evolution disabled" provably identical
    /// to the pre-feature behaviour rather than merely inert by convention.
    pub fn set_hot_params(
        &mut self,
        registry: Option<std::sync::Arc<crate::shadow_evolution::ParamRegistry>>,
    ) {
        self.hot_params = registry.clone();
        for s in &mut self.strategies {
            s.strategy.set_hot_params(registry.clone());
        }
    }

    pub fn has_hot_params(&self) -> bool {
        self.hot_params.is_some()
    }

    /// Per-strategy config-in-force views as (name, JSON string) for every
    /// strategy that reports one (observability). An external library reports
    /// through its OPTIONAL `bk_strategy_config_view` symbol; strategies that
    /// declare nothing are simply absent. The kernel keeps no knowledge of any
    /// strategy's parameter names.
    pub fn strategy_config_views(&self) -> Vec<(String, String)> {
        self.strategies
            .iter()
            .filter_map(|s| {
                s.strategy
                    .config_view_json()
                    .map(|v| (s.strategy.name().to_string(), v))
            })
            .collect()
    }

    /// Strategy names the engine knows about, in registration order (PR-B: the
    /// kernel ships none, so this is exactly what the loader registered).
    pub fn supported_strategies(&self) -> Vec<String> {
        self.strategies
            .iter()
            .map(|s| s.strategy.name().to_string())
            .collect()
    }
    /// Borrow every hosted strategy. Shadow Evolution asks each one which knobs it
    /// declares (E2-c), so the declaration has to be read off the live instances.
    ///
    /// Deliberately NOT filtered by `enabled`: `register_strategies` drops the cell
    /// of any strategy it is not handed, so filtering here would make a runtime
    /// disable destroy that strategy's evolved parameters and rollback anchor.
    /// A switched-off strategy just never emits the candidates that would use them.
    pub fn strategy_refs(&self) -> Vec<&dyn EngineStrategy> {
        self.strategies
            .iter()
            .map(|s| s.strategy.as_ref())
            .collect()
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
    /// This is the ONLY way a strategy enters the engine. The kernel ships none
    /// (PR-B): external v2 dylibs come through the C ABI v2 loader, which calls
    /// this. It starts DISABLED: an explicit enable is required before it can
    /// place orders. Returns the registered name, or an error when the name is
    /// taken.
    pub fn register_user_strategy(
        &mut self,
        strategy: Box<dyn EngineStrategy>,
        source: String,
    ) -> Result<String, String> {
        let name = strategy.name().to_string();
        if self.strategies.iter().any(|s| s.strategy.name() == name) {
            return Err(format!("strategy name already registered: {name}"));
        }
        let mut hosted = HostedStrategy {
            strategy,
            enabled: false,
            source,
        };
        // Hand the host config to the newly registered strategy.
        hosted
            .strategy
            .on_config(&self.cfg.trend, &self.cfg.spread_arb);
        // Registered after Shadow Evolution was configured? Forward the handle
        // so a later evolution still reaches this strategy.
        if let Some(h) = &self.hot_params {
            hosted.strategy.set_hot_params(Some(h.clone()));
        }
        self.strategies.push(hosted);
        Ok(name)
    }

    /// Where a registered strategy came from (`dylib:<path>`, `test`, ...) —
    /// free-form provenance the loader supplies. The kernel itself has no
    /// `builtin` class any more.
    pub fn strategy_source(&self, name: &str) -> Option<&str> {
        self.strategies
            .iter()
            .find(|s| s.strategy.name() == name)
            .map(|s| s.source.as_str())
    }

    /// Remove a strategy from the live dispatch (E9-b `strategy.unload` /
    /// `strategy.reload`).
    ///
    /// A registered strategy is DYNAMIC by construction — the kernel ships
    /// none — so every registered name is removable. The guard refuses while the
    /// strategy is ENABLED: an operator must disable it first, so a
    /// still-trading strategy is never silently pulled from under a running
    /// session. Returns `Err` with the reason on any refusal, `Ok(true)` when
    /// removed, `Ok(false)` when the name is unknown.
    pub fn unregister_user_strategy(&mut self, name: &str) -> Result<bool, String> {
        let Some(idx) = self
            .strategies
            .iter()
            .position(|s| s.strategy.name() == name)
        else {
            return Ok(false);
        };
        if self.strategies[idx].enabled {
            return Err(format!(
                "strategy {name} is ENABLED — disable it first (strategy.enable {name} false)"
            ));
        }
        self.strategies.remove(idx);
        Ok(true)
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

    fn compute_shares(&self, price: Decimal, strategy: &str) -> Decimal {
        let sizing = self.effective_sizing(strategy);
        // P0 #202: the equity-relative mode REPLACES the absolute budget when
        // it is armed. Everything below this line is the historical path, and
        // with `size_pct = 0` (the default) it is reached unchanged.
        if sizing.size_pct > Decimal::ZERO {
            return self.equity_shares(price, &sizing);
        }
        // A zero budget zeroes the leg outright (an explicit weight-0 off
        // switch): the share floor exists to protect a real entry, not to
        // resurrect a disabled one.
        if sizing.size_usd <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        if price <= Decimal::ZERO {
            return sizing.min_shares;
        }
        let raw = (sizing.size_usd / price).round();
        raw.max(sizing.min_shares).min(sizing.max_shares)
    }

    /// The ticket for one signal, or `None` when the configured sizing produces
    /// NO order at all (#202). Only the equity-relative mode can answer `None`
    /// — an absolute budget of 0 keeps emitting its historical zero-size order,
    /// so an unconfigured deployment is untouched.
    ///
    /// A relative budget that cannot buy one whole share is a statement about
    /// the account (too small for this percentage), and a 0-size order would
    /// only convert it into a per-tick "size must be positive" rejection; the
    /// skip is counted instead (`size_pct_skip_count`) so it stays visible.
    fn entry_ticket(&self, price: Decimal, strategy: &str) -> Option<Decimal> {
        let size = self.compute_shares(price, strategy);
        if size > Decimal::ZERO {
            return Some(size);
        }
        if self.effective_sizing(strategy).size_pct > Decimal::ZERO {
            return None;
        }
        Some(size)
    }

    /// The equity-relative ticket (#202).
    ///
    /// `budget = equity × pct% × weight`; the ticket is the WHOLE number of
    /// shares that fits inside it (floor, not round — a percentage budget is an
    /// upper bound, and rounding up would commit up to a share more than the
    /// operator allowed). `max_shares` still caps the lot.
    ///
    /// `min_shares` deliberately does NOT participate here: a share floor that
    /// can raise a ticket over its budget is the #202 bug itself (the default
    /// band [10, 10] is what turned a 4.8 USDC book into 10-share tickets), and
    /// this mode exists to remove it. One whole share is the floor of last
    /// resort; a budget that cannot buy even that emits nothing (see
    /// `entry_ticket`).
    fn equity_shares(&self, price: Decimal, sizing: &EffectiveSizing) -> Decimal {
        if self.equity_usd <= Decimal::ZERO || price <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let budget = match sizing.size_weight {
            Some(w) => {
                let raw = self.equity_usd * sizing.size_pct / Decimal::ONE_HUNDRED;
                (raw * w).max(Decimal::ZERO).min(raw)
            }
            None => self.equity_usd * sizing.size_pct / Decimal::ONE_HUNDRED,
        };
        if budget <= Decimal::ZERO || sizing.max_shares <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        (budget / price)
            .floor()
            .max(Decimal::ZERO)
            .min(sizing.max_shares)
    }

    /// The freshness budget in force (#205): the engine refuses to price a book
    /// older than this, and `0` means the check is OFF. Read by the panel's
    /// `engine.stats.orderbookFreshness` so the value that actually gates entries
    /// is reported by the engine that applies it, not re-derived from the config.
    pub fn max_orderbook_stale_ms(&self) -> i64 {
        self.cfg.max_orderbook_stale_ms
    }

    /// The global sizing band: what a strategy without an override gets, and
    /// the CEILING every override is clamped to (E2-a). Also the band a report
    /// reads before any strategy is named (#202's worst-case-order view).
    pub fn global_sizing(&self) -> EffectiveSizing {
        EffectiveSizing {
            size_usd: self.cfg.size_usd,
            min_shares: self.cfg.min_shares,
            max_shares: self.cfg.max_shares,
            size_pct: self.cfg.size_pct,
            strategy_scoped: false,
            size_weight: None,
        }
    }

    /// The sizing in force for one strategy (E2-a): its own override where
    /// configured, otherwise the globals — then clamped so a strategy can never
    /// spend more notional nor hold more shares than the global risk allows.
    /// `min_shares` is also capped by `max_shares` so the reported band is
    /// always coherent (the ceiling wins when an override overshoots).
    pub fn effective_sizing(&self, strategy: &str) -> EffectiveSizing {
        let globals = self.global_sizing();
        let Some(over) = self
            .cfg
            .strategy_sizes
            .get(strategy)
            .filter(|s| s.overrides_anything())
        else {
            return globals;
        };
        let mut size_usd = over
            .size_usd
            .map_or(globals.size_usd, |v| v.min(globals.size_usd));
        if let Some(w) = over.size_weight {
            // Weight re-weights this leg's share of the per-entry budget; it
            // never widens the global cap, and a non-positive weight zeroes the
            // leg out entirely (an explicit off switch).
            size_usd = (size_usd * w).max(Decimal::ZERO).min(globals.size_usd);
        }
        let max_shares = over
            .max_shares
            .map_or(globals.max_shares, |v| v.min(globals.max_shares));
        let min_shares = over
            .min_shares
            .map_or(globals.min_shares, |v| v.max(globals.min_shares))
            .min(max_shares);
        // #202: the global percentage is the ceiling when it is armed. An
        // unarmed (0) global is "no opinion", not a zero ceiling, so a
        // per-strategy percentage is what a single leg gets configured with.
        let size_pct = match over.size_pct {
            Some(v) if globals.size_pct > Decimal::ZERO => v.min(globals.size_pct),
            Some(v) => v.max(Decimal::ZERO),
            None => globals.size_pct,
        };
        EffectiveSizing {
            size_usd,
            min_shares,
            max_shares,
            size_pct,
            strategy_scoped: true,
            size_weight: over.size_weight,
        }
    }
}

/// Book snapshot for pricing, but only while it is fresh enough. Mirrors the
/// engine's historical freshness rule (non-empty book + `max_orderbook_stale_ms`).
/// The freshness check runs off `LocalBook`'s own timestamp, so a stale book
/// never pays for a snapshot allocation at all.
///
/// `max_stale_ms <= 0` is the operator's explicit "freshness check OFF" (#205):
/// the book is then accepted however old it is, and only an EMPTY book still has
/// nothing to price off. Anything else keeps the historic rule, so the default
/// is unchanged.
fn fresh_book(
    books: &HashMap<String, LocalBook>,
    token_id: &str,
    now_ms: i64,
    max_stale_ms: i64,
) -> Option<OrderbookSnapshot> {
    let b = books.get(token_id)?;
    if b.is_empty() || (max_stale_ms > 0 && now_ms - b.timestamp() > max_stale_ms) {
        return None;
    }
    Some(b.snapshot(token_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::RiskConfig;
    use crate::service::{Core, CoreConfig};
    use rust_decimal_macros::dec;

    fn cfg() -> EngineConfig {
        EngineConfig {
            scanner: ScannerConfig {
                assets: vec!["BTC".into()],
                round_duration_sec: 900,
                min_round_age_sec: 0,
                min_time_left_sec: 0,
            },
            trend: TrendConfig {
                confirm_sec: 5,
                ratio: dec!(0.5),
                min_price: dec!(0.5),
                broken_price: dec!(0.35),
                window_floor_ms: 0,
            },
            spread_arb: SpreadArbConfig {
                trend_max_entry_price: dec!(0.45),
                ..Default::default()
            },
            max_orderbook_stale_ms: DEFAULT_MAX_ORDERBOOK_STALE_MS,
            momentum_window_sec: 30,
            momentum_tol_pct: dec!(0.03),
            size_usd: dec!(2.5),
            min_shares: dec!(10),
            max_shares: dec!(10),
            size_pct: Decimal::ZERO,
            strategy_sizes: HashMap::new(),
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
            risk: RiskConfig {
                max_order_notional: dec!(5),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            ..Default::default()
        });
        // Keep the exit engine out of the way for signal tests.
        let cc = c.config_mut();
        cc.auto_exits_enabled = false;
        c
    }

    /// An engine with the three TEST-ONLY adapters hosted (`spread_arb` enabled,
    /// the other two registered-but-off) — the dispatch shape the deleted
    /// builtins had, so these tests keep exercising candidate dispatch, gates
    /// and hot parameters. The kernel itself ships no strategy (PR-B).
    fn engine() -> Engine {
        engine_from(cfg())
    }

    fn engine_from(cfg: EngineConfig) -> Engine {
        let mut e = Engine::new(cfg.clone());
        crate::strategies::test_support::host(&mut e, cfg.trend, cfg.spread_arb);
        e
    }

    #[test]
    fn engine_places_a_trend_confirmed_dip_buy() {
        let mut e = engine();
        let mut c = core();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });

        // Confirm the UP trend: several books with mid >= 0.5 within the window.
        for i in 0..12 {
            let t = now + i * 1000;
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: t,
            });
        }
        // Dip: mid 0.44 (bid 0.43) → entry 0.43 ≤ 0.45 → signal.
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        // Spot calm.
        e.on_data(DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(60000),
            now_ms: now + 12_000,
        });

        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "expected one spread_arb entry");
        // Placing through the core succeeds (funds seeded) and fills on the book.
        let req = orders.into_iter().next().unwrap();
        let (id, _) = c.place(req, 5000, now + 12_000).unwrap();
        assert!(!id.is_empty());
    }

    #[test]
    fn engine_skips_without_confirmed_trend() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // Only one dip book; no confirm window yet.
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now,
        });
        assert!(e.evaluate(now).is_empty());
    }

    #[test]
    fn engine_blocks_entry_when_spot_moves_against() {
        let mut e = engine();
        let _c = core();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        // Spot falls hard over the momentum window → UP entry blocked.
        e.on_data(DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(60000),
            now_ms: now + 1000,
        });
        e.on_data(DataEvent::Spot {
            asset: "BTC".into(),
            price: dec!(59000),
            now_ms: now + 12_000,
        });
        assert!(
            e.evaluate(now + 12_000).is_empty(),
            "spot moving against should block the entry"
        );
    }

    #[test]
    fn trend_break_is_reported_for_bid_cancellation() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        let broken = e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.30), dec!(100))],
            asks: vec![(dec!(0.32), dec!(100))],
            now_ms: now + 12_000,
        });
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].0, "up");
    }

    #[test]
    fn records_near_miss_when_timing_gate_blocks_a_valid_dip() {
        // A valid trend-confirmed dip that the timing gate rejects must be
        // counted (this is the near-miss telemetry the shadow log lacks).
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000; // force the timing gate to fail
        let mut e = engine_from(cfg);
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });

        let orders = e.evaluate(now + 12_000);
        assert!(orders.is_empty(), "timing gate must block the order");
        assert_eq!(
            e.last_blocked().len(),
            1,
            "the blocked dip should be recorded"
        );
        assert_eq!(e.last_blocked()[0].reason, BlockReason::Timing);
        e.tally_blocked();
        assert_eq!(e.blocked_timing_count(), 1);
        assert_eq!(e.blocked_momentum_count(), 0);
    }

    #[test]
    fn compute_shares_clamps_to_bounds() {
        let e = engine();
        // 2.5 / 0.25 = 10 shares within [10,10].
        assert_eq!(e.compute_shares(dec!(0.25), "spread_arb"), dec!(10));
        // Very low price would exceed max → clamped to 10.
        assert_eq!(e.compute_shares(dec!(0.01), "spread_arb"), dec!(10));
    }

    #[test]
    fn compute_shares_honours_custom_bounds() {
        // Small-balance live sizing: a 4-share fixed lot (2 positions on ~4.8u).
        let mut c = cfg();
        c.size_usd = dec!(2.5);
        c.min_shares = dec!(4);
        c.max_shares = dec!(4);
        let e = engine_from(c);
        // 2.5 / 0.45 ≈ 5.56 → rounds to 6, then clamped down to the 4-share lot.
        assert_eq!(e.compute_shares(dec!(0.45), "spread_arb"), dec!(4));
        // A wide band lets the nominal size win: 2.5 / 0.25 = 10 within [2,20].
        let mut c2 = cfg();
        c2.min_shares = dec!(2);
        c2.max_shares = dec!(20);
        let e2 = engine_from(c2);
        assert_eq!(e2.compute_shares(dec!(0.25), "spread_arb"), dec!(10));
        assert_eq!(e2.compute_shares(dec!(1.25), "spread_arb"), dec!(2));
    }

    // ── #202 equity-relative sizing ─────────────────────────────────────────

    /// A percentage engine. The absolute `size_usd` stays populated (2.5) even
    /// though the relative mode must ignore it, so any leak of the old budget
    /// into the ticket shows up as a wrong number here.
    fn pct_cfg(pct: Decimal, min_shares: Decimal, max_shares: Decimal) -> EngineConfig {
        let mut c = cfg();
        c.size_usd = dec!(2.5);
        c.size_pct = pct;
        c.min_shares = min_shares;
        c.max_shares = max_shares;
        c
    }

    #[test]
    fn equity_sizing_replaces_the_fixed_lot_and_follows_the_account() {
        let mut e = engine_from(pct_cfg(dec!(20), dec!(10), dec!(1000)));
        e.set_equity_usd(dec!(4.8));
        // 4.8 × 20% = 0.96 USD → 0.96/0.40 = 2.4 → 2 whole shares (floor: a
        // percentage budget is an upper bound). The absolute path sends 10
        // shares = 4.00 USD = 83% of this book.
        assert_eq!(e.compute_shares(dec!(0.40), "spread_arb"), dec!(2));
        assert_eq!(e.entry_budget_usd("spread_arb"), dec!(0.96));
        // The band's own floor (min_shares = 10) must NOT lift it back over the
        // budget — that floor is the #202 bug itself.
        assert_eq!(e.effective_sizing("spread_arb").min_shares, dec!(10));
        assert_eq!(e.compute_shares(dec!(0.40), "spread_arb"), dec!(2));
        // The same percentage on a 100× account is 100× the budget, and the lot
        // follows (the point of the knob: 4.8 → 480 must move the order size).
        e.set_equity_usd(dec!(480));
        assert_eq!(e.entry_budget_usd("spread_arb"), dec!(96));
        assert_eq!(e.compute_shares(dec!(0.40), "spread_arb"), dec!(240));
        // The old ceiling still wins when it is the tighter one: the shipped
        // [10, 10] band saturates even a 480 USD account at 10 shares (4.00 USD,
        // 0.8% of it), so arming `size_pct` means revisiting `max_shares`.
        let mut shipped = engine_from(pct_cfg(dec!(20), dec!(10), dec!(10)));
        shipped.set_equity_usd(dec!(480));
        assert_eq!(shipped.compute_shares(dec!(0.40), "spread_arb"), dec!(10));
    }

    #[test]
    fn equity_sizing_needs_a_budget_it_can_actually_spend() {
        // No equity pushed yet: sizing against an unknown account has no safe
        // reading, and falling back to the absolute lot would silently restore
        // the fixed 10 shares this knob exists to remove.
        let mut unknown = engine_from(pct_cfg(dec!(20), dec!(10), dec!(1000)));
        assert_eq!(
            unknown.compute_shares(dec!(0.40), "spread_arb"),
            Decimal::ZERO
        );
        unknown.set_equity_usd(dec!(4.8));
        assert_eq!(unknown.compute_shares(dec!(0.40), "spread_arb"), dec!(2));
        // A budget that cannot buy one whole share (1% of 4.8 = 0.048 at 0.40)
        // emits NO order rather than a 0-size one that becomes a per-tick
        // rejection; the caller counts the skip.
        let mut tiny = engine_from(pct_cfg(dec!(1), dec!(10), dec!(1000)));
        tiny.set_equity_usd(dec!(4.8));
        assert_eq!(tiny.compute_shares(dec!(0.40), "spread_arb"), Decimal::ZERO);
        assert_eq!(tiny.entry_ticket(dec!(0.40), "spread_arb"), None);
        // A non-positive price and a zero ceiling are dead ends too, never a
        // panic or an unbounded ticket.
        assert_eq!(
            tiny.equity_shares(Decimal::ZERO, &tiny.global_sizing()),
            Decimal::ZERO
        );
        let mut closed = engine_from(pct_cfg(dec!(20), dec!(10), Decimal::ZERO));
        closed.set_equity_usd(dec!(4.8));
        assert_eq!(
            closed.compute_shares(dec!(0.40), "spread_arb"),
            Decimal::ZERO
        );
    }

    #[test]
    fn per_strategy_equity_pct_is_clamped_by_the_global() {
        let over = |pct: Decimal, weight: Option<Decimal>| StrategySize {
            size_usd: None,
            min_shares: None,
            max_shares: None,
            size_weight: weight,
            size_pct: Some(pct),
        };
        // Global 20%: a 50% leg is clamped down to it, a 5% leg is kept.
        let mut c = pct_cfg(dec!(20), dec!(10), dec!(1000));
        c.strategy_sizes = [
            ("greedy".to_string(), over(dec!(50), None)),
            ("timid".to_string(), over(dec!(5), None)),
        ]
        .into();
        let mut e = engine_from(c);
        e.set_equity_usd(dec!(100));
        assert_eq!(e.effective_sizing("greedy").size_pct, dec!(20));
        assert!(e.effective_sizing("greedy").strategy_scoped);
        assert_eq!(e.entry_budget_usd("greedy"), dec!(20));
        assert_eq!(e.compute_shares(dec!(0.40), "greedy"), dec!(50));
        assert_eq!(e.compute_shares(dec!(0.40), "timid"), dec!(12)); // 5/0.4 = 12.5
        // A weight scales the leg's budget inside its own percentage.
        let mut c2 = pct_cfg(dec!(20), dec!(10), dec!(1000));
        c2.strategy_sizes = [("half".to_string(), over(dec!(20), Some(dec!(0.5))))].into();
        let mut e2 = engine_from(c2);
        e2.set_equity_usd(dec!(100));
        assert_eq!(e2.entry_budget_usd("half"), dec!(10));
        assert_eq!(e2.compute_shares(dec!(0.40), "half"), dec!(25));
        // Global OFF is "no opinion", not a zero ceiling: a leg configured with
        // its own percentage is then the only one in force, and a leg without
        // one stays on the absolute path.
        let mut c3 = pct_cfg(Decimal::ZERO, dec!(10), dec!(1000));
        c3.strategy_sizes = [("only".to_string(), over(dec!(10), None))].into();
        let mut e3 = engine_from(c3);
        e3.set_equity_usd(dec!(100));
        assert_eq!(e3.entry_budget_usd("only"), dec!(10));
        assert_eq!(e3.compute_shares(dec!(0.40), "only"), dec!(25));
        assert_eq!(e3.effective_sizing("unset").size_pct, Decimal::ZERO);
    }

    /// The shipped default must be untouched by this PR: with `size_pct = 0` the
    /// engine ignores the equity entirely, so the service pushing a balance once
    /// per cycle (as it now always does) cannot move one share of an
    /// unconfigured deployment's order flow.
    #[test]
    fn unconfigured_sizing_ignores_the_equity_entirely() {
        let mut e = engine_from(cfg()); // size_pct = 0, size_usd 2.5, band [10, 10]
        let before = e.compute_shares(dec!(0.25), "spread_arb");
        assert_eq!(before, dec!(10));
        e.set_equity_usd(dec!(4.8));
        assert_eq!(e.compute_shares(dec!(0.25), "spread_arb"), before);
        assert_eq!(e.entry_ticket(dec!(0.25), "spread_arb"), Some(before));
        assert_eq!(e.entry_budget_usd("spread_arb"), dec!(2.5));
        // Even a zero absolute budget keeps the historical zero-size order (that
        // path's own off switch) instead of turning into a silent skip.
        let mut off = cfg();
        off.size_usd = Decimal::ZERO;
        let off = engine_from(off);
        assert_eq!(off.compute_shares(dec!(0.25), "spread_arb"), Decimal::ZERO);
        assert_eq!(
            off.entry_ticket(dec!(0.25), "spread_arb"),
            Some(Decimal::ZERO)
        );
    }

    // ── E2-a per-strategy sizing ────────────────────────────────────────────

    fn sizing_cfg(overrides: &[(&str, StrategySize)]) -> EngineConfig {
        let mut c = cfg();
        c.size_usd = dec!(10);
        c.min_shares = dec!(2);
        c.max_shares = dec!(20);
        c.strategy_sizes = overrides
            .iter()
            .map(|(n, s)| ((*n).to_string(), s.clone()))
            .collect();
        c
    }

    #[test]
    fn strategy_sizing_falls_back_to_the_globals_when_absent() {
        let e = engine_from(sizing_cfg(&[]));
        // No entry at all → the global band, and reported as not strategy-scoped.
        let s = e.effective_sizing("whatever");
        assert_eq!(s.size_usd, dec!(10));
        assert_eq!(s.min_shares, dec!(2));
        assert_eq!(s.max_shares, dec!(20));
        assert!(!s.strategy_scoped);
        assert_eq!(e.compute_shares(dec!(0.50), "whatever"), dec!(20)); // 10/0.5=20

        // A caps-only entry (no sizing fields) must behave exactly like absent.
        let e2 = engine_from(sizing_cfg(&[("capped", StrategySize::default())]));
        assert_eq!(e2.effective_sizing("capped").size_usd, dec!(10));
        assert!(!e2.effective_sizing("capped").strategy_scoped);
    }

    #[test]
    fn strategy_sizing_overrides_take_effect_inside_the_global_band() {
        // Global band here is [2,20] on a 10u budget (see `sizing_cfg`).
        let e = engine_from(sizing_cfg(&[(
            "small",
            StrategySize {
                size_usd: Some(dec!(1)),
                min_shares: Some(dec!(1)),
                max_shares: Some(dec!(2)),
                size_weight: None,
                size_pct: None,
            },
        )]));
        let s = e.effective_sizing("small");
        // The 1-share floor is raised to the global floor (2): the global band
        // bounds the strategy from BELOW as well, so a lot can never fall under
        // the venue/risk minimum.
        assert_eq!(
            (s.size_usd, s.min_shares, s.max_shares),
            (dec!(1), dec!(2), dec!(2))
        );
        assert!(s.strategy_scoped);
        // 1 / 0.45 ≈ 2.2 → rounds to 2, inside [2,2].
        assert_eq!(e.compute_shares(dec!(0.45), "small"), dec!(2));
        // 1 / 0.05 = 20 → clamped to the strategy's own 2-share cap.
        assert_eq!(e.compute_shares(dec!(0.05), "small"), dec!(2));
        // 1 / 2.00 = 0.5 → rounds to 1, then raised to the global 2-share floor.
        assert_eq!(e.compute_shares(dec!(2.00), "small"), dec!(2));
        // The un-configured sibling keeps using the globals in the same engine.
        assert_eq!(e.compute_shares(dec!(0.50), "other"), dec!(20));
    }

    /// E16/#98 weighted allocation: a weight re-weights the leg's share of the
    /// per-entry budget and can never widen the global cap; a non-positive
    /// weight zeroes the leg out.
    #[test]
    fn strategy_size_weight_reweights_but_never_widens() {
        let e = engine_from(sizing_cfg(&[
            (
                "half",
                StrategySize {
                    size_weight: Some(dec!(0.5)),
                    ..Default::default()
                },
            ),
            (
                "greedy",
                StrategySize {
                    size_weight: Some(dec!(4)),
                    ..Default::default()
                },
            ),
            (
                "off",
                StrategySize {
                    size_weight: Some(Decimal::ZERO),
                    ..Default::default()
                },
            ),
        ]));
        // half: 10 × 0.5 = 5 → 5/0.5 = 10 shares (global band [2,20]).
        let h = e.effective_sizing("half");
        assert_eq!(h.size_usd, dec!(5));
        assert_eq!(h.size_weight, Some(dec!(0.5)));
        assert!(h.strategy_scoped);
        assert_eq!(e.compute_shares(dec!(0.50), "half"), dec!(10));
        // greedy: 10 × 4 clamps back to the global 10 — the weight cannot
        // widen the budget.
        let g = e.effective_sizing("greedy");
        assert_eq!(g.size_usd, dec!(10));
        // off: weight 0 → no budget at all.
        assert_eq!(e.effective_sizing("off").size_usd, Decimal::ZERO);
        assert_eq!(e.compute_shares(dec!(0.50), "off"), dec!(0));
    }

    #[test]
    fn strategy_sizing_can_never_exceed_the_global_risk_ceiling() {
        // An override that asks for MORE than the global band is clamped: more
        // notional, a higher share cap and a lower floor are all ignored.
        let e = engine_from(sizing_cfg(&[(
            "greedy",
            StrategySize {
                size_usd: Some(dec!(100)),
                min_shares: Some(dec!(0)),
                max_shares: Some(dec!(999)),
                size_weight: None,
                size_pct: None,
            },
        )]));
        let s = e.effective_sizing("greedy");
        assert_eq!(
            s.size_usd,
            dec!(10),
            "notional must be capped at the global budget"
        );
        assert_eq!(
            s.max_shares,
            dec!(20),
            "share ceiling must clamp to the global max"
        );
        assert_eq!(
            s.min_shares,
            dec!(2),
            "floor must not drop below the global min"
        );
        // 100/0.05 would be 2000 shares; the global ceiling still binds.
        assert_eq!(e.compute_shares(dec!(0.05), "greedy"), dec!(20));
    }

    #[test]
    fn strategy_floor_above_the_global_ceiling_still_yields_the_ceiling() {
        // Degenerate config: floor 50 with a global ceiling of 20. The ceiling
        // wins (a floor is never allowed to defeat the global risk cap).
        let e = engine_from(sizing_cfg(&[(
            "weird",
            StrategySize {
                size_usd: None,
                min_shares: Some(dec!(50)),
                max_shares: None,
                size_weight: None,
                size_pct: None,
            },
        )]));
        let s = e.effective_sizing("weird");
        assert_eq!((s.min_shares, s.max_shares), (dec!(20), dec!(20)));
        assert_eq!(e.compute_shares(dec!(0.10), "weird"), dec!(20));
    }

    /// Per-asset market helper (the shared `market()` only makes BTC).
    fn market_of(asset: &str, end_ms: i64) -> CryptoMarket {
        let lower = asset.to_lowercase();
        CryptoMarket {
            asset: asset.into(),
            condition_id: format!("cond_{asset}"),
            question_id: format!("q_{asset}"),
            up_token_id: format!("{lower}_up"),
            down_token_id: format!("{lower}_down"),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: end_ms,
            round_slot: end_ms / 1000 / 900,
            neg_risk: true,
            question: format!("{asset} up or down"),
        }
    }

    #[test]
    fn two_strategies_size_independently_in_one_evaluation() {
        // One cycle, two strategies, one dip each on its OWN asset: every entry
        // carries the strategy's own lot instead of a single global size.
        let mut e = engine_from(sizing_cfg(&[
            (
                "small",
                StrategySize {
                    size_usd: Some(dec!(1)),
                    min_shares: Some(dec!(1)),
                    max_shares: Some(dec!(1)),
                    size_weight: None,
                    size_pct: None,
                },
            ),
            (
                "big",
                StrategySize {
                    size_usd: Some(dec!(10)),
                    min_shares: Some(dec!(20)),
                    max_shares: Some(dec!(20)),
                    size_weight: None,
                    size_pct: None,
                },
            ),
        ]));
        e.register_user_strategy(
            Box::new(DipBuyer::on_assets("small", dec!(0.45), &["BTC"])),
            "test".into(),
        )
        .unwrap();
        e.register_user_strategy(
            Box::new(DipBuyer::on_assets("big", dec!(0.45), &["ETH"])),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("small", true));
        assert!(e.set_strategy_enabled("big", true));
        assert!(
            e.set_strategy_enabled("spread_arb", false),
            "isolate the two sized dips"
        );

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market_of("BTC", 1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        for asset in ["BTC", "ETH"] {
            e.on_data(DataEvent::Book {
                token_id: format!("{}_up", asset.to_lowercase()),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now + 1_000,
            });
        }
        let orders = e.evaluate(now + 2_000);
        assert_eq!(orders.len(), 2, "{orders:?}");
        let small = orders
            .iter()
            .find(|o| o.strategy == "small")
            .expect("small entry");
        let big = orders
            .iter()
            .find(|o| o.strategy == "big")
            .expect("big entry");
        assert_eq!(small.asset, "BTC");
        assert_eq!(big.asset, "ETH");
        assert_eq!(
            small.size,
            dec!(1),
            "1u / 0.44 rounds to 2, capped by its own 1-share lot"
        );
        assert_eq!(big.size, dec!(20));
        assert_eq!(small.price, dec!(0.44), "same dip price for both");
        assert_eq!(big.price, dec!(0.44));
    }

    // ── P-1.1 strategy dispatch ─────────────────────────────────────────────

    /// A trivial hosted strategy: buy either outcome token when its mid dips to
    /// or below a level. Implements the SAME full EngineStrategy contract an
    /// external v2 dylib does (external is only a loading difference).
    struct DipBuyer {
        name: String,
        buy_below: Decimal,
        /// Empty = every market. A non-empty list restricts the strategy to those
        /// assets, which is how one cycle can host several strategies without
        /// them competing for the same token.
        assets: Vec<String>,
        /// Shared entry gates this strategy declares it does not need (E2-b).
        gates: GateExemptions,
    }
    impl DipBuyer {
        fn new(name: &str, buy_below: Decimal) -> Self {
            Self {
                name: name.to_string(),
                buy_below,
                assets: Vec::new(),
                gates: GateExemptions::none(),
            }
        }
        fn on_assets(name: &str, buy_below: Decimal, assets: &[&str]) -> Self {
            Self {
                name: name.to_string(),
                buy_below,
                assets: assets.iter().map(|a| (*a).to_string()).collect(),
                gates: GateExemptions::none(),
            }
        }
        /// Same as [`Self::new`] but declaring gate exemptions and restricted to
        /// the given assets (so it does not compete for the builtin's token under
        /// the one-entry-per-token rule).
        fn exempting(
            name: &str,
            buy_below: Decimal,
            assets: &[&str],
            gates: GateExemptions,
        ) -> Self {
            Self {
                name: name.to_string(),
                buy_below,
                assets: assets.iter().map(|a| (*a).to_string()).collect(),
                gates,
            }
        }
    }
    impl EngineStrategy for DipBuyer {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(
            &mut self,
            _token_id: &str,
            _snap: &crate::model::OrderbookSnapshot,
            _now_ms: i64,
        ) {
        }
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
        fn gate_exemptions(&self) -> GateExemptions {
            self.gates
        }
        fn find_candidates(&mut self, ctx: &StrategyCtx<'_>) -> Vec<TradeSignal> {
            let mut out = Vec::new();
            for market in ctx.markets() {
                if !self.assets.is_empty() && !self.assets.contains(&market.asset) {
                    continue;
                }
                for (token_id, direction) in [
                    (&market.up_token_id, SignalDirection::Up),
                    (&market.down_token_id, SignalDirection::Down),
                ] {
                    let Some(book) = ctx.fresh_book(token_id) else {
                        continue;
                    };
                    if book.mid_price <= self.buy_below {
                        out.push(TradeSignal {
                            strategy: self.name.clone(),
                            asset: market.asset.clone(),
                            direction,
                            token_id: token_id.clone(),
                            condition_id: market.condition_id.clone(),
                            price: book.mid_price,
                            reason: format!("{} dip", self.name),
                            shares: None,
                        });
                    }
                }
            }
            out
        }
    }

    #[test]
    fn a_fresh_engine_registers_nothing_and_only_the_host_can_add_strategies() {
        // PR-B hard switch: the kernel ships ZERO strategies, so a bare
        // `Engine::new` must come up empty. This is the assertion that keeps a
        // privileged in-tree code path from creeping back in.
        let bare = Engine::new(cfg());
        assert!(
            bare.supported_strategies().is_empty(),
            "the kernel must not register any strategy: {:?}",
            bare.supported_strategies()
        );
        assert!(bare.enabled_strategies().is_empty());
        assert_eq!(bare.strategy_source("spread_arb"), None);

        // The test host then registers three adapters in a defined order: it is
        // the tie-break when two strategies want the same token in the same
        // cycle (one entry per token per cycle), and `spread_arb` is first.
        let mut e = engine();
        assert_eq!(
            e.supported_strategies(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string()
            ]
        );
        assert_eq!(
            e.enabled_strategies(),
            vec!["spread_arb".to_string()],
            "the host enables the first adapter explicitly; the rest stay off"
        );
        assert_eq!(e.strategy_source("trend_follow"), Some("test"));
        assert!(
            !e.set_strategy_enabled("nope", true),
            "unknown name must not toggle"
        );
        assert!(e.set_strategy_enabled("trend_follow", true));
        assert_eq!(
            e.enabled_strategies(),
            vec!["spread_arb".to_string(), "trend_follow".to_string()]
        );
    }

    #[test]
    fn the_chase_leg_declares_no_gate_exemption_but_is_evolvable() {
        // E4-a / #30: entering WITH the move is exactly what the shared spot
        // momentum gate wants, so the chase leg opts out of nothing. (E4-b, the
        // counter-trend leg, is the one that needs `momentum: true`.)
        let e = engine();
        let ex = e
            .strategy_gate_exemptions("trend_follow")
            .expect("registered");
        assert_eq!(ex, GateExemptions::none());
        assert!(!ex.any());
        // The only declaration under the default registry is the E4-b fade leg.
        let declared = e.declared_gate_exemptions();
        assert_eq!(declared.len(), 1, "{declared:?}");
        assert_eq!(declared[0].0, "mean_reversion");
        // It IS evolvable, with a coherent declaration and a twin that builds —
        // a declaration without a factory would be inert.
        let s = e
            .strategy_refs()
            .into_iter()
            .find(|s| s.name() == "trend_follow")
            .expect("registered");
        let knobs = s.evolvable_knobs();
        assert_eq!(knobs.len(), 6);
        assert!(knobs.iter().all(|k| k.is_coherent()));
        assert!(s.shadow_factory().is_some());
    }

    /// A rising 1-cent-tick book: `bid` climbs `steps` cents from 0.50, the offer
    /// sits one tick above. Prices live on the real cent grid (mid `x.xx5`), which
    /// matters: a half-cent-offset book would let `round2` collapse the ask onto
    /// the mid and the chase leg refuses to "lift" an offer that is not above mid.
    fn rising_books(e: &mut Engine, token: &str, steps: i64, start_ms: i64) {
        for i in 0..=steps {
            let bid = dec!(0.50) + Decimal::from(i) / Decimal::ONE_HUNDRED;
            e.on_data(DataEvent::Book {
                token_id: token.into(),
                bids: vec![(bid, dec!(100))],
                asks: vec![(bid + dec!(0.01), dec!(100))],
                now_ms: start_ms + i * 1000,
            });
        }
    }

    #[test]
    fn the_chase_leg_trades_a_breakout_the_dip_buyer_would_not() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // A RISING up-token: bid 0.50 → 0.62 (mid 0.505 → 0.625, +24%).
        rising_books(&mut e, "up", 12, now);
        let at = now + 12_000;
        // Disabled → nothing, however good the setup looks.
        assert!(
            e.evaluate(at).is_empty(),
            "the chase leg must not trade while disabled"
        );
        assert!(e.set_strategy_enabled("trend_follow", true));
        let orders = e.evaluate(at);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(orders[0].strategy, "trend_follow");
        assert_eq!(orders[0].token_id, "up");
        assert_eq!(orders[0].direction, "up");
        // It LIFTS the offer: the entry is above the mid, the inverse of the dip
        // buyer's below-mid resting bid.
        let mid = dec!(0.625);
        assert_eq!(orders[0].price, dec!(0.63), "the lifted offer");
        assert!(
            orders[0].price > mid,
            "{} must be above the mid {mid}",
            orders[0].price
        );
        assert!(
            orders[0].internal_key.starts_with("trend_follow:BTC:up:"),
            "{}",
            orders[0].internal_key
        );
        // And it cleared the spot gate without an exemption: with no spot buffer
        // at all the gate is a pass, so a breach here would mean a declaration.
        assert!(
            e.last_exemptions().is_empty(),
            "the chase leg waived nothing"
        );
    }

    #[test]
    fn the_chase_leg_is_blocked_by_the_shared_spot_gate_when_spot_falls() {
        // The gate rejects a bet whose direction fights the spot move. A chase
        // entry is WITH the token's move, but when spot opposes it the candidate
        // must be stopped (it declares no exemption) — proving the new strategy
        // is subject to the same gates as the incumbent.
        let mut e = engine();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: 1_000_000,
        });
        assert!(e.set_strategy_enabled("trend_follow", true));
        // Spot falls: the momentum filter will refuse an UP bet.
        for i in 0..10 {
            e.on_data(DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(60000) - Decimal::from(i) * dec!(10),
                now_ms: 1_000_000 + i * 1000,
            });
        }
        rising_books(&mut e, "up", 12, 1_000_000);
        let orders = e.evaluate(1_000_000 + 12_000);
        assert!(orders.is_empty(), "spot moved against the bet: {orders:?}");
        assert!(
            e.last_blocked()
                .iter()
                .any(|b| b.strategy == "trend_follow" && b.reason == BlockReason::Momentum),
            "{:?}",
            e.last_blocked()
        );
    }

    #[test]
    fn the_two_builtins_do_not_starve_each_other() {
        // Both strategies enabled, both presented with the setups they want on
        // DIFFERENT tokens: each gets its own entry in the same cycle, each
        // tagged with its own name, and neither is dropped.
        let mut e = engine();
        assert!(e.set_strategy_enabled("trend_follow", true));
        let now = 1_000_000i64;
        let mut m = market(1_800_000);
        m.down_token_id = "down".into();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![m.clone()],
            now_ms: now,
        });

        // "up" climbs (chase), "down" holds then dips (dip buy).
        for i in 0..=12 {
            let up_bid = dec!(0.50) + Decimal::from(i) / Decimal::ONE_HUNDRED;
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(up_bid, dec!(100))],
                asks: vec![(up_bid + dec!(0.01), dec!(100))],
                now_ms: now + i * 1000,
            });
            e.on_data(DataEvent::Book {
                token_id: "down".into(),
                bids: vec![(dec!(0.61), dec!(100))],
                asks: vec![(dec!(0.63), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        // Dip on "down": mid 0.44 → the dip buyer's resting bid is 0.43, inside its
        // 0.45 ceiling, while the chase leg wants the OTHER token and is unaffected.
        e.on_data(DataEvent::Book {
            token_id: "down".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        let orders = e.evaluate(now + 12_000);
        assert_eq!(
            orders.len(),
            2,
            "one entry per strategy per token: {orders:?}"
        );
        assert!(
            orders
                .iter()
                .any(|o| o.strategy == "trend_follow" && o.token_id == "up"),
            "{orders:?}"
        );
        assert!(
            orders
                .iter()
                .any(|o| o.strategy == "spread_arb" && o.token_id == "down"),
            "{orders:?}"
        );
        // Neither strategy was blocked out of existence.
        assert!(e.last_blocked().is_empty(), "{:?}", e.last_blocked());
    }

    #[test]
    fn a_shared_token_is_settled_by_registration_order_not_by_starvation() {
        // When both want the SAME token in the SAME cycle the kernel emits one
        // entry (one entry per token per cycle) and the registered-first strategy
        // wins. This is the documented, deterministic tie-break — not a race.
        //
        // The default engine config cannot produce this collision at all: the dip
        // buyer's ceiling is 0.45 while the chase leg only looks above 0.55. So the
        // collision is constructed deliberately by raising the dip buyer's ceiling,
        // which is exactly the configuration where the tie-break has to be decided.
        let mut c = cfg();
        c.spread_arb.trend_max_entry_price = dec!(0.70);
        let mut e = engine_from(c);
        assert!(e.set_strategy_enabled("trend_follow", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // A book both want: "up" rising on the cent grid (the chase leg lifts the
        // 0.63 offer) while the mid 0.625 is still inside the dip buyer's raised
        // band (it would rest 0.61, below the mid).
        rising_books(&mut e, "up", 12, now);
        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "one entry per token per cycle: {orders:?}");
        assert_eq!(
            orders[0].strategy, "spread_arb",
            "the incumbent holds the tie-break"
        );
        assert!(
            orders[0].price < dec!(0.625),
            "and it rests below the mid: {}",
            orders[0].price
        );
    }

    #[test]
    fn user_strategy_registers_disabled_and_is_tagged_once_enabled() {
        let mut e = engine();
        let name = e
            .register_user_strategy(
                Box::new(DipBuyer::new("dip_buyer", dec!(0.6))),
                "test".into(),
            )
            .unwrap();
        assert_eq!(name, "dip_buyer");
        assert_eq!(
            e.supported_strategies(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string(),
                "dip_buyer".to_string(),
            ]
        );
        assert_eq!(
            e.enabled_strategies(),
            vec!["spread_arb".to_string()],
            "starts disabled"
        );
        assert_eq!(e.strategy_source("dip_buyer"), Some("test"));
        // Duplicate names are rejected outright.
        assert!(
            e.register_user_strategy(
                Box::new(DipBuyer::new("dip_buyer", dec!(0.6))),
                "test".into()
            )
            .is_err()
        );

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
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
        assert!(
            orders[0].internal_key.starts_with("dip_buyer:BTC:"),
            "{}",
            orders[0].internal_key
        );
    }

    /// Issue #205: the freshness budget is the entry gate, so the operator's value
    /// has to be the one that decides. One book, one decision time, several
    /// configs: refused at the compiled default, accepted *unchanged* once the
    /// budget is widened past the book's age. The book is deliberately older than
    /// any plausible default and well inside the knob's ceiling, so this is the
    /// exact case an operator reaching for the flag cares about.
    #[test]
    fn the_configured_staleness_budget_decides_whether_a_book_can_trade() {
        let now = 1_000_000i64;
        let book_ms = now - 30_000; // 30s old at decision time

        let entries = |budget_ms: i64| -> Vec<crate::model::OrderRequest> {
            let mut e = engine_from(EngineConfig {
                max_orderbook_stale_ms: budget_ms,
                ..cfg()
            });
            e.register_user_strategy(
                Box::new(DipBuyer::new("dip_buyer", dec!(0.6))),
                "test".into(),
            )
            .unwrap();
            assert!(e.set_strategy_enabled("dip_buyer", true));
            e.on_data(DataEvent::RoundMarkets {
                markets: vec![market(1_800_000)],
                now_ms: now,
            });
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: book_ms,
            });
            e.evaluate(now)
        };

        // Default budget → the 30s-old book is stale, and a stale book is not a
        // priceable book: the strategy never sees it, so nothing is placed.
        let default_budget = entries(DEFAULT_MAX_ORDERBOOK_STALE_MS);
        assert!(
            default_budget.is_empty(),
            "a {}-ms-old book must not trade under the {DEFAULT_MAX_ORDERBOOK_STALE_MS}ms default: {default_budget:?}",
            now - book_ms
        );
        // Widened budget → the very same book is fresh enough and trades.
        let wide = entries(60_000);
        assert_eq!(
            wide.len(),
            1,
            "a 60s budget must accept the same book: {wide:?}"
        );
        assert_eq!(wide[0].strategy, "dip_buyer");
        assert_eq!(wide[0].asset, "BTC");
        // `0` is the operator's explicit "check OFF": accepted however old.
        assert_eq!(
            entries(0).len(),
            1,
            "0 disables the freshness check, so the book is accepted"
        );
        // The age is the deciding variable, and it sits strictly between the two
        // budgets — otherwise this test would pass for the wrong reason.
        assert!(now - book_ms > DEFAULT_MAX_ORDERBOOK_STALE_MS);
        assert!(now - book_ms < 60_000);
    }

    /// The boundary of the freshness rule, at the one place that enforces it for
    /// entries: an age equal to the budget is still fresh, one millisecond past it
    /// is not, and `0` disables the age test without accepting an EMPTY book
    /// (there would be nothing to price off).
    #[test]
    fn the_freshness_budget_boundary_and_its_off_switch() {
        let now = 1_000_000i64;
        let mut books = HashMap::new();
        let mut b = LocalBook::new();
        b.apply_snapshot(
            &[(dec!(0.55), dec!(100))],
            &[(dec!(0.57), dec!(100))],
            now - 8_000,
        );
        books.insert("up".to_string(), b);

        assert!(
            fresh_book(&books, "up", now, 8_000).is_some(),
            "an age exactly at the budget is still fresh"
        );
        assert!(
            fresh_book(&books, "up", now, 7_999).is_none(),
            "one millisecond past the budget is stale"
        );
        assert!(
            fresh_book(&books, "up", now, 0).is_some(),
            "0 disables the age test"
        );
        assert!(
            fresh_book(&books, "up", now, -1).is_some(),
            "any non-positive value is the OFF switch"
        );

        // OFF is not "accept anything": an empty book is still unpricable.
        let mut empty = LocalBook::new();
        empty.apply_snapshot(&[], &[], now);
        books.insert("flat".to_string(), empty);
        assert!(fresh_book(&books, "flat", now, 0).is_none());
        assert!(fresh_book(&books, "flat", now, 8_000).is_none());
        // An unknown token has no book at all.
        assert!(fresh_book(&books, "nope", now, 0).is_none());
    }

    #[test]
    fn two_strategies_run_side_by_side_with_separate_tags() {
        let mut e = engine();
        e.register_user_strategy(
            Box::new(DipBuyer::new("dip_buyer", dec!(0.45))),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("dip_buyer", true));

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        // Confirm the UP trend over 12 samples (spread_arb needs it).
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        // UP dips → spread_arb candidate (and dip_buyer's too, on the same token).
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        // DOWN is cheap → only dip_buyer signals there (no trend confirmation).
        e.on_data(DataEvent::Book {
            token_id: "down".into(),
            bids: vec![(dec!(0.41), dec!(100))],
            asks: vec![(dec!(0.43), dec!(100))],
            now_ms: now + 12_000,
        });

        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 2, "one entry per token per cycle: {orders:?}");
        assert!(
            orders
                .iter()
                .any(|o| o.token_id == "up" && o.strategy == "spread_arb"),
            "{orders:?}"
        );
        assert!(
            orders
                .iter()
                .any(|o| o.token_id == "down" && o.strategy == "dip_buyer"),
            "{orders:?}"
        );
    }

    // ── E2-b per-strategy gate opt-out (#27) ────────────────────────────────

    /// Book a trend-confirming run on `token`, then dip it: the setup the
    /// builtin `spread_arb` needs to produce a candidate.
    fn trend_then_dip(e: &mut Engine, token: &str, now: i64) {
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: token.into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        e.on_data(DataEvent::Book {
            token_id: token.into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
    }

    /// A single dip book on `token` at `at_ms`, with no trend run — the builtin
    /// stays idle, so only a strategy that needs no trend confirmation can
    /// signal here. Book it at the instant you evaluate: a stale book is not
    /// priceable at all (`fresh_book` returns None).
    fn dip_only(e: &mut Engine, token: &str, at_ms: i64) {
        e.on_data(DataEvent::Book {
            token_id: token.into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: at_ms,
        });
    }

    /// Push a spot price that fell over the momentum window (against an UP bet).
    fn spot_falls(e: &mut Engine, asset: &str, from: Decimal, to: Decimal, now: i64) {
        e.on_data(DataEvent::Spot {
            asset: asset.into(),
            price: from,
            now_ms: now,
        });
        e.on_data(DataEvent::Spot {
            asset: asset.into(),
            price: to,
            now_ms: now + 2_000,
        });
    }

    #[test]
    fn declared_timing_exemption_lets_a_candidate_through_the_window_gate() {
        // Timing gate forced shut by an absurd min_round_age; a strategy that
        // declares the timing window unnecessary still enters. The exempting
        // strategy trades ETH so it does not compete for the builtin's BTC token
        // (one entry per token per cycle is NOT waivable).
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "window_fade",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("window_fade", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        dip_only(&mut e, "eth_up", now + 12_000);

        // The builtin is still gated; the declaring strategy is exempt.
        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(orders[0].strategy, "window_fade");
        assert_eq!(orders[0].asset, "ETH");
        assert!(
            e.last_blocked().iter().any(|b| b.strategy == "spread_arb"),
            "builtin stays gated"
        );
        assert!(
            !e.last_blocked().iter().any(|b| b.strategy == "window_fade"),
            "exempted strategy must not appear as blocked"
        );
        // Auditable: the honoured exemption names the strategy, the gate and the
        // exact block that was waived.
        let ex = e.last_exemptions();
        assert_eq!(ex.len(), 1, "{ex:?}");
        assert_eq!(ex[0].strategy, "window_fade");
        assert_eq!(ex[0].gate, "timing");
        assert!(ex[0].detail.contains("Round too young"), "{}", ex[0].detail);
        let line = ex[0].audit_line();
        assert!(
            line.contains("本单因策略 window_fade 豁免门禁 timing"),
            "{line}"
        );
    }

    #[test]
    fn a_strategy_that_declares_nothing_is_still_gated() {
        // Identical setup, identical strategy body — only the declaration differs.
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::on_assets("plain", dec!(0.45), &["ETH"])),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("plain", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        dip_only(&mut e, "eth_up", now + 12_000);

        assert!(
            e.evaluate(now + 12_000).is_empty(),
            "undeclared strategy must stay gated"
        );
        assert!(
            e.last_exemptions().is_empty(),
            "nothing may be recorded as exempted"
        );
        assert!(
            e.last_blocked().iter().any(|b| b.strategy == "plain"),
            "{:?}",
            e.last_blocked()
        );
    }

    #[test]
    fn round_markets_declared_expiry_beats_the_wall_clock_grid() {
        // The venue's declared round end (expires_at_ms) must drive
        // `time_left_sec` — not the host's wall-clock slot grid. Here the grid
        // would put the round end at 1_800_000 (800s left at 1_010_000), while
        // the venue declares 1_900_000 (890s left). Without the wiring the
        // D-31 floor flaked on wall-clock position (the strategy-gate-check
        // CI failure); with it the clock follows the venue declaration.
        let mut e = engine();
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market_of("BTC", 1_900_000)],
            now_ms: 1_000_000,
        });
        assert_eq!(
            e.scanner().round_state(1_010_000).time_left_sec,
            890,
            "the declared expiry, not the slot grid, is authoritative"
        );
    }

    #[test]
    fn d31_declared_floor_stops_the_timing_exemption_waiving_the_time_left_gate() {
        // D-31: `timing: true` used to waive the time-left gate outright, so an
        // exempt strategy could enter with the round nearly over — where the exit
        // policy fires on the next tick and the trade is a guaranteed zero-hold.
        // A declared floor excludes exactly that window. The GLOBAL gate is the
        // ordinary 180s (that is what actually produces the block); the two
        // strategies differ ONLY in the floor they declare, which is the point.
        let mut late_cfg = cfg();
        late_cfg.scanner.min_round_age_sec = 0;
        late_cfg.scanner.min_time_left_sec = 180;
        let mut e = engine_from(late_cfg);
        // ETH: declares a 180s floor → must NOT be waived at 172s left.
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "floored",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: Some(180),
                },
            )),
            "test".into(),
        )
        .unwrap();
        // SOL: declares the SAME `timing` exemption but a lower floor, so its
        // opt-out still reaches into the last 100s. Keeping both in one cycle is
        // the point: the declared floor, and only it, separates them.
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "lowfloor",
                dec!(0.45),
                &["SOL"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: Some(100),
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("floored", true));
        assert!(e.set_strategy_enabled("lowfloor", true));
        // XRP: declares `timing` with NO floor. That must mean "use the kernel's
        // 180s", not "no floor at all" — i.e. it is blocked exactly like `floored`.
        // This is the backwards-compatibility guarantee a pre-D-31 library relies
        // on when it emits only the two booleans.
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "intestate",
                dec!(0.45),
                &["XRP"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("intestate", true));

        // Round expires at 1_800_000 → exactly 172s left at `at`.
        let at = 1_800_000 - 172_000;
        let now = at - 12_000;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![
                market(1_800_000),
                market_of("ETH", 1_800_000),
                market_of("SOL", 1_800_000),
                market_of("XRP", 1_800_000),
            ],
            now_ms: now,
        });
        dip_only(&mut e, "eth_up", at);
        dip_only(&mut e, "sol_up", at);
        dip_only(&mut e, "xrp_up", at);

        let orders = e.evaluate(at);
        assert_eq!(
            e.scanner().round_state(at).time_left_sec,
            172,
            "fixture drift"
        );
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(
            orders[0].strategy, "lowfloor",
            "only the lower declared floor may still enter this late"
        );
        assert!(
            e.last_exemptions().iter().all(|x| x.strategy == "lowfloor"),
            "only the lower declared floor may be recorded as exempted: {:?}",
            e.last_exemptions()
        );
        assert!(
            e.last_blocked()
                .iter()
                .any(|b| b.strategy == "floored" && b.reason == BlockReason::Timing),
            "the floored strategy must be timing-blocked: {:?}",
            e.last_blocked()
        );
        assert!(
            e.last_blocked()
                .iter()
                .any(|b| b.strategy == "intestate" && b.reason == BlockReason::Timing),
            "an undeclared floor means the kernel's, not none: {:?}",
            e.last_blocked()
        );
        assert!(
            e.last_exemptions().iter().all(|x| x.strategy != "floored"),
            "nothing may be recorded as exempted for the floored strategy"
        );

        // The floor narrows the exemption; below it, nothing about the young-round
        // case changed. Re-run the same strategy against a "too young" block: its
        // `time_left_sec` is large, so the 180s floor never conflicts.
        let mut young_cfg = cfg();
        young_cfg.scanner.min_round_age_sec = 10_000;
        young_cfg.scanner.min_time_left_sec = 180;
        let mut e = engine_from(young_cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "floored",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: true,
                    momentum: false,
                    timing_min_time_left_sec: Some(180),
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("floored", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        dip_only(&mut e, "eth_up", now + 12_000);
        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "too-young must stay waivable: {orders:?}");
        assert_eq!(orders[0].strategy, "floored");
        let ex = e.last_exemptions();
        assert_eq!(ex.len(), 1, "{ex:?}");
        assert!(
            ex[0].detail.contains("Round too young"),
            "the honoured exemption is the young-round one: {}",
            ex[0].detail
        );
    }

    #[test]
    fn d31_floor_is_read_through_the_abi_json_and_clamped_at_zero() {
        // The floor crosses the ABI as an optional JSON key, so it must survive a
        // round trip, degrade to the kernel default when absent, and clamp a
        // negative declaration (which would restore the unbounded waiver).
        let declared = serde_json::json!({
            "timing": true,
            "momentum": false,
            "timing_min_time_left_sec": 180,
        });
        let g = GateExemptions::from_json(&declared);
        assert!(g.timing && !g.momentum);
        assert_eq!(g.timing_min_time_left_sec, Some(180));
        assert_eq!(g.timing_floor_sec(0), 180);
        assert_eq!(g.to_json(), declared, "the declaration round-trips");

        // A pre-D-31 library emits only the two booleans: the missing key must
        // mean "use the kernel default", never "no floor".
        let legacy = GateExemptions::from_json(&serde_json::json!({
            "timing": true,
            "momentum": false,
        }));
        assert_eq!(legacy.timing_min_time_left_sec, None);
        assert_eq!(legacy.timing_floor_sec(180), 180);
        assert_eq!(
            legacy.to_json(),
            serde_json::json!({ "timing": true, "momentum": false }),
            "an undeclared floor must not be written back"
        );

        // Malformed / negative declarations degrade inward, never outward.
        let bad = GateExemptions::from_json(&serde_json::json!({
            "timing": true,
            "timing_min_time_left_sec": "180",
        }));
        assert_eq!(
            bad.timing_min_time_left_sec, None,
            "a string is not a floor"
        );
        assert_eq!(
            GateExemptions::from_json(&serde_json::json!({
                "timing": true,
                "timing_min_time_left_sec": -60,
            }))
            .timing_floor_sec(0),
            0,
            "a negative floor clamps to zero"
        );
    }

    #[test]
    fn declared_momentum_exemption_lets_a_mean_reversion_entry_through() {
        let mut e = engine();
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "fader",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: false,
                    momentum: true,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("fader", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        dip_only(&mut e, "eth_up", now + 12_000);
        // Both spots fall over the momentum window: exactly the case a
        // mean-reversion entry wants (and the builtin filter rejects).
        spot_falls(&mut e, "BTC", dec!(60000), dec!(59000), now + 1000);
        spot_falls(&mut e, "ETH", dec!(3000), dec!(2950), now + 1000);

        let orders = e.evaluate(now + 12_000);
        assert_eq!(orders.len(), 1, "{orders:?}");
        assert_eq!(orders[0].strategy, "fader");
        assert_eq!(orders[0].asset, "ETH");
        let ex = e.last_exemptions();
        assert_eq!(ex.len(), 1, "{ex:?}");
        assert_eq!(ex[0].gate, "momentum");
        assert!(ex[0].detail.contains("spot ETH"), "{}", ex[0].detail);
        assert!(ex[0].detail.contains("vs up"), "{}", ex[0].detail);
        // The builtin, declaring nothing, was blocked by the very same filter.
        assert!(
            e.last_blocked()
                .iter()
                .any(|b| b.strategy == "spread_arb" && b.reason == BlockReason::Momentum)
        );
    }

    #[test]
    fn an_exemption_is_scoped_to_the_declaring_strategy_only() {
        // Two strategies on different assets in the SAME cycle: only the one that
        // declared the momentum exemption gets through.
        let mut e = engine();
        assert!(e.set_strategy_enabled("spread_arb", false));
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "fader",
                dec!(0.45),
                &["BTC"],
                GateExemptions {
                    timing: false,
                    momentum: true,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("fader", true));
        // A second, undeclared strategy restricted to ETH.
        e.register_user_strategy(
            Box::new(DipBuyer::on_assets("gated", dec!(0.45), &["ETH"])),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("gated", true));

        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        dip_only(&mut e, "up", now + 2_000);
        dip_only(&mut e, "eth_up", now + 2_000);
        spot_falls(&mut e, "BTC", dec!(60000), dec!(59000), now);
        spot_falls(&mut e, "ETH", dec!(3000), dec!(2950), now);

        let orders = e.evaluate(now + 2_000);
        assert_eq!(
            orders.len(),
            1,
            "only the declaring strategy may pass: {orders:?}"
        );
        assert_eq!(orders[0].strategy, "fader");
        assert_eq!(orders[0].asset, "BTC");
        assert_eq!(e.last_exemptions().len(), 1);
        assert!(e.last_blocked().iter().any(|b| b.strategy == "gated"
            && b.asset == "ETH"
            && b.reason == BlockReason::Momentum));
    }

    #[test]
    fn the_no_market_precondition_is_never_exemptible() {
        // Declaring every gate must not conjure a market: with no round markets
        // there is no priceable token, so nothing is exempted.
        let mut e = engine();
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "all_in",
                dec!(0.99),
                &[],
                GateExemptions::all(),
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("all_in", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now,
        });
        assert!(
            e.evaluate(now).is_empty(),
            "no market → no candidates at all"
        );
        assert!(
            e.last_exemptions().is_empty(),
            "a structural precondition is not exemptible"
        );
    }

    #[test]
    fn blocked_candidates_carry_their_owning_strategy() {
        // Per-strategy attribution of blocked.timing / blocked.momentum (#27).
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        assert!(e.evaluate(now + 12_000).is_empty());
        assert_eq!(e.last_blocked().len(), 1);
        assert_eq!(e.last_blocked()[0].strategy, "spread_arb");

        e.tally_blocked();
        assert_eq!(e.gate_tally("spread_arb").blocked_timing, 1);
        assert_eq!(e.gate_tally("spread_arb").exempted_timing, 0);
        assert_eq!(e.gate_tally("nobody"), StrategyGateTally::default());
    }

    #[test]
    fn honoured_exemptions_are_counted_per_strategy_and_drained_once() {
        let mut cfg = cfg();
        cfg.scanner.min_round_age_sec = 10_000;
        let mut e = engine_from(cfg);
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "window_fade",
                dec!(0.45),
                &["ETH"],
                GateExemptions {
                    timing: true,
                    momentum: true,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        assert!(e.set_strategy_enabled("window_fade", true));
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000), market_of("ETH", 1_800_000)],
            now_ms: now,
        });
        trend_then_dip(&mut e, "up", now);
        dip_only(&mut e, "eth_up", now + 12_000);
        assert_eq!(e.evaluate(now + 12_000).len(), 1);
        e.tally_blocked();

        let drained = e.take_exemptions();
        assert_eq!(drained.len(), 1);
        assert!(e.take_exemptions().is_empty(), "drained once");
        let t = e.gate_tally("window_fade");
        assert_eq!(t.exempted_timing, 1);
        assert_eq!(t.exempted_momentum, 0);
        // Counting an exemption never inflates the blocked counters: the builtin
        // was blocked by the very gate the other strategy declared unnecessary.
        assert_eq!(e.blocked_timing_count(), 1, "only the builtin was blocked");
        assert_eq!(e.gate_tally("spread_arb").blocked_timing, 1);
        assert_eq!(e.gate_tally("window_fade").blocked_timing, 0);
    }

    #[test]
    fn declared_exemptions_are_listed_for_audit() {
        let mut e = engine();
        e.register_user_strategy(
            Box::new(DipBuyer::exempting(
                "fader",
                dec!(0.45),
                &[],
                GateExemptions {
                    timing: false,
                    momentum: true,
                    timing_min_time_left_sec: None,
                },
            )),
            "test".into(),
        )
        .unwrap();
        e.register_user_strategy(Box::new(DipBuyer::new("plain", dec!(0.45))), "test".into())
            .unwrap();
        let declared = e.declared_gate_exemptions();
        assert_eq!(
            declared.len(),
            2,
            "silent strategies are omitted: {declared:?}"
        );
        assert_eq!(
            declared[0].0, "mean_reversion",
            "the builtin declares first (hosted first)"
        );
        assert_eq!(declared[0].1.gates(), vec!["momentum"]);
        assert_eq!(declared[1].0, "fader");
        assert_eq!(declared[1].1.gates(), vec!["momentum"]);
        assert_eq!(
            e.strategy_gate_exemptions("plain"),
            Some(GateExemptions::none())
        );
        assert_eq!(e.strategy_gate_exemptions("nobody"), None);
    }

    #[test]
    fn the_builtin_declares_no_exemptions() {
        let e = engine();
        assert_eq!(
            e.strategy_gate_exemptions("spread_arb"),
            Some(GateExemptions::none())
        );
    }

    #[test]
    fn disabling_spread_arb_stops_its_entries() {
        let mut e = engine();
        let now = 1_000_000i64;
        e.on_data(DataEvent::RoundMarkets {
            markets: vec![market(1_800_000)],
            now_ms: now,
        });
        for i in 0..12 {
            e.on_data(DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.55), dec!(100))],
                asks: vec![(dec!(0.57), dec!(100))],
                now_ms: now + i * 1000,
            });
        }
        e.on_data(DataEvent::Book {
            token_id: "up".into(),
            bids: vec![(dec!(0.43), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
            now_ms: now + 12_000,
        });
        assert_eq!(e.evaluate(now + 12_000).len(), 1);
        assert!(e.set_strategy_enabled("spread_arb", false));
        // Same round, no order noted as pending: the empty result must come from
        // the disable, not from suppression.
        assert!(
            e.evaluate(now + 12_001).is_empty(),
            "disabled strategy must not emit"
        );
        assert!(e.set_strategy_enabled("spread_arb", true));
        assert_eq!(
            e.evaluate(now + 12_002).len(),
            1,
            "re-enabling restores entries"
        );
    }
}
