//! Event-driven backtester (P-1.2): replay a recorded market-data stream through
//! the SAME core the live path runs.
//!
//! There is no separate simulation engine here. `EventBacktester` owns a real
//! [`Core`] in [`Mode::Dry`] and drives the full live chain — 行情 → 信号 →
//! 风控 → 账本 → OME → 出场 — on a virtual clock:
//!
//! 1. maintenance runs on its own `tick_ms` schedule — the live loop's timer,
//!    independent of event density;
//! 2. at each fire the core runs `tick` (escalations + exits) and
//!    `engine_evaluate` (signal → order), exactly like `ipc::server`'s interval;
//! 3. events pulled from the [`DataSource`] are delivered at their own
//!    timestamps through the same `engine_on_data` choke point a live feed uses.
//!
//! Because the replay drives that one path, the fill model
//! ([`crate::sim::FillModel`]) is the only difference between a faithful replay
//! (identity model) and a stress run (slippage / latency / fill probability).
//!
//! Acceptance for the live/backtest pair is *identical* results when the same
//! events are replayed with the identity model: the archive written by a live
//! run records exactly the events (and their timestamps) the engine consumed, so
//! a replay is a re-run of the same decisions, not an approximation.

use crate::data_source::{DataSource, SourceStats, TimedEvent};
use crate::ipc::schema::Event;
use crate::service::{Core, CoreConfig};
use crate::sim::FillModel;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc;

/// A `Backtester` replays a data source and reports what the strategy did.
pub trait Backtester {
    /// Run the full replay. Errors are returned only for setup failures; runtime
    /// problems are collected into the report (`errors`/`riskAlerts`).
    fn run(&mut self) -> Result<BacktestReport, String>;
    /// Provenance line for logs (source + model).
    fn describe(&self) -> String;
}

/// How to run a replay.
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    /// Base core config (strategy knobs: assets, sizing, gates, exit policy,
    /// per-strategy limits). The replay forces the safe subset — see
    /// [`EventBacktester::new`].
    pub core: CoreConfig,
    /// Virtual-clock step between maintenance cycles. 50 ms matches the live
    /// server loop, so signals/exits are evaluated at the same cadence.
    pub tick_ms: i64,
    /// After the last event, keep the clock running this long (ms) so pending
    /// maker escalations and exits fire. 0 = stop at the last event.
    pub tail_ms: i64,
    /// Counterfactual knob overrides for a replay: `(strategy, knob, value)`.
    ///
    /// Delivered through the SAME hot-param registry Shadow Evolution uses, so a
    /// counterfactual run differs from the shipped behaviour only in the value
    /// handed over — same declared knobs, same cell, same `on_hot_params` push on
    /// the hot path. Empty (the default) replays the config in force. A knob the
    /// strategy does not declare is simply not applied by it (the declaration is
    /// the strategy's own), exactly as a proposal would be.
    pub hot_params: Vec<(String, String, Decimal)>,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            core: CoreConfig::default(),
            tick_ms: 50,
            tail_ms: 0,
            hot_params: Vec::new(),
        }
    }
}

/// An in-memory [`DataSource`]: used by tests and by callers that already hold a
/// parsed stream (e.g. a trimmed slice of an archive).
pub struct VecSource {
    events: std::vec::IntoIter<TimedEvent>,
    label: String,
    total: u64,
}

impl VecSource {
    pub fn new(events: Vec<TimedEvent>) -> Self {
        let total = events.len() as u64;
        Self {
            events: events.into_iter(),
            label: format!("memory ({total} events)"),
            total,
        }
    }
}

impl DataSource for VecSource {
    fn next_event(&mut self) -> Option<TimedEvent> {
        self.events.next()
    }
    fn describe(&self) -> String {
        self.label.clone()
    }
    fn stats(&self) -> SourceStats {
        SourceStats {
            events: self.total,
            ..Default::default()
        }
    }
}

/// One realised trade as reported (mirrors the `PositionClosed` event).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeLine {
    pub id: String,
    pub asset: String,
    pub direction: String,
    pub reason: String,
    /// The position's identity, mirroring `PositionClosed`: `strategy`
    /// attributes the trade, and `conditionId` groups the legs of ONE trade
    /// that closed as several positions. A complete-set pair is exactly that —
    /// it settles as two (winner $1 / loser $0), and neither leg's own P&L says
    /// anything on its own: the trade's statistics exist only as the group.
    pub strategy: String,
    pub token_id: String,
    pub condition_id: String,
    #[serde(with = "crate::decimal")]
    pub net_pnl_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_pct: Decimal,
}

/// Aggregate trade statistics over the realized equity curve.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeStats {
    pub closed: u64,
    pub wins: u64,
    pub losses: u64,
    /// Trades that closed exactly flat (neither win nor loss).
    pub flat: u64,
    #[serde(with = "crate::decimal")]
    pub gross_profit_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub gross_loss_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub net_pnl_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub avg_pnl_usd: Decimal,
    #[serde(with = "crate::decimal")]
    pub win_rate_pct: Decimal,
    /// gross profit / |gross loss|; `null` when there were no losing trades.
    #[serde(with = "crate::decimal::opt", skip_serializing_if = "Option::is_none")]
    pub profit_factor: Option<Decimal>,
    /// Realized peak-to-trough decline (open positions are not marked here).
    #[serde(with = "crate::decimal")]
    pub max_drawdown_usd: Decimal,
    /// Max drawdown as a percentage of the equity peak that preceded it.
    #[serde(with = "crate::decimal")]
    pub max_drawdown_pct: Decimal,
    /// Fees summed from the per-strategy accounting.
    #[serde(with = "crate::decimal")]
    pub fees_usd: Decimal,
}

/// The taker-fee schedule a replay charged, as reported (#203).
///
/// Part of the report rather than a CLI transcript detail: a fee-sensitive
/// conclusion ("this strategy is still profitable") is only interpretable
/// against the schedule it was measured under, and the fee is exactly the input
/// that moved the strategy's sign. `source` travels with it so a reader of the
/// JSON — a reviewer, a later gate — can see whether the parameters are
/// published or inherited from this repository's history.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeScheduleReport {
    pub name: String,
    #[serde(with = "crate::decimal")]
    pub rate: Decimal,
    pub exponent: u32,
    pub source: String,
}

impl From<crate::exit_policy::FeeSchedule> for FeeScheduleReport {
    fn from(s: crate::exit_policy::FeeSchedule) -> Self {
        Self {
            name: s.name.to_string(),
            rate: s.rate,
            exponent: s.exponent,
            source: s.source.to_string(),
        }
    }
}

/// Order counts by the last status each order reached.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderCounts {
    pub orders: u64,
    pub filled: u64,
    pub cancelled: u64,
    pub rejected: u64,
    pub failed: u64,
    pub live_at_end: u64,
}

/// How much of what we asked for actually traded (issue #183).
///
/// A dry run used to fill every crossing maker whole, so these numbers could
/// only ever read 100% and a backtest could not tell a book deep enough for the
/// strategy from one that would have left the entry half-open. They are now a
/// property of the run rather than an assumption.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FillStats {
    /// Orders observed at least once, in any status.
    pub orders: u64,
    /// Orders whose last observed state was fully filled.
    pub filled_orders: u64,
    /// Orders that traded but never reached their size — the partial fills.
    pub partial_orders: u64,
    /// Orders that never traded a single share (cancelled, rejected, expired…).
    pub unfilled_orders: u64,
    /// Share of orders that traded at all. `0` when no order was placed.
    #[serde(with = "crate::decimal")]
    pub fill_rate_pct: Decimal,
    /// Share of orders left PARTIALLY filled — the dry run's exposure to the
    /// live-bug-② shape (#183).
    #[serde(with = "crate::decimal")]
    pub partial_rate_pct: Decimal,
    /// Filled size over requested size across every order: the volume view.
    #[serde(with = "crate::decimal")]
    pub size_fill_ratio_pct: Decimal,
}

/// The replay result. Serializable so a run can be filed next to the archive.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestReport {
    pub source: String,
    pub source_stats: SourceStats,
    pub tick_ms: i64,
    pub tail_ms: i64,
    pub start_at_ms: i64,
    pub end_at_ms: i64,
    pub virtual_ms: i64,
    pub fill_model: FillModel,
    /// The taker-fee schedule this replay charged (#203). See [`FeeScheduleReport`].
    pub fee_schedule: FeeScheduleReport,
    pub entry_maker_timeout_ms: i64,
    pub orders: OrderCounts,
    /// Fill rate / partial-fill rate (#183): what actually traded, order by
    /// order, so a dry acceptance run reports its own optimism instead of
    /// hiding it behind a 100% assumption.
    pub fill_stats: FillStats,
    pub fills: u64,
    pub trades: TradeStats,
    /// Per-strategy accounting (`engine.stats.strategies[]`), verbatim.
    pub strategies: Vec<Value>,
    /// Feed counters (`engine.stats`): books/tops/spots/rounds/evaluations/…
    pub feed: Value,
    pub blocked: Value,
    pub open_positions: usize,
    #[serde(with = "crate::decimal")]
    pub open_notional_usd: Decimal,
    pub risk_alerts: Vec<String>,
    pub errors: Vec<String>,
    pub trade_lines: Vec<TradeLine>,
    /// Trades beyond the `trade_lines` cap (list truncated, counts not).
    pub trade_lines_truncated: u64,
    /// True when the replay had to force `Mode::Dry` (it always does).
    pub forced_dry: bool,
}

impl BacktestReport {
    /// Human-readable rendering for the CLI.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("backtest: {}\n", self.source));
        s.push_str(&format!(
            "  window           : {} → {} ({} ms virtual)\n",
            self.start_at_ms, self.end_at_ms, self.virtual_ms
        ));
        s.push_str(&format!(
            "  events           : {} ({} malformed skipped, {} out-of-order)\n",
            self.source_stats.events,
            self.source_stats.malformed_lines,
            self.source_stats.out_of_order_events
        ));
        s.push_str(&format!(
            "  fill model       : slippage {} tick(s), maker latency {} ms, maker fill {} bps\n",
            self.fill_model.taker_slippage_ticks,
            self.fill_model.maker_latency_ms,
            self.fill_model.maker_fill_prob_bps
        ));
        s.push_str(&format!(
            "  entry escalate   : {} ms\n",
            self.entry_maker_timeout_ms
        ));
        s.push_str(&format!(
            "  taker fee        : {} rate={} exponent={} (#203)\n",
            self.fee_schedule.name, self.fee_schedule.rate, self.fee_schedule.exponent
        ));
        s.push_str(&format!(
            "  orders           : {} ({} filled, {} cancelled, {} rejected, {} failed, {} live at end)\n",
            self.orders.orders,
            self.orders.filled,
            self.orders.cancelled,
            self.orders.rejected,
            self.orders.failed,
            self.orders.live_at_end
        ));
        s.push_str(&format!("  fills            : {}\n", self.fills));
        let f = &self.fill_stats;
        s.push_str(&format!(
            "  fill rate        : {:.2}% of orders ({} filled, {} partial, {} never) — {:.2}% of size\n",
            f.fill_rate_pct, f.filled_orders, f.partial_orders, f.unfilled_orders, f.size_fill_ratio_pct
        ));
        s.push_str(&format!(
            "  partial fills    : {:.2}% of orders (#183)\n",
            f.partial_rate_pct
        ));
        let t = &self.trades;
        s.push_str(&format!(
            "  trades           : {} closed ({} win / {} loss / {} flat, win {:.0}%)\n",
            t.closed, t.wins, t.losses, t.flat, t.win_rate_pct
        ));
        s.push_str(&format!(
            "  gross            : +{:.2} / {:.2} (fees {:.2})\n",
            t.gross_profit_usd, t.gross_loss_usd, t.fees_usd
        ));
        match t.profit_factor {
            Some(pf) => s.push_str(&format!("  profit factor    : {:.2}\n", pf)),
            None => s.push_str("  profit factor    : n/a (no losing trades)\n"),
        }
        s.push_str(&format!("  net PnL          : {:.2}\n", t.net_pnl_usd));
        s.push_str(&format!(
            "  max drawdown     : {:.2} ({:.2}% of peak)\n",
            t.max_drawdown_usd, t.max_drawdown_pct
        ));
        s.push_str(&format!(
            "  open at end      : {} position(s), ${:.2} notional\n",
            self.open_positions, self.open_notional_usd
        ));
        for strat in &self.strategies {
            let name = strat.get("name").and_then(Value::as_str).unwrap_or("?");
            let pnl = strat
                .get("netPnlUsd")
                .map(render_num)
                .unwrap_or_else(|| "?".into());
            let n = strat
                .get("closedTrades")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let open = strat
                .get("openPositions")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            s.push_str(&format!(
                "  strategy {name:<12}: net {pnl}, {n} closed, {open} open\n"
            ));
        }
        if !self.risk_alerts.is_empty() {
            s.push_str(&format!(
                "  risk alerts      : {}\n",
                self.risk_alerts.len()
            ));
            for a in self.risk_alerts.iter().take(5) {
                s.push_str(&format!("    - {a}\n"));
            }
        }
        if !self.errors.is_empty() {
            s.push_str(&format!("  errors           : {}\n", self.errors.len()));
            for e in self.errors.iter().take(5) {
                s.push_str(&format!("    - {e}\n"));
            }
        }
        if !self.trade_lines.is_empty() {
            s.push_str("  trades:\n");
            for line in &self.trade_lines {
                s.push_str(&format!(
                    "    {:<14} {:<4} {:<18} {:>8.2} ({:.2}%)\n",
                    line.asset, line.direction, line.reason, line.net_pnl_usd, line.net_pnl_pct
                ));
            }
            if self.trade_lines_truncated > 0 {
                s.push_str(&format!("    … {} more\n", self.trade_lines_truncated));
            }
        }
        s
    }
}

fn render_num(v: &Value) -> String {
    match v {
        Value::Number(n) => format!("{n}"),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A Decimal carried either as a JSON number (the `dec_json` convention used by
/// the strategy accounting) or as a string (rust_decimal's default serde).
fn json_decimal(v: Option<&Value>) -> Option<Decimal> {
    match v? {
        Value::Number(n) => Decimal::from_str_exact(&n.to_string()).ok(),
        Value::String(s) => Decimal::from_str_exact(s.trim()).ok(),
        _ => None,
    }
}

/// Event-driven replay driver: a real [`Core`] on a virtual clock.
pub struct EventBacktester {
    source: Box<dyn DataSource>,
    core: Core,
    rx: mpsc::UnboundedReceiver<Event>,
    tick_ms: i64,
    tail_ms: i64,
    /// Orders by their last observed status.
    order_status: HashMap<String, String>,
    /// Orders by the last observed `(size, filled_size)` (#183).
    order_sizes: HashMap<String, (Decimal, Decimal)>,
    fills: u64,
    trades: Vec<TradeLine>,
    risk_alerts: Vec<String>,
    errors: Vec<String>,
    /// Re-check the invariants the equivalence gate relies on.
    checked_events: u64,
    /// A refused engine install (#265): why this replay must not produce a
    /// report. Kept rather than panicked on in `new` (which cannot fail) so the
    /// caller gets `run`'s ordinary `Err` — and a sweep can tell "this candidate
    /// could not load its strategy" from "the strategy had no signal".
    install_error: Option<String>,
}

/// Trades kept in the report's `trade_lines` (counts always cover all trades).
const MAX_TRADE_LINES: usize = 2_000;
/// Risk alerts / errors kept in the report.
const MAX_MESSAGES: usize = 200;

impl EventBacktester {
    /// Build a replay around `source`.
    ///
    /// The core config is forced to the replay-safe subset: `Mode::Dry`,
    /// no persistence (trade/order/position logs, near-miss), no live feeds or
    /// discovery, no shadow evolution. Strategy knobs (assets, sizing, gates,
    /// exit policy, per-strategy limits, fill model) are taken as configured.
    /// `event_archive_path` is left alone: recording during a replay is an
    /// explicit opt-in and is useful for trimming a long archive.
    pub fn new(mut cfg: BacktestConfig, source: Box<dyn DataSource>) -> Self {
        let mut core_cfg = std::mem::take(&mut cfg.core);
        core_cfg.mode = crate::model::Mode::Dry;
        core_cfg.trade_log_path = None;
        core_cfg.order_log_path = None;
        core_cfg.position_log_path = None;
        core_cfg.near_miss_path = None;
        core_cfg.discovery_enabled = false;
        core_cfg.feed_ws_enabled = false;
        core_cfg.market_plugin = None;
        core_cfg.shadow_evolution_enabled = false;
        core_cfg.shadow_evolution_tuning = None;
        // #234: this core may run under a counterfactual taker-fee schedule
        // (`--fee-model`), which the live charge sites refuse for any core that
        // has not declared itself a replay. The declaration belongs here — the
        // only constructor that may charge one.
        core_cfg.fee_schedule_replay = true;

        let mut core = Core::new(core_cfg.clone());
        // #265: a replay that asked for strategies and resolved none must fail
        // rather than report zero trades — the whole point of a sweep is that a
        // number means what it says.
        let install_error = if core_cfg.engine_enabled {
            core_cfg.install_engine(&mut core).err()
        } else {
            None
        };
        // Counterfactual knobs go in AFTER the engine registered its strategies
        // (the registry forwards to what is already wired), through the same
        // cell a Shadow Evolution proposal writes.
        if !cfg.hot_params.is_empty() {
            use crate::shadow_evolution::{ParamRegistry, StrategyParams};
            let mut by_strategy: std::collections::BTreeMap<&str, StrategyParams> =
                std::collections::BTreeMap::new();
            for (strategy, knob, value) in &cfg.hot_params {
                by_strategy
                    .entry(strategy.as_str())
                    .or_default()
                    .set(knob, *value);
            }
            let registry = std::sync::Arc::new(ParamRegistry::new());
            for (strategy, params) in by_strategy {
                registry.publish(strategy, params);
            }
            if let Some(engine) = core.engine.as_mut() {
                engine.set_hot_params(Some(registry));
            }
        }
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        core.set_event_sink(tx);
        let tick_ms = cfg.tick_ms.max(1);
        Self {
            source,
            core,
            rx,
            tick_ms,
            tail_ms: cfg.tail_ms.max(0),
            order_status: HashMap::new(),
            order_sizes: HashMap::new(),
            fills: 0,
            trades: Vec::new(),
            risk_alerts: Vec::new(),
            errors: Vec::new(),
            checked_events: 0,
            install_error,
        }
    }

    /// Read-only access to the replayed core (tests, post-run inspection).
    pub fn core(&self) -> &Core {
        &self.core
    }

    /// Mutable access to the replayed core (host a strategy, toggle one, seed
    /// state) before `run`.
    pub fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    /// Host an additional strategy into the replay's engine.
    ///
    /// The kernel ships no strategies (PR-B): `install_engine` loads whatever
    /// the configured `strategy_dir` holds and registers nothing else, so a
    /// programmatic replay that wants a specific strategy hands one over here —
    /// the same `EngineStrategy` contract a loaded library implements. Call it
    /// after construction and before `run`; like every registration the strategy
    /// starts DISABLED, so enable it by name on the core's engine.
    pub fn host_strategy(&mut self, strategy: Box<dyn crate::strategies::EngineStrategy>) {
        if let Some(engine) = self.core.engine.as_mut() {
            let _ = engine.register_user_strategy(strategy, "replay-hosted".into());
        }
    }

    /// Advance the virtual clock one maintenance step: the live loop's
    /// `tick` + `engine_evaluate`, in that order.
    fn step(&mut self, now_ms: i64) {
        if let Err(e) = self.core.tick(now_ms) {
            self.push_error(format!("tick@{now_ms}: {e:?}"));
        }
        self.core.engine_evaluate(now_ms);
        self.drain();
    }

    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::PositionClosed {
                    id,
                    asset,
                    direction,
                    reason,
                    strategy,
                    token_id,
                    condition_id,
                    net_pnl_usd,
                    net_pnl_pct,
                    ..
                } => {
                    self.trades.push(TradeLine {
                        id,
                        asset,
                        direction,
                        reason,
                        strategy,
                        token_id,
                        condition_id,
                        net_pnl_usd,
                        net_pnl_pct,
                    });
                }
                Event::OrderUpdate { order } => {
                    self.order_status
                        .insert(order.order_id.clone(), format!("{:?}", order.status));
                    // Latest (size, filled) per order: the fill-rate view (#183)
                    // needs what was ASKED for against what TRADED, which the
                    // status alone cannot say.
                    self.order_sizes
                        .insert(order.order_id.clone(), (order.size, order.filled_size));
                }
                Event::Fill { .. } => self.fills += 1,
                Event::RiskAlert { code, message } => {
                    self.push_risk(format!("{code:?}: {message}"));
                }
                Event::Error { error } => self.push_error(format!("{error:?}")),
                _ => {}
            }
        }
    }

    fn push_risk(&mut self, msg: String) {
        if self.risk_alerts.len() < MAX_MESSAGES {
            self.risk_alerts.push(msg);
        }
    }

    fn push_error(&mut self, msg: String) {
        if self.errors.len() < MAX_MESSAGES {
            self.errors.push(msg);
        }
    }

    fn order_counts(&self) -> OrderCounts {
        let mut c = OrderCounts {
            orders: self.order_status.len() as u64,
            ..Default::default()
        };
        for status in self.order_status.values() {
            match status.as_str() {
                "Filled" => c.filled += 1,
                "Cancelled" => c.cancelled += 1,
                "Rejected" => c.rejected += 1,
                "Failed" => c.failed += 1,
                "Pending" | "Live" | "PartiallyFilled" => c.live_at_end += 1,
                _ => {}
            }
        }
        c
    }

    /// Fill rate / partial-fill rate over every order the run saw (#183).
    fn fill_stats(&self) -> FillStats {
        let mut s = FillStats {
            orders: self.order_sizes.len() as u64,
            ..Default::default()
        };
        let mut requested = Decimal::ZERO;
        let mut filled = Decimal::ZERO;
        for (size, got) in self.order_sizes.values() {
            requested += *size;
            filled += *got;
            if *got <= Decimal::ZERO {
                s.unfilled_orders += 1;
            } else if *got >= *size {
                s.filled_orders += 1;
            } else {
                s.partial_orders += 1;
            }
        }
        let pct = |n: Decimal, d: Decimal| {
            if d <= Decimal::ZERO {
                Decimal::ZERO
            } else {
                n * Decimal::ONE_HUNDRED / d
            }
        };
        let total = Decimal::from(s.orders);
        s.fill_rate_pct = pct(Decimal::from(s.filled_orders + s.partial_orders), total);
        s.partial_rate_pct = pct(Decimal::from(s.partial_orders), total);
        s.size_fill_ratio_pct = pct(filled, requested);
        s
    }

    fn trade_stats(&self, fees_usd: Decimal) -> TradeStats {
        let mut wins = 0u64;
        let mut losses = 0u64;
        let mut flat = 0u64;
        let mut gross_profit = Decimal::ZERO;
        let mut gross_loss = Decimal::ZERO;
        let mut net = Decimal::ZERO;
        for t in &self.trades {
            net += t.net_pnl_usd;
            if t.net_pnl_usd > Decimal::ZERO {
                wins += 1;
                gross_profit += t.net_pnl_usd;
            } else if t.net_pnl_usd < Decimal::ZERO {
                losses += 1;
                gross_loss += t.net_pnl_usd.abs();
            } else {
                flat += 1;
            }
        }
        let closed = self.trades.len() as u64;
        // Realized equity curve: start from zero PnL, walk the trades in close
        // order, track peak-to-trough. Percentage is of the peak that preceded
        // the trough (a drawdown cannot exceed the equity that made it).
        let mut equity = Decimal::ZERO;
        let mut peak = Decimal::ZERO;
        let mut max_dd = Decimal::ZERO;
        let mut max_dd_pct = Decimal::ZERO;
        for t in &self.trades {
            equity += t.net_pnl_usd;
            if equity > peak {
                peak = equity;
            }
            let dd = peak - equity;
            if dd > max_dd {
                max_dd = dd;
                // Peak is > 0 whenever a drawdown exists (a fall from a positive
                // peak), so the division is safe; guard anyway.
                if peak > Decimal::ZERO {
                    max_dd_pct = (dd / peak) * Decimal::ONE_HUNDRED;
                }
            }
        }
        let pct = |n: u64, d: u64| {
            if d == 0 {
                Decimal::ZERO
            } else {
                Decimal::from(n) * Decimal::ONE_HUNDRED / Decimal::from(d)
            }
        };
        TradeStats {
            closed,
            wins,
            losses,
            flat,
            gross_profit_usd: gross_profit,
            gross_loss_usd: gross_loss,
            net_pnl_usd: net,
            avg_pnl_usd: if closed == 0 {
                Decimal::ZERO
            } else {
                net / Decimal::from(closed)
            },
            win_rate_pct: pct(wins, closed),
            profit_factor: if gross_loss > Decimal::ZERO {
                Some(gross_profit / gross_loss)
            } else {
                None
            },
            max_drawdown_usd: max_dd,
            max_drawdown_pct: max_dd_pct,
            fees_usd,
        }
    }
}

impl Backtester for EventBacktester {
    fn describe(&self) -> String {
        format!("{} → {:?}", self.source.describe(), self.core.mode())
    }

    fn run(&mut self) -> Result<BacktestReport, String> {
        // #265: the engine refused to install (an explicit strategy request that
        // resolved to nothing). Report it instead of replaying into a report that
        // would read as "the strategy produced no signals".
        if let Some(why) = &self.install_error {
            return Err(format!("strategy startup self-check: {why}"));
        }
        // The event clock starts at the first event's timestamp — the instant
        // the live core saw its first datum — and only ever moves forward.
        let mut pending = self.source.next_event();
        let mut clock = pending.as_ref().map(|p| p.at_ms.max(0)).unwrap_or(0);
        let start_at_ms = clock;
        let mut end_at_ms = clock;
        let mut delivered: u64 = 0;
        // Maintenance runs on its own `tick_ms` schedule, exactly like the live
        // server's interval timer: a real feed delivers bursts of sub-millisecond
        // events, so an event-gap-driven cycle would evaluate far less often than
        // live and exits would fire late (see `dense_stream_keeps_live_cadence`).
        let mut next_eval_ms = clock + self.tick_ms;

        while let Some(te) = pending.take() {
            let at = if te.at_ms > 0 { te.at_ms } else { clock };
            if at > clock {
                // Everything the live timer would have fired up to this instant.
                while next_eval_ms <= at {
                    self.step(next_eval_ms);
                    next_eval_ms += self.tick_ms;
                }
                clock = at;
            }
            // Late/out-of-order events still get the current clock (never a
            // backwards jump) — the source counts them separately.
            self.core.engine_on_data(te.event, clock);
            self.checked_events += 1;
            delivered += 1;
            end_at_ms = clock;
            self.drain();
            pending = self.source.next_event();
        }

        // Tail: keep ticking so maker escalations and pending exits resolve.
        if self.tail_ms > 0 {
            let end = clock + self.tail_ms;
            while next_eval_ms <= end {
                clock = next_eval_ms;
                self.step(clock);
                next_eval_ms += self.tick_ms;
            }
            if end > clock {
                clock = end;
            }
            end_at_ms = clock;
        }

        // Pull the last events out (a final close may have been emitted).
        self.drain();

        // Diagnostics are taken as of the end of the *virtual* run: the host
        // clock would make every book look years stale in an offline replay.
        let feed = self.core.engine_stats_at(clock);
        let strategies = self.core.strategy_stats();
        let blocked = feed.get("blocked").cloned().unwrap_or(Value::Null);
        let fees_usd: Decimal = strategies
            .iter()
            .filter_map(|s| json_decimal(s.get("feesUsd")))
            .sum();
        let views = self.core.position_views(clock);
        let open_notional: Decimal = views.iter().map(|p| p.entry_price * p.shares).sum();
        let stats = self.source.stats();
        let (trade_lines, truncated) = if self.trades.len() > MAX_TRADE_LINES {
            (
                self.trades[..MAX_TRADE_LINES].to_vec(),
                (self.trades.len() - MAX_TRADE_LINES) as u64,
            )
        } else {
            (self.trades.clone(), 0)
        };
        let trades = self.trade_stats(fees_usd);
        debug_assert_eq!(
            delivered, stats.events,
            "source delivered a different event count"
        );

        Ok(BacktestReport {
            source: self.source.describe(),
            source_stats: stats,
            tick_ms: self.tick_ms,
            tail_ms: self.tail_ms,
            start_at_ms,
            end_at_ms,
            virtual_ms: clock - start_at_ms,
            fill_model: self.core.config().fill_model,
            fee_schedule: crate::exit_policy::fee_schedule().into(),
            entry_maker_timeout_ms: self.core.config().entry_maker_timeout_ms,
            orders: self.order_counts(),
            fill_stats: self.fill_stats(),
            fills: self.fills,
            trades,
            strategies,
            feed,
            blocked,
            open_positions: views.len(),
            open_notional_usd: open_notional,
            risk_alerts: std::mem::take(&mut self.risk_alerts),
            errors: std::mem::take(&mut self.errors),
            trade_lines,
            trade_lines_truncated: truncated,
            forced_dry: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_source::open_replay;
    use crate::engine::DataEvent;
    use crate::exit_policy::ExitConfig;
    use crate::model::{CryptoMarket, Mode};
    use crate::position::PositionConfig;
    use crate::risk::RiskConfig;
    use rust_decimal_macros::dec;
    use std::path::PathBuf;

    /// A round slot boundary is far away at this clock, so nothing expires.
    const NOW: i64 = 1_000_000;

    fn market(now: i64) -> CryptoMarket {
        let slot = now / 1000 / 900;
        CryptoMarket {
            asset: "BTC".into(),
            condition_id: "cond".into(),
            question_id: "q".into(),
            up_token_id: "up".into(),
            down_token_id: "down".into(),
            up_price: dec!(0.6),
            down_price: dec!(0.4),
            expires_at_ms: (slot + 1) * 900 * 1000,
            round_slot: slot,
            neg_risk: true,
            question: "BTC up or down".into(),
        }
    }

    /// The production-shaped scenario (same as the P-1.1 dispatch tests, driven by
    /// the clock instead of by hand): a confirmed UP trend on a calm book, one dip
    /// that arms a resting maker entry, then a +100% book that lets the exit policy
    /// take profit. On the feed-ws path the entry fills at the escalation deadline
    /// (D-11), which is what this replay reproduces.
    fn scenario_events(now: i64) -> Vec<TimedEvent> {
        let mut evs = vec![TimedEvent {
            at_ms: now,
            event: DataEvent::RoundMarkets {
                markets: vec![market(now)],
                now_ms: now,
            },
        }];
        for i in 0..12 {
            let t = now + i * 1000;
            evs.push(TimedEvent {
                at_ms: t,
                event: DataEvent::Book {
                    token_id: "up".into(),
                    bids: vec![(dec!(0.55), dec!(100))],
                    asks: vec![(dec!(0.57), dec!(100))],
                    now_ms: t,
                },
            });
        }
        let t = now + 12_000;
        evs.push(TimedEvent {
            at_ms: t,
            event: DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: t,
            },
        });
        evs.push(TimedEvent {
            at_ms: t,
            event: DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(60000),
                now_ms: t,
            },
        });
        // The take-profit book arrives AFTER the escalation deadline: the
        // escalated taker leg fills against the 12 s book's 0.45 ask (the
        // honest FOK price), and the +100% bid then fires the exit policy.
        let t = now + 19_000;
        evs.push(TimedEvent {
            at_ms: t,
            event: DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.99), dec!(100))],
                asks: vec![(dec!(1.0), dec!(100))],
                now_ms: t,
            },
        });
        evs
    }

    /// A core config that can trade the scenario, built the way production builds
    /// it: `CoreConfig::install_engine` derives the engine from these knobs.
    fn base_core(archive: Option<String>) -> CoreConfig {
        CoreConfig {
            mode: Mode::Dry,
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            assets: vec!["BTC".into()],
            round_duration_sec: 900,
            min_round_age_sec: 0,
            trend_confirm_sec: 5,
            trend_window_floor_ms: 0,
            auto_exits_enabled: true,
            positions: PositionConfig {
                exit: ExitConfig {
                    min_time_left_sec: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
            event_archive_path: archive,
            ..Default::default()
        }
    }

    fn bt(core: CoreConfig, src: Box<dyn DataSource>, tail_ms: i64) -> BacktestReport {
        let mut b = EventBacktester::new(
            BacktestConfig {
                core,
                tick_ms: 50,
                tail_ms,
                hot_params: Vec::new(),
            },
            src,
        );
        // PR-B: the kernel registers no strategies, so a replay that drives the
        // reference dip buyer hosts the test adapter and enables it, exactly as
        // the production path enables a loaded library.
        b.host_strategy(Box::new(
            crate::strategies::test_support::TestSpreadArb::new(
                crate::signal::TrendConfig {
                    confirm_sec: 5,
                    window_floor_ms: 0,
                    ..Default::default()
                },
                Default::default(),
            ),
        ));
        assert!(
            b.core_mut().set_strategy_enabled("spread_arb", true),
            "the hosted adapter registers under its reference name"
        );
        b.run().expect("replay runs")
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("blitzkrieg-bt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn strategy<'a>(r: &'a BacktestReport, name: &str) -> &'a Value {
        r.strategies
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("no {name} in {}", r.render()))
    }

    #[test]
    fn identity_replay_trades_the_scenario_and_reports_it() {
        let r = bt(
            base_core(None),
            Box::new(VecSource::new(scenario_events(NOW))),
            10_000,
        );
        assert_eq!(
            r.trades.closed,
            1,
            "one round trip expected:\n{}",
            r.render()
        );
        assert_eq!(
            r.trades.wins,
            1,
            "the +100% book must take profit:\n{}",
            r.render()
        );
        assert!(r.trades.net_pnl_usd > Decimal::ZERO, "\n{}", r.render());
        assert_eq!(r.open_positions, 0, "the exit must flatten the position");
        assert_eq!(
            r.orders.filled,
            2,
            "escalated entry + exit:\n{}",
            r.render()
        );
        assert_eq!(
            r.orders.cancelled, 1,
            "the unfilled maker is cancelled at escalation"
        );
        assert_eq!(r.fills, 2);
        assert_eq!(r.source_stats.events, 16);
        assert_eq!(r.fill_model, FillModel::default());
        assert_eq!(r.entry_maker_timeout_ms, 5000);
        // The report's trade list and the strategy ledger must be the same money.
        let s = strategy(&r, "spread_arb");
        assert_eq!(json_decimal(s.get("netPnlUsd")), Some(r.trades.net_pnl_usd));
        assert_eq!(json_decimal(s.get("feesUsd")), Some(r.trades.fees_usd));
        assert!(
            r.trades.fees_usd > Decimal::ZERO,
            "the escalated taker entry pays a fee"
        );
        // #183: the report also states the fill rate it actually achieved, so an
        // acceptance run cannot read as "everything filled" by assumption.
        assert_eq!(r.fill_stats.orders, 3, "entry maker, escalated leg, exit");
        assert_eq!(r.fill_stats.filled_orders, 2);
        assert_eq!(r.fill_stats.partial_orders, 0);
        assert_eq!(r.fill_stats.unfilled_orders, 1, "the cancelled maker entry");
        assert_eq!(r.fill_stats.partial_rate_pct, Decimal::ZERO);
        assert!(
            r.fill_stats.size_fill_ratio_pct > Decimal::ZERO
                && r.fill_stats.size_fill_ratio_pct < Decimal::ONE_HUNDRED,
            "the cancelled entry traded no size: {}",
            r.fill_stats.size_fill_ratio_pct
        );
    }

    /// #183 ACCEPTANCE: a shallow crossing book leaves the entry PARTIALLY
    /// filled, and the report says so — before the depth cap this replay read as
    /// a 100% fill and hid the live-bug-② shape entirely.
    #[test]
    fn a_shallow_book_reports_a_partial_fill() {
        let mut cfg = base_core(None);
        // No exit management: the subject here is the entry's fill, not what the
        // exit policy would do with a one-share position afterwards.
        cfg.auto_exits_enabled = false;
        let mut b = EventBacktester::new(
            BacktestConfig {
                core: cfg,
                tick_ms: 50,
                tail_ms: 0,
                hot_params: Vec::new(),
            },
            Box::new(VecSource::new(vec![
                TimedEvent {
                    at_ms: NOW,
                    event: DataEvent::RoundMarkets {
                        markets: vec![market(NOW)],
                        now_ms: NOW,
                    },
                },
                TimedEvent {
                    at_ms: NOW,
                    event: DataEvent::Book {
                        token_id: "up".into(),
                        bids: vec![(dec!(0.42), dec!(100))],
                        asks: vec![(dec!(0.44), dec!(100))],
                        now_ms: NOW,
                    },
                },
                // The dip: one share rests at the resting bid's own price, so
                // the maker trades exactly that one share and no more.
                TimedEvent {
                    at_ms: NOW + 1_000,
                    event: DataEvent::Book {
                        token_id: "up".into(),
                        bids: vec![(dec!(0.42), dec!(100))],
                        asks: vec![(dec!(0.43), dec!(1))],
                        now_ms: NOW + 1_000,
                    },
                },
            ])),
        );
        b.core_mut()
            .place(
                crate::model::OrderRequest {
                    token_id: "up".into(),
                    condition_id: "cond".into(),
                    side: crate::model::Side::Buy,
                    mode: crate::model::FillPolicy::Maker,
                    price: dec!(0.43),
                    size: dec!(5),
                    internal_key: "entry:shallow".into(),
                    strategy: "spread_arb".into(),
                    asset: "BTC".into(),
                    direction: "up".into(),
                    round_slot: NOW / 1000 / 900,
                },
                0,
                NOW,
            )
            .expect("the maker entry places");

        let r = b.run().expect("replay runs");
        assert_eq!(r.fill_stats.orders, 1, "\n{}", r.render());
        assert_eq!(
            r.fill_stats.partial_orders,
            1,
            "1 of 5 shares traded:\n{}",
            r.render()
        );
        assert_eq!(r.fill_stats.filled_orders, 0);
        assert_eq!(r.fill_stats.unfilled_orders, 0);
        assert_eq!(
            r.fill_stats.size_fill_ratio_pct,
            dec!(20),
            "1 filled of 5 requested"
        );
        assert_eq!(r.fill_stats.partial_rate_pct, dec!(100));
        assert_eq!(r.fills, 1, "one partial fill is still one fill event");
        // And the render must show it, not just the JSON.
        assert!(
            r.render().contains("partial fill"),
            "the human report says what traded:\n{}",
            r.render()
        );
    }

    /// A real feed delivers bursts of sub-millisecond events, but maintenance in
    /// live runs on the server's `tick_ms` *timer*, not on arrivals. Regression:
    /// with an event-gap-driven cycle a 13-minute real capture replayed 803
    /// maintenance cycles against the live core's 15 618, so exits fired late and
    /// the per-cycle blocked tallies were starved (D-12/§35).
    #[test]
    fn dense_stream_keeps_live_evaluation_cadence() {
        let mut evs = vec![TimedEvent {
            at_ms: NOW,
            event: DataEvent::RoundMarkets {
                markets: vec![market(NOW)],
                now_ms: NOW,
            },
        }];
        for i in 0..=2_000 {
            let t = NOW + i;
            evs.push(TimedEvent {
                at_ms: t,
                event: DataEvent::Spot {
                    asset: "BTC".into(),
                    price: dec!(60000),
                    now_ms: t,
                },
            });
        }
        // Out-of-order arrivals are a normal property of a multi-stream feed:
        // the late event must be clamped, never rewind the clock, and must not
        // buy itself an extra maintenance cycle.
        evs.push(TimedEvent {
            at_ms: NOW + 500,
            event: DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(60001),
                now_ms: NOW + 500,
            },
        });

        let r = bt(base_core(None), Box::new(VecSource::new(evs)), 0);

        // 2 000 ms of stream at the live 50 ms cadence = 40 maintenance cycles,
        // regardless of the 2 002 events that arrived in between.
        assert_eq!(r.feed["evaluations"], 40, "\n{}", r.render());
        assert_eq!(
            r.source_stats.events, 2_003,
            "every event is still delivered"
        );
        assert_eq!(
            r.virtual_ms, 2_000,
            "the late event must not move the clock"
        );
    }

    /// The other half of the cadence contract: a sparse stream (gaps far larger
    /// than `tick_ms`) must still fire every scheduled cycle, so a quiet market
    /// cannot stop exits from being evaluated.
    #[test]
    fn sparse_stream_evaluates_every_due_cycle() {
        let events = vec![
            TimedEvent {
                at_ms: NOW,
                event: DataEvent::RoundMarkets {
                    markets: vec![market(NOW)],
                    now_ms: NOW,
                },
            },
            TimedEvent {
                at_ms: NOW + 10_000,
                event: DataEvent::Spot {
                    asset: "BTC".into(),
                    price: dec!(60000),
                    now_ms: NOW + 10_000,
                },
            },
        ];
        let r = bt(base_core(None), Box::new(VecSource::new(events)), 0);
        assert_eq!(r.feed["evaluations"], 200, "10 000 ms / 50 ms per cycle");
    }

    #[test]
    fn live_archive_replay_is_identical() {
        let dir = tmp_dir("equiv");
        let archive = dir.join("events.jsonl").display().to_string();
        let events = scenario_events(NOW);

        // "Live" run: the real core consumes the events and mirrors them to disk.
        let live = bt(
            base_core(Some(archive.clone())),
            Box::new(VecSource::new(events)),
            10_000,
        );
        assert!(
            std::path::Path::new(&archive).exists(),
            "the live run must record an archive"
        );
        assert!(
            live.trades.closed >= 1,
            "the equivalence fixture must trade:\n{}",
            live.render()
        );

        // Backtest run: the same core, fed from the archive.
        let replay = bt(
            base_core(None),
            Box::new(open_replay(&archive).unwrap()),
            10_000,
        );

        assert_eq!(
            replay.source_stats, live.source_stats,
            "same event stream, no parse damage"
        );
        assert_eq!(replay.source_stats.malformed_lines, 0);
        assert_eq!(replay.start_at_ms, live.start_at_ms);
        assert_eq!(replay.end_at_ms, live.end_at_ms);
        assert_eq!(replay.trades.closed, live.trades.closed);
        assert_eq!(replay.trades.wins, live.trades.wins);
        assert_eq!(replay.trades.losses, live.trades.losses);
        assert_eq!(
            replay.trades.net_pnl_usd, live.trades.net_pnl_usd,
            "PnL must match exactly"
        );
        assert_eq!(replay.trades.fees_usd, live.trades.fees_usd);
        assert_eq!(replay.fills, live.fills);
        assert_eq!(replay.orders.filled, live.orders.filled);
        assert_eq!(replay.orders.cancelled, live.orders.cancelled);
        assert_eq!(replay.open_positions, live.open_positions);
        assert_eq!(replay.feed["books"], live.feed["books"]);
        assert_eq!(replay.feed["evaluations"], live.feed["evaluations"]);
        assert_eq!(replay.blocked, live.blocked);
        assert_eq!(
            replay.strategies, live.strategies,
            "per-strategy ledger must match"
        );
        assert_eq!(
            replay
                .trade_lines
                .iter()
                .map(|t| (t.asset.clone(), t.reason.clone(), t.net_pnl_usd))
                .collect::<Vec<_>>(),
            live.trade_lines
                .iter()
                .map(|t| (t.asset.clone(), t.reason.clone(), t.net_pnl_usd))
                .collect::<Vec<_>>(),
            "trade list must match"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_archive_lines_are_skipped_without_changing_the_result() {
        let dir = tmp_dir("badline");
        let archive = dir.join("events.jsonl").display().to_string();
        let clean = bt(
            base_core(Some(archive.clone())),
            Box::new(VecSource::new(scenario_events(NOW))),
            10_000,
        );

        // Damage the tail the way a killed process would: a truncated record plus
        // junk that no writer should ever produce.
        let text = std::fs::read_to_string(&archive).unwrap();
        let last = text.lines().last().unwrap();
        let mut damaged = text.clone();
        damaged.push_str(&last[..last.len() / 2]);
        damaged.push_str("\nnot json at all\n");
        std::fs::write(&archive, damaged).unwrap();

        let replay = bt(
            base_core(None),
            Box::new(open_replay(&archive).unwrap()),
            10_000,
        );
        assert_eq!(
            replay.source_stats.malformed_lines, 2,
            "truncated + junk lines are both counted"
        );
        assert_eq!(
            replay.source_stats.events, clean.source_stats.events,
            "no event is lost"
        );
        assert_eq!(
            replay.trades.closed, clean.trades.closed,
            "a damaged tail is not fatal"
        );
        assert_eq!(replay.trades.wins, clean.trades.wins);
        assert_eq!(replay.trades.net_pnl_usd, clean.trades.net_pnl_usd);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slippage_worsens_the_replay_pnl() {
        let events = scenario_events(NOW);
        let identity = bt(
            base_core(None),
            Box::new(VecSource::new(events.clone())),
            10_000,
        );

        let mut slipped_core = base_core(None);
        slipped_core.fill_model = FillModel {
            taker_slippage_ticks: 2,
            ..FillModel::default()
        };
        let slipped = bt(slipped_core, Box::new(VecSource::new(events)), 10_000);

        assert_eq!(slipped.trades.closed, identity.trades.closed);
        let diff = identity.trades.net_pnl_usd - slipped.trades.net_pnl_usd;
        // Only TAKER legs slip: here the escalated entry buys 2 ticks higher
        // (0.02 x 10 shares = 0.20), while the take-profit exit rests at its own
        // limit and must not slip — a maker fill is priced by your own quote.
        assert!(
            diff > dec!(0.19) && diff < dec!(0.25),
            "expected ~0.20 of taker slippage cost, got {diff}\n{}",
            slipped.render()
        );
    }

    #[test]
    fn a_long_escalation_deadline_leaves_the_maker_entry_unfilled() {
        // Documents the current live fill path (D-11): on a feed-driven replay the
        // resting maker entry fills at the escalation deadline, so pushing that
        // deadline past the replay window means no entry at all.
        let mut core = base_core(None);
        core.entry_maker_timeout_ms = 60_000;
        let r = bt(core, Box::new(VecSource::new(scenario_events(NOW))), 10_000);
        assert_eq!(
            r.orders.orders,
            1,
            "the entry is still placed:\n{}",
            r.render()
        );
        assert_eq!(
            r.orders.live_at_end, 1,
            "it is still resting when the replay ends"
        );
        assert_eq!(r.fills, 0);
        assert_eq!(r.trades.closed, 0);
        assert_eq!(r.open_positions, 0);

        // Same events, default deadline: the escalation fills it and the exit closes.
        let r2 = bt(
            base_core(None),
            Box::new(VecSource::new(scenario_events(NOW))),
            10_000,
        );
        assert_eq!(r2.fills, 2);
        assert_eq!(r2.trades.closed, 1);
    }
}
