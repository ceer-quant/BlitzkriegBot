//! Core service — assembles the OME, ledger, risk gate and dry matcher and
//! implements every typed command. Synchronous except for the event sink, so it
//! is fully unit-testable; the async UDS layer and live venue sit on top.

use crate::ipc::schema::Event;
use crate::ledger::Ledger;
use crate::model::*;
use crate::ome::{FillDelta, Ome, SubmitParams};
use crate::position::{OpenParams, PositionConfig, PositionManager};
use crate::shadow_evolution::{EvolutionOutcome, EvolutionStatus, MutableParams, ShadowEvolution, ShadowEvolutionConfig};
use crate::risk::{LossBreaker, RiskConfig, RiskGate};
use crate::sim::{rests_on_book, Book};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub mode: Mode,
    pub default_maker_timeout_ms: i64,
    pub risk: RiskConfig,
    /// Starting simulated cash balance in DRY mode.
    pub dry_seed_balance: Decimal,
    /// Condition ids to subscribe on the live user channel (empty = REST only).
    pub markets: Vec<String>,
    /// Position sizing/exit/cooldown config (Rust owns positions in P2).
    pub positions: PositionConfig,
    /// Consecutive-loss breaker threshold and cooldown.
    pub max_consecutive_losses: u32,
    pub breaker_cooldown_sec: i64,
    /// Round length used to derive a position's expiry from its round slot.
    pub round_duration_sec: i64,
    /// When false the core never auto-submits exits on tick; positions are only
    /// closed by explicit fills/flatten. Used by order-layer tests and for a
    /// "positions managed externally" deployment. Default true.
    pub auto_exits_enabled: bool,
    /// Instantiate the self-driving market-data engine (signal → order). Default
    /// false so the P0/P1/P2 bridges keep working until the feed layer is wired.
    pub engine_enabled: bool,
    /// Assets the engine trades and its sizing/round timing (mirrors the TS config).
    pub assets: Vec<String>,
    pub min_round_age_sec: i64,
    pub size_usd: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
    /// Engine trend confirmation window (sec) and floor (ms) — exposed so tests
    /// and ops can shorten the confirmation window without touching code.
    pub trend_confirm_sec: i64,
    pub trend_window_floor_ms: i64,
    /// Start Rust-native WS feeds (P4): Polymarket orderbook + Binance spot.
    /// When enabled Node no longer has to push `books.*` / `spot.price`.
    pub feed_ws_enabled: bool,
    /// Select the operative market plugin by name (matches `MarketPlugin::name`,
    /// e.g. "polymarket"). None = first registered. Ignored when the name is not
    /// registered (falls back to the first) so a stray flag never bricks startup.
    pub market_plugin: Option<String>,
    /// Binance stream symbols (e.g. ["BTC","ETH"]); empty disables spot.
    pub binance_assets: Vec<String>,
    /// Where to append near-miss records (blocked signals + path). None = off.
    pub near_miss_path: Option<String>,
    /// Where to append closed trades (Node-compatible JSONL). None = off.
    pub trade_log_path: Option<String>,
    /// Where to persist tracked ORDERS (crash recovery + orphan detection).
    /// None = off. Default `data/orders/orders.jsonl`.
    pub order_log_path: Option<String>,
    /// Where to persist OPEN POSITIONS (crash recovery). None = off.
    /// Default `data/positions/positions.jsonl`.
    pub position_log_path: Option<String>,
    /// Discover rounds inside the core (Gamma slug queries) instead of relying on
    /// Node to push `engine.markets`. Default true when the engine is on.
    pub discovery_enabled: bool,
    /// Start with Shadow Evolution enabled (opt-in; default false).
    pub shadow_evolution_enabled: bool,
    /// Optional Shadow Evolution tuning (tests/ops). None = crate defaults.
    pub shadow_evolution_tuning: Option<ShadowEvolutionTuning>,
    /// Optional per-strategy entry caps, keyed by strategy name (P-1.1). An
    /// absent entry means unlimited — the builtin `spread_arb` keeps its current
    /// behaviour unless an operator configures a cap.
    pub strategy_limits: HashMap<String, StrategyLimit>,
    /// Maker→taker escalation deadline for engine entries (P-1.2). Defaults to the
    /// long-standing 5000 ms; a backtest may model a different venue latency.
    pub entry_maker_timeout_ms: i64,
    /// Fill-realism model for the dry matcher (P-1.2). Default = identity, so the
    /// live/dry path is unchanged; a backtest raises slippage/latency/probability.
    pub fill_model: crate::sim::FillModel,
    /// Where to mirror every market-data event the engine consumes (P-1.3).
    /// None = off (default). Used to build the local L2/tick corpus a backtest
    /// replays.
    pub event_archive_path: Option<String>,
    /// Stop recording once the archive reaches this size (MiB); 0 = unlimited.
    /// Nothing is ever deleted — a full archive just stops growing. This is the
    /// whole-session bound; `event_archive_rotate_mb` bounds one segment.
    pub event_archive_max_mb: u64,
    /// Rotate the archive into a new segment every this many MiB (0 = never).
    /// A 24/7 capture needs this: `≈11 MB/min` reaches any sane session cap within
    /// hours, and an un-rotated archive that hits the cap goes dark silently.
    /// Rotation renames the finished segment (UTC-stamped) and opens a fresh one;
    /// nothing is ever deleted.
    pub event_archive_rotate_mb: u64,
    /// Stop recording rather than let the archive's volume drop below this many
    /// MiB free (0 = no guard). Checked once per rotation, so it costs nothing on
    /// the hot path.
    pub event_archive_min_free_mb: u64,
}

impl CoreConfig {
    /// Build the engine config from this core config — the ONE mapping used by
    /// both the live server and the backtester, so a replay drives the identical
    /// engine a live run would (P-1.2).
    pub fn engine_config(&self) -> crate::engine::EngineConfig {
        crate::engine::EngineConfig {
            scanner: crate::scanner::ScannerConfig {
                assets: self.assets.clone(),
                round_duration_sec: self.round_duration_sec,
                min_round_age_sec: self.min_round_age_sec,
                min_time_left_sec: self.positions.exit.min_time_left_sec,
            },
            trend: crate::signal::TrendConfig {
                confirm_sec: self.trend_confirm_sec,
                window_floor_ms: self.trend_window_floor_ms,
                ..Default::default()
            },
            size_usd: self.size_usd,
            min_shares: self.min_shares,
            max_shares: self.max_shares,
            strategy_sizes: self
                .strategy_limits
                .iter()
                .map(|(name, limit)| (name.clone(), limit.sizing()))
                .collect(),
            ..Default::default()
        }
    }

    /// Install the engine described by this config (the live server's startup
    /// step, reused verbatim by the backtester).
    pub fn install_engine(&self, core: &mut Core) {
        core.enable_engine(crate::engine::Engine::new(self.engine_config()));
    }
}

/// Entry caps and sizing for one strategy (E2-a). Every field is optional;
/// `None` = inherit the global value (no cap / global sizing). A per-strategy
/// sizing override can only tighten the global risk band, never widen it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StrategyLimit {
    /// Max simultaneously open positions held by the strategy.
    pub max_open_positions: Option<usize>,
    /// Max notional (USD, sum of entry cost) held open by the strategy. A new
    /// entry is rejected when open notional + its own notional would exceed it.
    #[serde(with = "crate::decimal::opt", default)]
    pub max_open_notional_usd: Option<Decimal>,
    /// Target notional per entry (USD). Clamped to the global `size_usd`.
    #[serde(with = "crate::decimal::opt", default)]
    pub size_usd: Option<Decimal>,
    /// Floor on shares per entry. Raised to the global `min_shares` when lower.
    #[serde(with = "crate::decimal::opt", default)]
    pub min_shares: Option<Decimal>,
    /// Ceiling on shares per entry. Clamped to the global `max_shares`.
    #[serde(with = "crate::decimal::opt", default)]
    pub max_shares: Option<Decimal>,
}

impl StrategyLimit {
    /// The sizing knobs this limit overrides (`None` across the board when it
    /// configures only caps — the engine then uses the globals).
    pub fn sizing(&self) -> crate::engine::StrategySize {
        crate::engine::StrategySize {
            size_usd: self.size_usd,
            min_shares: self.min_shares,
            max_shares: self.max_shares,
        }
    }
}

/// Overridable Shadow Evolution thresholds (all optional).
#[derive(Debug, Clone, Default)]
pub struct ShadowEvolutionTuning {
    pub min_sample_count: Option<u32>,
    pub min_win_rate_improvement: Option<Decimal>,
    pub min_profit_factor_improvement: Option<Decimal>,
    pub min_observation_secs: Option<i64>,
    pub cooldown_secs: Option<i64>,
    pub variant_count: Option<usize>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Dry,
            default_maker_timeout_ms: 5000,
            risk: RiskConfig::default(),
            dry_seed_balance: Decimal::from(10_000),
            markets: Vec::new(),
            positions: PositionConfig::default(),
            max_consecutive_losses: 3,
            breaker_cooldown_sec: 300,
            round_duration_sec: 900,
            auto_exits_enabled: true,
            engine_enabled: false,
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            min_round_age_sec: 30,
            size_usd: Decimal::new(25, 1), // 2.5
            min_shares: Decimal::from(10),
            max_shares: Decimal::from(10),
            trend_confirm_sec: 60,
            trend_window_floor_ms: 10_000,
            feed_ws_enabled: false,
            market_plugin: None,
            binance_assets: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            near_miss_path: None,
            trade_log_path: Some("data/trades/trades.jsonl".to_string()),
            order_log_path: Some("data/orders/orders.jsonl".to_string()),
            position_log_path: Some("data/positions/positions.jsonl".to_string()),
            discovery_enabled: true,
            shadow_evolution_enabled: false,
            shadow_evolution_tuning: None,
            strategy_limits: HashMap::new(),
            entry_maker_timeout_ms: 5000,
            fill_model: crate::sim::FillModel::default(),
            event_archive_path: None,
            event_archive_max_mb: 512,
            event_archive_rotate_mb: 0,
            event_archive_min_free_mb: 0,
        }
    }
}

pub struct Core {
    config: CoreConfig,
    ome: Ome,
    ledger: Ledger,
    risk: RiskGate,
    breaker: LossBreaker,
    positions: PositionManager,
    books: HashMap<TokenId, Book>,
    /// Exit reason chosen for an in-flight closing SELL, keyed by token id, so a
    /// full sell fill closes the position with the right reason.
    exit_reasons: HashMap<TokenId, ExitReason>,
    /// Strategy close intents drained from the engine in `engine_evaluate` and
    /// consumed by the shared exit-submission path in `run_exit_checks`.
    strategy_exits: Vec<crate::strategies::StrategyExitIntent>,
    /// Optional self-driving engine (P3). When present, book/spot/round events
    /// flow in and the core evaluates strategies and places orders on its own.
    engine: Option<crate::engine::Engine>,
    /// Optional Rust-native feed handle (P4); set when `--feed-ws` is enabled.
    feed: Option<std::sync::Arc<dyn blitzkrieg_market_api::SubscriptionControl>>,
    /// Extension registry (plugin lifecycle).
    extensions: crate::extension::ExtensionRegistry,
    /// Shadow Evolution (opt-in; disabled by default).
    shadow_evolution: ShadowEvolution,
    /// Closed-trade log (Node-compatible), when `trade_log_path` is set.
    trade_db: Option<crate::trade_db::TradeDb>,
    /// Durable order log, when `order_log_path` is set. Orders are restored from
    /// it at startup so a restart never forgets resting orders (orphan guard).
    order_db: Option<crate::order_db::OrderDb>,
    /// Durable snapshot of the OPEN position book, when `position_log_path` is
    /// set. Restored at startup so a restart never forgets open positions
    /// (unmanaged-exit guard; same failure class as the orphan-order bug).
    position_db: Option<crate::position_db::PositionDb>,
    /// Feed/decision counters for observability (P4 diagnostics).
    stats: CoreStats,
    /// Optional mirror of every market-data event into a JSONL archive (P-1.3).
    /// The backtester replays this file.
    event_archive: Option<crate::data_source::EventArchive>,
    /// Per-strategy order/trade accounting (P-1.1). Session-scoped: reset on
    /// restart, like `stats`; the durable per-trade record is the trade log.
    strategy_accounting: HashMap<String, StrategyAccounting>,
    next_id: u64,
    tx: Option<mpsc::UnboundedSender<Event>>,
}

impl Core {
    pub fn new(config: CoreConfig) -> Self {
        let trade_db = config.trade_log_path.as_ref().map(crate::trade_db::TradeDb::new);
        let order_db = config.order_log_path.as_ref().map(crate::order_db::OrderDb::new);
        let position_db = config.position_log_path.as_ref().map(crate::position_db::PositionDb::new);
        let shadow_cfg = {
            let mut c = ShadowEvolutionConfig::default();
            c.enabled = config.shadow_evolution_enabled;
            if let Some(t) = &config.shadow_evolution_tuning {
                if let Some(v) = t.min_sample_count { c.min_sample_count = v; }
                if let Some(v) = t.min_win_rate_improvement { c.min_win_rate_improvement = v; }
                if let Some(v) = t.min_profit_factor_improvement { c.min_profit_factor_improvement = v; }
                if let Some(v) = t.min_observation_secs { c.min_observation_secs = v; }
                if let Some(v) = t.cooldown_secs { c.cooldown_secs = v; }
                if let Some(v) = t.variant_count { c.variant_count = v; }
            }
            // D-2: variant exits must replay the SAME policy the live position
            // manager runs, or the counterfactual is judged against an exit
            // mechanism the live path never uses.
            c.exit_cfg = config.positions.exit.clone();
            c
        };
        let breaker = LossBreaker::new(config.max_consecutive_losses, config.breaker_cooldown_sec);
        let positions = PositionManager::new(config.positions.clone());
        // Optional market-data archive (P-1.3). A failure to open only disables
        // recording — trading must never be blocked by an archive path problem.
        let event_archive = config.event_archive_path.as_ref().and_then(|p| {
            let cap_bytes = config.event_archive_max_mb.saturating_mul(1024 * 1024);
            let rotate_bytes = config.event_archive_rotate_mb.saturating_mul(1024 * 1024);
            let min_free_bytes =
                config.event_archive_min_free_mb.saturating_mul(1024 * 1024);
            match crate::data_source::EventArchive::open_full(
                std::path::Path::new(p),
                cap_bytes,
                rotate_bytes,
                min_free_bytes,
            ) {
                Ok(a) => {
                    tracing::info!(
                        path = %p,
                        cap_mb = config.event_archive_max_mb,
                        rotate_mb = config.event_archive_rotate_mb,
                        min_free_mb = config.event_archive_min_free_mb,
                        "recording market-data archive"
                    );
                    Some(a)
                }
                Err(e) => {
                    tracing::warn!(path = %p, error = %e, "cannot open event archive; recording disabled");
                    None
                }
            }
        });
        let mut ledger = Ledger::new();
        // DRY mode has no venue to reconcile against; seed the local cash so the
        // reserve/overspend gate is meaningful for embedders that don't seed it.
        if config.mode == Mode::Dry {
            ledger.set_balance(config.dry_seed_balance);
        }
        Self {
            risk: RiskGate::new(config.risk.clone()),
            config,
            ome: Ome::new(),
            ledger,
            breaker,
            positions,
            books: HashMap::new(),
            exit_reasons: HashMap::new(),
            strategy_exits: Vec::new(),
            engine: None,
            feed: None,
            extensions: {
                let mut reg = crate::extension::ExtensionRegistry::new();
                // Built-in example extension (installed, not enabled by default).
                reg.install(Box::new(crate::extension::builtins::BinanceSpotExtension::new()), None);
                reg
            },
            shadow_evolution: ShadowEvolution::new(shadow_cfg, MutableParams::default()),
            trade_db,
            order_db,
            position_db,
            stats: CoreStats::default(),
            event_archive,
            strategy_accounting: HashMap::new(),
            next_id: 1,
            tx: None,
        }
    }

    pub fn set_event_sink(&mut self, tx: mpsc::UnboundedSender<Event>) {
        self.tx = Some(tx);
    }

    /// Restore the OME from the durable order log (crash recovery). Call once at
    /// startup, BEFORE trading begins. Live orders are re-adopted (with their
    /// venue ids) so the startup sweep can reconcile them; terminal orders are
    /// dropped and the log compacted to just the restored set.
    pub fn restore_orders(&mut self) -> usize {
        let Some(db) = self.order_db.as_ref() else { return 0 };
        let loaded = db.load();
        if loaded.is_empty() {
            return 0;
        }
        let live: Vec<crate::model::TrackedOrder> =
            loaded.into_iter().filter(|o| o.status.is_live()).collect();
        let n = live.len();
        self.ome.restore(live.clone());
        db.compact(&live);
        if n > 0 {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!("recovered {n} live order(s) from the order log after restart"),
            });
        }
        n
    }

    /// Persist one order's current snapshot (best effort).
    fn persist_order(&self, id: &str) {
        if let (Some(db), Some(o)) = (self.order_db.as_ref(), self.ome.get(id)) {
            db.append(o);
        }
    }

    /// Restore the OPEN position book from durable storage (crash recovery). Call
    /// once at startup, BEFORE trading begins. Without this a restart forgets
    /// every open position: it is no longer valued or exit-managed, so the trade
    /// drifts to expiry unmanaged. Mirrors `restore_orders`.
    pub fn restore_positions(&mut self) -> usize {
        let Some(db) = self.position_db.as_ref() else { return 0 };
        let loaded = db.load();
        if loaded.is_empty() {
            return 0;
        }
        let n = loaded.len();
        self.positions.restore_open(loaded);
        if n > 0 {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!("recovered {n} open position(s) from the position log after restart"),
            });
        }
        n
    }

    /// Persist the current OPEN position set (best effort). Positions are few and
    /// short-lived, so the whole set is rewritten on every change rather than
    /// appended (a close needs no tombstone).
    fn persist_positions(&self) {
        if let Some(db) = self.position_db.as_ref() {
            db.save(self.positions.open_positions());
        }
    }

    /// Venue order ids the OME believes are resting (orphan detection input).
    pub fn known_venue_ids(&self) -> std::collections::HashSet<String> {
        self.ome.known_venue_ids()
    }

    /// Cancel a venue order the OME has no record of. Used by the startup sweep
    /// to clear orphans left by a previous process/crash. No local state exists
    /// for these ids, so this is purely a venue-side action; the caller performs
    /// the actual cancel and reports the id here for logging/visibility.
    pub fn note_orphan_cancelled(&self, venue_order_id: &str) {
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::Internal,
            message: format!("cancelled orphan venue order {venue_order_id} (not in local ledger)"),
        });
    }
    pub fn mode(&self) -> Mode {
        self.config.mode
    }
    pub fn set_balance(&mut self, b: Decimal) {
        self.ledger.set_balance(b);
    }
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }
    pub fn ome(&self) -> &Ome {
        &self.ome
    }
    pub fn positions(&self) -> &PositionManager {
        &self.positions
    }

    /// Mutable access to config (engine wiring adjusts exits/positions live).
    pub fn config_mut(&mut self) -> &mut CoreConfig {
        &mut self.config
    }

    /// Read-only view of the active config (diagnostics/backtest reports).
    pub fn config(&self) -> &CoreConfig {
        &self.config
    }

    // ── Strategy engine & extensions (P0.5) ─────────────────────────────────
    /// Strategy names the kernel supports (the self-driving engine's set).
    pub fn strategy_names(&self) -> Vec<String> {
        self.engine.as_ref().map(|e| e.supported_strategies()).unwrap_or_default()
    }
    /// Enabled strategy names.
    pub fn enabled_strategy_names(&self) -> Vec<String> {
        self.engine.as_ref().map(|e| e.enabled_strategies()).unwrap_or_default()
    }
    /// Toggle a strategy on the driving engine. Returns false when the engine
    /// is not attached or the name is unknown.
    pub fn set_strategy_enabled(&mut self, name: &str, enabled: bool) -> bool {
        self.engine.as_mut().map(|e| e.set_strategy_enabled(name, enabled)).unwrap_or(false)
    }
    /// Load an external v2 strategy shared library and register it into the
    /// driving engine's live dispatch. It starts DISABLED — an explicit
    /// `strategy.enable` is required before it can trade. External and in-tree
    /// strategies implement the same full `EngineStrategy` contract; this is
    /// only a loading difference. Requires `strategy-loading`.
    pub fn load_strategy_lib(&mut self, path: &str) -> String {
        #[cfg(feature = "strategy-loading")]
        {
            use crate::strategy_engine::loader::load_foreign;
            let p = std::path::Path::new(path);
            let Some(engine) = self.engine.as_mut() else {
                return "Failed: strategy engine not attached (load libraries after engine init)".into();
            };
            return match load_foreign(p) {
                Ok(loaded) => {
                    let name = loaded.name.clone();
                    let version = loaded.version.clone();
                    match engine.register_user_strategy(
                        Box::new(loaded.strategy),
                        format!("dylib:{}", p.display()),
                    ) {
                        Ok(_) => format!("{name}@{version} registered into the engine dispatch (disabled)"),
                        Err(reason) => format!("rejected: {reason}"),
                    }
                }
                Err(outcome) => format!("{outcome:?}"),
            };
        }
        #[cfg(not(feature = "strategy-loading"))]
        {
            let outcome = crate::strategy_engine::loader::load_strategy(std::path::Path::new(path));
            format!("{outcome:?}")
        }
    }

    pub fn extensions(&self) -> &crate::extension::ExtensionRegistry {
        &self.extensions
    }
    pub fn extensions_mut(&mut self) -> &mut crate::extension::ExtensionRegistry {
        &mut self.extensions
    }

    /// List installed extensions (name, type, lifecycle state).
    pub fn extension_list(
        &self,
    ) -> Vec<(String, crate::extension::ExtensionType, crate::extension::ExtensionState)> {
        self.extensions.list()
    }

    /// Install an extension (state = Installed). Lifecycle transitions then go
    /// through `enable_extension` / `disable_extension`.
    pub fn install_extension(
        &mut self,
        ext: Box<dyn crate::extension::Extension>,
        config_path: Option<std::path::PathBuf>,
    ) {
        self.extensions.install(ext, config_path);
    }

    /// Enable an extension (runs `on_load`). The extension receives ONLY a
    /// narrowed context — no credentials, venue client, socket or internal state.
    pub async fn enable_extension(&mut self, name: &str) -> Result<(), String> {
        let ctx = CoreExtensionContext { tx: self.tx.clone(), strategies: self.strategy_names() };
        self.extensions.enable(name, &ctx).await
    }

    /// Disable an extension (runs `on_unload`).
    pub async fn disable_extension(&mut self, name: &str) -> Result<(), String> {
        self.extensions.disable(name).await
    }

    /// Broadcast a kernel event to every enabled extension (isolated per ext).
    pub async fn dispatch_to_extensions(&self, event: &Event) {
        self.extensions.dispatch(event).await;
    }

    /// Recent closed trades (for the UI). Reads the persisted log when present,
    /// else falls back to the in-memory closed positions.
    pub fn recent_trades(&self, limit: usize) -> Vec<serde_json::Value> {
        if let Some(path) = self.config.trade_log_path.as_ref() {
            let rows = crate::trade_db::read_recent(std::path::Path::new(path), limit);
            if !rows.is_empty() {
                return rows;
            }
        }
        let closed = self.positions.closed_positions();
        let start = if limit > 0 && closed.len() > limit { closed.len() - limit } else { 0 };
        closed[start..]
            .iter()
            .map(|c| serde_json::to_value(crate::trade_db::TradeRecord::from_closed(c)).unwrap_or(serde_json::Value::Null))
            .collect()
    }

    // ── Shadow Evolution (opt-in) ───────────────────────────────────────────
    pub fn shadow_evolution(&self) -> &ShadowEvolution {
        &self.shadow_evolution
    }

    /// Operator override: validate + hot-swap mutable parameters.
    pub fn shadow_evolution_apply(&mut self, params: MutableParams, now_ms: i64) -> Result<(), String> {
        self.shadow_evolution.apply_params(params, now_ms)
    }

    /// Attach the evolution hot-swap handle to the driving engine so parameter
    /// changes take effect on the next tick.
    fn rewire_hot_params(&mut self) {
        if let Some(e) = self.engine.as_mut() {
            e.set_hot_params(self.shadow_evolution.handle());
        }
    }

    pub fn shadow_evolution_enable(&mut self, now_ms: i64) -> bool {
        self.shadow_evolution.enable(now_ms);
        self.rewire_hot_params();
        true
    }

    pub fn shadow_evolution_disable(&mut self) -> bool {
        self.shadow_evolution.disable();
        true
    }

    pub fn shadow_evolution_status(&self, now_ms: i64) -> EvolutionStatus {
        self.shadow_evolution.status(now_ms)
    }

    pub fn shadow_evolution_rollback(&mut self, now_ms: i64) -> Result<EvolutionOutcome, String> {
        self.shadow_evolution.rollback(now_ms)
    }

    pub fn shadow_evolution_variants(&mut self, now_ms: i64) -> Vec<crate::shadow_evolution::VariantView> {
        self.shadow_evolution.variant_views(now_ms)
    }

    pub fn shadow_evolution_history(&self, limit: usize) -> Vec<crate::shadow_evolution::audit::AuditRecord> {
        self.shadow_evolution.history(limit)
    }

    /// Feed the evolution engine a round's markets (sets token expiries).
    pub fn shadow_evolution_on_round(&mut self, markets: &[CryptoMarket], now_ms: i64) {
        self.shadow_evolution.on_round(markets, now_ms);
    }

    /// Feed the evolution engine a book tick (observation only). `confirmed`
    /// comes from the same trend tracker the live engine uses.
    pub fn shadow_evolution_on_tick(
        &mut self,
        token_id: &str,
        book: &crate::model::OrderbookSnapshot,
        confirmed: bool,
        now_ms: i64,
    ) {
        self.shadow_evolution.on_tick(token_id, book, confirmed, now_ms);
    }

    /// Run one evolution evaluation and map outcomes to kernel events.
    pub fn shadow_evolution_evaluate(&mut self, now_ms: i64) {
        if !self.shadow_evolution.is_enabled() {
            return;
        }
        for outcome in self.shadow_evolution.evaluate(now_ms) {
            self.emit_shadow_outcome(outcome);
        }
    }

    fn emit_shadow_outcome(&self, outcome: EvolutionOutcome) {
        match outcome {
            EvolutionOutcome::Signal(sig) => self.emit(Event::EvolutionSignal { signal: sig }),
            EvolutionOutcome::Applied(sig) => self.emit(Event::EvolutionApplied { signal: sig }),
            EvolutionOutcome::Rejected { signal, reason } => {
                self.emit(Event::EvolutionRejected { signal, reason })
            }
            EvolutionOutcome::RolledBack { .. } => {
                // Rollback is surfaced via the status/history IPC; the audit log
                // already records it. Emitting an applied event keeps the UI in sync.
                self.emit(Event::EvolutionApplied {
                    signal: crate::shadow_evolution::EvolveSignal::new(
                        "rollback".into(),
                        0,
                        MutableParams::default(),
                        MutableParams::default(),
                        crate::shadow_evolution::EvolutionReason::CombinedImprovement,
                        rust_decimal::Decimal::ONE,
                        0,
                        rust_decimal::Decimal::ZERO,
                        "rollback".into(),
                    ),
                })
            }
        }
    }

    // ── Self-driving engine (P3) ────────────────────────────────────────────
    pub fn enable_engine(&mut self, engine: crate::engine::Engine) {
        self.engine = Some(engine);
    }
    pub fn has_engine(&self) -> bool {
        self.engine.is_some()
    }

    /// Install the running data feed's subscription control (P4). Called by the
    /// market plugin when its `DataFeed` starts, so the core stays unaware of
    /// any concrete feed implementation.
    pub fn set_subscription(&mut self, control: std::sync::Arc<dyn blitzkrieg_market_api::SubscriptionControl>) {
        self.feed = Some(control);
    }

    /// Subscribe the round's tokens on the running orderbook feed (P4).
    /// No-op when feeds are disabled.
    pub async fn subscribe_feed_tokens(&self, tokens: Vec<String>) {
        if let Some(feed) = &self.feed {
            if !tokens.is_empty() {
                feed.set_tokens(tokens);
            }
        }
    }

    /// Feed a market-data event to the engine; applies any trend-break bid
    /// cancellation and, for round updates, nothing else.
    pub fn engine_on_data(&mut self, ev: crate::engine::DataEvent, now_ms: i64) {
        // Archive the raw event BEFORE anything consumes it, so a replay sees
        // exactly the stream the engine saw (P-1.3). The event's own timestamp is
        // authoritative (it is the clock the engine decides on); the caller's
        // `now_ms` is only a fallback for an event that arrived unstamped.
        if let Some(a) = self.event_archive.as_mut() {
            let own = crate::data_source::event_at_ms(&ev);
            let at = if own > 0 { own } else { now_ms };
            a.record_at(at, &ev);
        }
        match &ev {
            crate::engine::DataEvent::Book { .. } => self.stats.books += 1,
            crate::engine::DataEvent::TopOfBook { .. } => self.stats.tops += 1,
            crate::engine::DataEvent::Spot { .. } => self.stats.spots += 1,
            crate::engine::DataEvent::RoundMarkets { .. } => self.stats.rounds += 1,
        }

        // Mirror the latest book into `self.books`, which the position/exit path
        // reads (`run_exit_checks`). Without this, a Rust-native feed (--feed-ws)
        // left Core.books empty, so open positions never re-valued and NO
        // non-forced exit (TP/trailing/SL) could ever fire.
        match &ev {
            crate::engine::DataEvent::Book { token_id, bids, asks, .. } => {
                self.books.insert(
                    token_id.clone(),
                    crate::sim::Book { bids: bids.clone(), asks: asks.clone() },
                );
            }
            crate::engine::DataEvent::TopOfBook { token_id, best_bid, best_ask, .. } => {
                let mut bids = Vec::new();
                let mut asks = Vec::new();
                if let Some(b) = best_bid {
                    if *b > Decimal::ZERO {
                        bids.push((*b, Decimal::ONE));
                    }
                }
                if let Some(a) = best_ask {
                    if *a > Decimal::ZERO {
                        asks.push((*a, Decimal::ONE));
                    }
                }
                // Only overwrite when we actually have a side (don't clobber a
                // full book with an empty top-of-book update).
                if !bids.is_empty() || !asks.is_empty() {
                    self.books.insert(token_id.clone(), crate::sim::Book { bids, asks });
                }
            }
            _ => {}
        }

        // Shadow Evolution observation. D-3 fidelity: the shadow is fed AFTER the
        // live engine consumes this very event, so a variant replays the SAME tick
        // instant — same book and same trend confirmation — the live strategy
        // decided on. Feeding it before `engine.on_data` (the old behaviour) gave
        // every variant the PREVIOUS book, a one-tick lag that mis-timed entries
        // and diluted the counterfactual. Capture what we need before `ev` moves.
        enum ShadowFeed {
            Book(String, i64),
            Round(Vec<CryptoMarket>, i64),
        }
        let shadow_feed = if self.shadow_evolution.is_enabled() {
            match &ev {
                crate::engine::DataEvent::Book { token_id, now_ms: t, .. }
                | crate::engine::DataEvent::TopOfBook { token_id, now_ms: t, .. } => {
                    Some(ShadowFeed::Book(token_id.clone(), *t))
                }
                crate::engine::DataEvent::RoundMarkets { markets, now_ms: t } => {
                    Some(ShadowFeed::Round(markets.clone(), *t))
                }
                _ => None,
            }
        } else {
            None
        };

        // Drive the live engine (when attached). Absence must not skip the shadow
        // round feed below, so this is an `if let`, not an early return.
        if let Some(engine) = self.engine.as_mut() {
            let broken = engine.on_data(ev);
            for (token, _price) in broken {
                // Regime change: pull resting entry bids for this token.
                let ids: Vec<String> = self
                    .ome
                    .live_orders()
                    .into_iter()
                    .filter(|o| o.side == Side::Buy && o.token_id == token)
                    .map(|o| o.order_id.clone())
                    .collect();
                for id in ids {
                    let _ = self.cancel(&id, now_ms);
                }
            }
        }

        // Now feed the shadow the tick the live engine just consumed.
        match shadow_feed {
            Some(ShadowFeed::Book(token_id, t)) => {
                let confirmed = self
                    .engine
                    .as_ref()
                    .map(|e| e.confirmed_tokens().contains(&token_id))
                    .unwrap_or(false);
                let book = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.book_snapshot(&token_id));
                if let Some(book) = book {
                    self.shadow_evolution_on_tick(&token_id, &book, confirmed, t);
                }
            }
            Some(ShadowFeed::Round(markets, t)) => self.shadow_evolution_on_round(&markets, t),
            None => {}
        }
    }

    /// Run one evaluation cycle: engine produces entry orders, core places them.
    /// Entries go through the usual risk/capacity gates; rejections are skipped.
    /// A configured per-strategy cap (P-1.1) is checked before the order layer.
    pub fn engine_evaluate(&mut self, now_ms: i64) -> usize {
        if self.engine.is_none() {
            return 0;
        }
        self.stats.evaluations += 1;
        // Shadow Evolution: evaluate FIRST so any applied hot-swap is in force
        // for this very cycle's order decision (next-tick semantics, no restart).
        self.shadow_evolution_evaluate(now_ms);
        let engine = self.engine.as_mut().expect("engine present");
        let orders = engine.evaluate(now_ms);
        // Close intents produced by strategies this cycle join the SAME exit
        // submission path as policy exits (handled in run_exit_checks).
        self.strategy_exits.extend(engine.drain_strategy_exits());
        engine.tally_blocked();
        self.stats.signals += orders.len() as u64;
        let tokens: Vec<(String, crate::model::OrderRequest)> =
            orders.into_iter().map(|o| (o.token_id.clone(), o)).collect();
        let mut placed = 0;
        for (token, req) in tokens {
            let name = req.strategy.clone();
            if let Some(limit) = self.config.strategy_limits.get(&name).cloned() {
                if self.strategy_limit_ok(&req, &limit).is_err() {
                    self.stats.strategy_limit_rejected += 1;
                    self.strategy_accounting.entry(name).or_default().limit_rejected += 1;
                    continue;
                }
            }
            match self.place(req, self.config.entry_maker_timeout_ms, now_ms) {
                Ok((_id, _)) => {
                    self.strategy_accounting.entry(name).or_default().placed += 1;
                    if let Some(engine) = self.engine.as_mut() {
                        engine.note_order_placed(&token);
                    }
                    placed += 1;
                }
                Err(_) => {
                    self.stats.place_rejected += 1;
                    self.strategy_accounting.entry(name).or_default().rejected += 1;
                }
            }
        }
        placed
    }

    /// Check a configured per-strategy entry cap against the strategy's current
    /// open exposure. Returns the rejection reason when a cap would be exceeded.
    fn strategy_limit_ok(
        &self,
        req: &crate::model::OrderRequest,
        limit: &StrategyLimit,
    ) -> Result<(), String> {
        let open: Vec<&crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == req.strategy)
            .collect();
        if let Some(max_pos) = limit.max_open_positions {
            if open.len() >= max_pos {
                return Err(format!(
                    "strategy {} at position cap ({}/{})",
                    req.strategy,
                    open.len(),
                    max_pos
                ));
            }
        }
        if let Some(max_notional) = limit.max_open_notional_usd {
            let used: Decimal = open.iter().map(|p| p.cost_usd).sum();
            let incoming = req.price * req.size;
            if used + incoming > max_notional {
                return Err(format!(
                    "strategy {} notional cap ({used}+{incoming} > {max_notional})",
                    req.strategy
                ));
            }
        }
        Ok(())
    }

    /// Per-strategy accounting (P-1.1): open exposure (live) plus session order
    /// counters and realized PnL per registered strategy.
    pub fn strategy_stats(&self) -> Vec<serde_json::Value> {
        let enabled = self.enabled_strategy_names();
        self.strategy_names()
            .into_iter()
            .map(|name| {
                let acc = self.strategy_accounting.get(&name).cloned().unwrap_or_default();
                let opens: Vec<&crate::position::OpenPosition> = self
                    .positions
                    .open_positions()
                    .iter()
                    .filter(|p| p.strategy == name)
                    .collect();
                let open_notional: Decimal = opens.iter().map(|p| p.cost_usd).sum();
                let source = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.strategy_source(&name))
                    .unwrap_or("")
                    .to_string();
                // E2-a: the sizing actually in force for this strategy (its own
                // override clamped by the globals, else the globals) plus the
                // configured caps, so the report shows both quota and occupancy.
                let effective = self
                    .engine
                    .as_ref()
                    .map(|e| e.effective_sizing(&name))
                    .unwrap_or(crate::engine::EffectiveSizing {
                        size_usd: self.config.size_usd,
                        min_shares: self.config.min_shares,
                        max_shares: self.config.max_shares,
                        strategy_scoped: false,
                    });
                let limit = self.config.strategy_limits.get(&name);
                serde_json::json!({
                    "name": name,
                    "enabled": enabled.contains(&name),
                    "source": source,
                    "openPositions": opens.len(),
                    "openNotionalUsd": dec_json(open_notional),
                    // ── E2-a quota + effective sizing ──
                    "maxOpenPositions": limit.and_then(|l| l.max_open_positions),
                    "maxOpenNotionalUsd": limit
                        .and_then(|l| l.max_open_notional_usd)
                        .map(dec_json),
                    "sizingSource": if effective.strategy_scoped { "strategy" } else { "global" },
                    "effectiveSizeUsd": dec_json(effective.size_usd),
                    "effectiveMinShares": dec_json(effective.min_shares),
                    "effectiveMaxShares": dec_json(effective.max_shares),
                    "ordersPlaced": acc.placed,
                    "ordersRejected": acc.rejected,
                    "limitRejected": acc.limit_rejected,
                    "closedTrades": acc.closed_trades,
                    "wins": acc.wins,
                    "losses": acc.losses,
                    "feesUsd": dec_json(acc.fees_usd),
                    "netPnlUsd": dec_json(acc.net_pnl_usd),
                })
            })
            .collect()
    }

    /// Diagnostic snapshot: feed counters + engine trend/confirmed state.
    pub fn engine_stats(&self) -> serde_json::Value {
        self.engine_stats_at(now_ms())
    }

    /// The same snapshot as of an explicit instant. The token lists are sorted:
    /// their source is a `HashSet`, whose iteration order is randomised per
    /// process, and a report that shuffles between runs cannot be diffed.
    pub fn engine_stats_at(&self, as_of_ms: i64) -> serde_json::Value {
        let mut confirmed = self
            .engine
            .as_ref()
            .map(|e| e.confirmed_tokens().into_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        confirmed.sort();
        let mut confirmed_detail = self
            .engine
            .as_ref()
            .map(|e| e.confirmed_diagnostics(as_of_ms))
            .unwrap_or_default();
        confirmed_detail.sort_by(|a, b| a["token"].as_str().cmp(&b["token"].as_str()));
        let blocked = self
            .engine
            .as_ref()
            .map(|e| {
                serde_json::json!({
                    "timing": e.blocked_timing_count(),
                    "momentum": e.blocked_momentum_count(),
                })
            })
            .unwrap_or(serde_json::Value::Null);
        serde_json::json!({
            "books": self.stats.books,
            "tops": self.stats.tops,
            "spots": self.stats.spots,
            "rounds": self.stats.rounds,
            "evaluations": self.stats.evaluations,
            "signals": self.stats.signals,
            "placeRejected": self.stats.place_rejected,
            "strategyLimitRejected": self.stats.strategy_limit_rejected,
            "blocked": blocked,
            "confirmed": confirmed,
            "confirmedDetail": confirmed_detail,
            "strategies": self.strategy_stats(),
            "archive": self
                .event_archive
                .as_ref()
                .map(|a| serde_json::to_value(a.status()).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null),
        })
    }

    /// Snapshot the current round for the UI.
    pub fn round_view(&self) -> Option<RoundView> {
        let engine = self.engine.as_ref()?;
        let now = now_ms();
        let r = engine.scanner().round_state(now);
        let can_trade = engine.scanner().can_trade(now).is_ok();
        let market_prices = r
            .markets
            .iter()
            .map(|m| {
                let up = engine
                    .book_snapshot(&m.up_token_id)
                    .map(|b| b.mid_price)
                    .filter(|p| *p > Decimal::ZERO)
                    .unwrap_or(m.up_price);
                let down = engine
                    .book_snapshot(&m.down_token_id)
                    .map(|b| b.mid_price)
                    .filter(|p| *p > Decimal::ZERO)
                    .unwrap_or(m.down_price);
                MarketPriceView { asset: m.asset.clone(), up, down }
            })
            .collect();
        Some(RoundView {
            slot: r.slot,
            age_sec: r.age_sec,
            time_left_sec: r.time_left_sec,
            markets: r.markets.len(),
            can_trade,
            market_prices,
        })
    }

    /// Open positions enriched for the UI (unrealised PnL uses the current price
    /// we track; the book tick updates it on every check).
    pub fn position_views(&self, now_ms: i64) -> Vec<crate::ipc::schema::PositionView> {
        use crate::exit_policy::pnl_pct;
        self.positions
            .open_positions()
            .iter()
            .map(|p| crate::ipc::schema::PositionView {
                id: p.id.clone(),
                asset: p.asset.clone(),
                direction: p.direction.as_str().to_string(),
                strategy: p.strategy.clone(),
                token_id: p.token_id.clone(),
                entry_price: p.entry_price,
                current_price: p.current_price,
                shares: p.shares,
                unrealized_pct: pnl_pct(p.current_price, p.entry_price),
                high_pnl_pct: p.high_pnl_pct(),
                was_maker_entry: p.was_maker_entry,
                entered_at_ms: p.entered_at_ms,
                expires_at_ms: p.expires_at_ms,
                remaining_sec: (p.expires_at_ms - now_ms) / 1000,
            })
            .collect()
    }
    pub fn list_orders(&self) -> Vec<TrackedOrder> {
        self.ome.all().into_iter().cloned().collect()
    }

    // ── Live venue entry points ─────────────────────────────────────────────
    // In DRY the core synthesises fills itself. In LIVE the CLOB adapter drives
    // these same OME/ledger paths from venue acknowledgements and the user-WS
    // stream, so dry and live share one authoritative state machine.

    /// Reserve/risk-gate/register an order as Pending without simulating a fill.
    /// Used by the live adapter before the async CLOB POST resolves.
    pub fn place_pending(
        &mut self,
        req: OrderRequest,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        self.risk.check(&req)?;
        let id = self.new_order_id();
        if req.side == Side::Buy {
            self.ledger.reserve(&id, req.price * req.size)?;
        }
        self.ome.submit(SubmitParams { order_id: id.clone(), request: req, submitted_at_ms: now_ms })?;
        self.emit_order(&id);
        Ok((id, OrderStatus::Pending))
    }

    /// Venue acknowledged the order (resting live). Optionally map to its venue id.
    pub fn confirm_live(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        if self.ome.get(id).is_some() {
            self.ome.mark_live(id, now_ms)?;
            // Release reservation only on fill/cancel; keep it while resting.
            self.emit_order(id);
        }
        Ok(())
    }

    /// Venue rejected/killed an order before acceptance: release any reservation.
    pub fn reject_live(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        if let Some(o) = self.ome.get(id).cloned() {
            if o.side == Side::Buy {
                self.ledger.release(id);
            }
            self.ome.mark_terminal(id, OrderStatus::Rejected, now_ms)?;
            self.emit_order(id);
        }
        Ok(())
    }

    /// Apply an authoritative venue/user-WS fill (cumulative size) through the
    /// same idempotent ledger + OME path the dry matcher uses, then project the
    /// resulting delta onto the position book.
    pub fn ingest_fill(&mut self, fill: Fill, now_ms: i64) -> CoreResult<()> {
        if let Some(d) = self.ome.apply_fill(fill, now_ms)? {
            self.apply_delta_effects(d.clone(), now_ms);
        }
        Ok(())
    }

    /// Ledger + position projection for a canonical fill delta. Single choke
    /// point shared by dry fills, live user-WS fills and reconciliation gaps.
    fn apply_delta_effects(&mut self, d: FillDelta, now_ms: i64) {
        let px = d.price;
        match d.side {
            Side::Buy => self.ledger.settle_buy_fill(&d.order_id, px * d.delta),
            Side::Sell => self.ledger.settle_sell_fill(px * d.delta),
        }
        self.project_fill_delta(&d, now_ms);
        self.emit_fill(d);
    }

    /// Keep the position book in sync with fills: BUY opens/averages-in, SELL
    /// reduces/closes. One position per token, so token_id identifies it.
    fn project_fill_delta(&mut self, d: &FillDelta, now_ms: i64) {
        let token = &d.token_id;
        match d.side {
            Side::Buy => {
                if let Some(pos) = self.positions.open_positions().iter().find(|p| &p.token_id == token) {
                    let id = pos.id.clone();
                    let (old_shares, old_entry) = (pos.shares, pos.entry_price);
                    let add = d.delta;
                    let new_shares = old_shares + add;
                    if new_shares > Decimal::ZERO {
                        let new_entry = ((old_entry * old_shares) + (d.price * add)) / new_shares;
                        // Mutate through the manager API (kept minimal: adjust
                        // via close-with-avg is not applicable, so re-open path).
                        self.positions.adjust_open(&id, new_entry, new_shares);
                        self.persist_positions();
                    }
                } else {
                    let direction = parse_direction(&d.direction);
                    // A round slot N covers [N*dur, (N+1)*dur): the market expires
                    // at the END of the slot. (Using N*dur would place expiry in the
                    // past and force-exit the position immediately.)
                    let expires_at_ms = if d.round_slot > 0 {
                        (d.round_slot + 1) * self.config.round_duration_sec * 1000
                    } else {
                        now_ms + self.config.round_duration_sec * 1000
                    };
                    let p = OpenParams {
                        strategy: d.strategy.clone(),
                        asset: d.asset.clone(),
                        direction,
                        token_id: d.token_id.clone(),
                        condition_id: d.condition_id.clone(),
                        entry_price: d.price,
                        shares: d.delta,
                        expires_at_ms,
                        was_maker: d.mode == FillPolicy::Maker || d.mode == FillPolicy::MakerThenTaker,
                        target_exit_price: None,
                    };
                    self.positions.open(p, now_ms);
                    self.persist_positions();
                }
            }
            Side::Sell => {
                let found = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| &p.token_id == token)
                    .map(|p| (p.id.clone(), p.shares, p.entry_price));
                if let Some((id, shares, entry)) = found {
                    let remaining = shares - d.delta;
                    if remaining <= Decimal::new(1, 2) {
                        // Fully closed: use the recorded exit reason when this
                        // SELL was produced by the exit engine, else Manual.
                        let reason = self.exit_reasons.remove(token).unwrap_or(ExitReason::Manual);
                        if let Some(closed) = self.positions.close(&id, d.price, reason, false, now_ms) {
                            self.persist_positions();
                            self.on_position_closed(&closed, now_ms);
                        }
                    } else {
                        let new_cost = entry * remaining;
                        self.positions.adjust_open(&id, entry, remaining);
                        self.persist_positions();
                        let _ = new_cost;
                    }
                }
            }
        }
    }

    /// Breaker + event emission when a position closes.
    fn on_position_closed(&mut self, closed: &crate::position::ClosedPosition, now_ms: i64) {
        // Persist first so the panel/analysis have the record even if a later
        // step fails (matches the Node behaviour of saving on close).
        if let Some(db) = self.trade_db.as_mut() {
            let rec = crate::trade_db::TradeRecord::from_closed(closed);
            db.record(&rec, now_ms);
        }
        // Per-strategy accounting (P-1.1): session realized PnL + fee totals.
        {
            let entry_fee =
                (closed.entry_fee_pct / Decimal::ONE_HUNDRED) * closed.entry_price * closed.shares;
            let exit_fee =
                (closed.exit_fee_pct / Decimal::ONE_HUNDRED) * closed.exit_price * closed.shares;
            let acc = self.strategy_accounting.entry(closed.strategy.clone()).or_default();
            acc.closed_trades += 1;
            if closed.net_pnl_usd >= Decimal::ZERO {
                acc.wins += 1;
            } else {
                acc.losses += 1;
            }
            acc.fees_usd += entry_fee + exit_fee;
            acc.net_pnl_usd += closed.net_pnl_usd;
        }
        let tripped = self.breaker.record(closed.net_pnl_usd, now_ms);
        self.emit(Event::PositionClosed {
            id: closed.id.clone(),
            asset: closed.asset.clone(),
            direction: closed.direction.as_str().to_string(),
            reason: format!("{:?}", closed.exit_reason),
            net_pnl_usd: closed.net_pnl_usd,
            net_pnl_pct: closed.net_pnl_pct,
            daily_pnl_usd: self.positions.daily_pnl(),
        });
        if tripped {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::RiskRejected,
                message: format!(
                    "consecutive-loss breaker tripped: {} losses, halting new entries {}s",
                    self.breaker.consecutive_losses(),
                    self.config.breaker_cooldown_sec
                ),
            });
        }
    }

    /// Map a venue order id to a core order (for user-WS events). Returns the
    /// core id when known.
    pub fn core_id_for_venue(&self, venue_or_core: &str) -> Option<String> {
        self.ome.by_venue_or_id(venue_or_core).map(|o| o.order_id.clone())
    }

    /// Live orders accepted locally but not yet submitted to the venue (no
    /// venue id bound). The live bridge submits these each tick.
    pub fn pending_unbound(&self) -> Vec<TrackedOrder> {
        self.ome
            .live_orders()
            .into_iter()
            .filter(|o| o.venue_order_id.is_none())
            .cloned()
            .collect()
    }

    /// Attach the venue id returned by a successful LIVE POST and mark it live.
    pub fn bind_venue(&mut self, core_id: &str, venue_order_id: String, now_ms: i64) -> CoreResult<()> {
        self.ome.bind_venue(core_id, venue_order_id, now_ms)?;
        self.ome.mark_live(core_id, now_ms)?;
        self.emit_order(core_id);
        Ok(())
    }

    /// Run a reconciliation sweep against a venue snapshot. Applies missed fills
    /// and repairs ghost/partial orders, emitting a report event.
    pub fn reconcile(&mut self, snap: crate::reconcile::VenueSnapshot) -> CoreResult<crate::reconcile::ReconcileReport> {
        let report = crate::reconcile::reconcile(&mut self.ome, &snap)?;
        // Ledger + position effects for any gap fills the OME just applied.
        for gap in report.actions.iter().filter_map(|a| match a {
            crate::reconcile::ReconcileAction::FilledGap { delta, .. } => Some(delta.clone()),
            _ => None,
        }) {
            self.apply_delta_effects(gap, snap.now_ms);
        }
        if !report.actions.is_empty() || !report.suspect_ghost_ids.is_empty() {
            self.emit(Event::ReconcileReport {
                filled: report
                    .actions
                    .iter()
                    .filter(|a| matches!(a, crate::reconcile::ReconcileAction::FilledGap { .. }))
                    .count(),
                marked_filled: report
                    .actions
                    .iter()
                    .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedFilled { .. }))
                    .count(),
                marked_cancelled: report
                    .actions
                    .iter()
                    .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedCancelled { .. }))
                    .count(),
                ghost_ids: report.suspect_ghost_ids.clone(),
            });
        }
        Ok(report)
    }

    fn emit(&self, ev: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }
    fn emit_order(&self, id: &str) {
        if let Some(o) = self.ome.get(id) {
            // Durable persistence: every status change flows through here (submit /
            // confirm / reject / bind / cancel), so the order log always reflects
            // the latest state and a restart can rebuild it (orphan guard).
            self.persist_order(id);
            self.emit(Event::OrderUpdate { order: o.clone() });
        }
    }
    fn emit_fill(&self, d: FillDelta) {
        if let Some(o) = self.ome.get(&d.order_id) {
            self.persist_order(&d.order_id);
            self.emit(Event::Fill { delta: d.into(), order: o.clone() });
        }
    }

    fn new_order_id(&mut self) -> String {
        let prefix = if self.config.mode == Mode::Dry { "dry" } else { "live" };
        let id = format!("{prefix}_{}", self.next_id);
        self.next_id += 1;
        id
    }

    // ── Risk ───────────────────────────────────────────────────────────────
    pub fn kill(&mut self, reason: String) {
        self.risk.kill(reason.clone());
        self.emit(Event::RiskAlert { code: CoreErrorCode::KillSwitchActive, message: reason });
    }
    /// Flush pending near-miss records to disk (call on shutdown).
    pub fn flush_near_misses(&mut self) {
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.flush_near_misses())
                .unwrap_or_default();
            if !recs.is_empty() {
                if let Err(e) = crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs) {
                    tracing::warn!(error = %e, path = %path, "near-miss flush failed");
                }
            }
        }
    }

    pub fn resume(&mut self) {
        self.risk.resume();
    }
    pub fn is_killed(&self) -> bool {
        self.risk.is_killed()
    }
    /// Broadcast a structured error to Node (never swallowed).
    pub fn emit_error(&self, e: CoreError) {
        self.emit(Event::Error { error: e });
    }
    /// Broadcast a risk alert.
    pub fn emit_risk_alert(&self, code: CoreErrorCode, message: String) {
        self.emit(Event::RiskAlert { code, message });
    }

    // ── Orders ─────────────────────────────────────────────────────────────
    pub fn place(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        self.risk.check(&req)?;

        // Entry gates apply to opening BUY orders only; exits (SELL) are never
        // blocked by capacity, breaker or cooldowns.
        if req.side == Side::Buy {
            if self.breaker.is_halted(now_ms) {
                return Err(CoreError::new(
                    CoreErrorCode::RiskRejected,
                    format!("breaker active until {}", self.breaker.halted_until_ms()),
                ));
            }
            let direction = parse_direction(&req.direction);
            if let Err(reason) = self.positions.can_open(Some(&req.asset), Some(direction), now_ms) {
                return Err(CoreError::new(CoreErrorCode::RiskRejected, reason));
            }
        }

        let id = self.new_order_id();

        // Reserve BUY notional up front (prevents over-commitment).
        if req.side == Side::Buy {
            self.ledger.reserve(&id, req.price * req.size)?;
        }

        self.ome.submit(SubmitParams { order_id: id.clone(), request: req, submitted_at_ms: now_ms })?;
        self.place_after_submit(&id, maker_timeout_ms, now_ms)?;

        let status = self.ome.get(&id).map(|o| o.status).unwrap_or(OrderStatus::Pending);
        self.emit_order(&id);
        Ok((id, status))
    }

    fn place_after_submit(&mut self, id: &str, maker_timeout_ms: i64, now_ms: i64) -> CoreResult<()> {
        let order = self
            .ome
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::new(CoreErrorCode::Internal, "order missing after submit"))?;

        match self.config.mode {
            Mode::Dry => {
                self.ome.mark_live(id, now_ms)?;
                match order.mode {
                    FillPolicy::Taker => {
                        // Cross immediately and fully at the buffered limit,
                        // worsened by the fill model's taker slippage (identity by
                        // default, so the live/dry path is unchanged).
                        let fill_price = self.config.fill_model.apply_slippage(order.side, order.price);
                        self.authoritative_fill(id, order.size, fill_price, now_ms)?;
                    }
                    FillPolicy::Maker | FillPolicy::MakerThenTaker => {
                        if order.mode == FillPolicy::MakerThenTaker {
                            let timeout = if maker_timeout_ms > 0 {
                                maker_timeout_ms
                            } else {
                                self.config.default_maker_timeout_ms
                            };
                            self.ome.set_escalation(id, now_ms + timeout)?;
                        }
                        // Fill now if the current book already crosses.
                        self.try_maker_fill(id, now_ms);
                    }
                }
            }
            Mode::Live => {
                // Pending until the venue adapter confirms; the async layer calls
                // mark_live / feeds user-WS fills. Implemented with the CLOB adapter.
            }
        }
        Ok(())
    }

    /// Apply one authoritative (cumulative) fill and its ledger effect.
    fn authoritative_fill(
        &mut self,
        id: &str,
        cumulative: Decimal,
        price: Decimal,
        now_ms: i64,
    ) -> CoreResult<()> {
        let (token, side) = match self.ome.get(id) {
            Some(o) => (o.token_id.clone(), o.side),
            None => return Err(CoreError::new(CoreErrorCode::UnknownOrder, id)),
        };
        let fill = Fill {
            order_id: id.into(),
            trade_id: Some(format!("{id}:{now_ms}")),
            token_id: token,
            side,
            price,
            size: cumulative,
            status: FillStatus::Confirmed,
            ts_ms: now_ms,
            tx_hash: None,
        };
        if let Some(d) = self.ome.apply_fill(fill, now_ms)? {
            self.apply_delta_effects(d, now_ms);
        }
        Ok(())
    }

    /// Fill a resting maker order if the latest book crosses its limit.
    fn try_maker_fill(&mut self, id: &str, now_ms: i64) {
        let Some(order) = self.ome.get(id).cloned() else { return };
        if !order.status.is_live() || order.filled_size >= order.size {
            return;
        }
        let crosses = self.books.get(&order.token_id).map(|b| b.crosses(&order)).unwrap_or(false);
        if !crosses {
            return;
        }
        // Fill model (P-1.2), identity by default so this path is unchanged:
        //  - latency: the order cannot be hit before the venue could have it;
        //  - fill probability: a crossing does not guarantee a fill (queue
        //    position) — a deterministic per-order draw keeps replays exact.
        let model = self.config.fill_model;
        if now_ms < model.maker_eligible_at_ms(order.submitted_at_ms) {
            return;
        }
        if !model.maker_fill_wins(&order.order_id) {
            return;
        }
        // Dry maker fills are full fills at the resting limit (Node parity).
        if let Err(e) = self.authoritative_fill(id, order.size, order.price, now_ms) {
            self.emit(Event::Error { error: e });
        }
    }

    pub fn cancel(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        let order = self
            .ome
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, id))?;
        if order.side == Side::Buy {
            self.ledger.release(id);
        }
        self.ome.mark_terminal(id, OrderStatus::Cancelled, now_ms)?;
        self.emit_order(id);
        Ok(())
    }

    pub fn cancel_all(&mut self, token_id: Option<&str>, now_ms: i64) -> CoreResult<usize> {
        let ids: Vec<String> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| token_id.map(|t| o.token_id == t).unwrap_or(true))
            .map(|o| o.order_id.clone())
            .collect();
        for id in &ids {
            self.cancel(id, now_ms)?;
        }
        Ok(ids.len())
    }

    /// Force-close positions: a specific id, or all when id is None. Returns how
    /// many were closed. Exits are SELL orders; in dry mode they cross at the
    /// latest book bid, in live mode the venue bridge submits them.
    pub fn flatten(&mut self, position_id: Option<&str>, now_ms: i64) -> CoreResult<usize> {
        let targets: Vec<(String, String, String, Decimal, String, String, String, Decimal)> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| position_id.map(|id| p.id == id).unwrap_or(true))
            .map(|p| {
                (
                    p.id.clone(),
                    p.token_id.clone(),
                    p.condition_id.clone(),
                    p.shares,
                    p.strategy.clone(),
                    p.asset.clone(),
                    p.direction.as_str().to_string(),
                    p.current_price,
                )
            })
            .collect();
        let mut closed = 0usize;
        for (id, token, condition, shares, strategy, asset, direction, current) in targets {
            // Already have a live sell for this token? skip.
            if self.ome.live_for(&token, Side::Sell).into_iter().any(|o| o.status.is_live()) {
                continue;
            }
            let price = if current > Decimal::ZERO { current } else { Decimal::new(1, 2) };
            self.exit_reasons.insert(token.clone(), ExitReason::Manual);
            let order = OrderRequest {
                token_id: token,
                condition_id: condition,
                side: Side::Sell,
                mode: FillPolicy::Taker,
                price,
                size: shares,
                internal_key: format!("flatten:{id}"),
                strategy,
                asset,
                direction,
                round_slot: 0,
            };
            if let Err(e) = self.place(order, 0, now_ms) {
                self.emit_error(e);
            } else {
                closed += 1;
            }
        }
        Ok(closed)
    }

    // ── Market data (P0 bridge from Node; P3 ingests inside the core) ───────
    pub fn book_snapshot(
        &mut self,
        token_id: &str,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        now_ms: i64,
    ) {
        self.books.insert(token_id.to_string(), Book { bids, asks });
        let ids: Vec<String> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.token_id == token_id && rests_on_book(o.mode))
            .map(|o| o.order_id.clone())
            .collect();
        for id in ids {
            self.try_maker_fill(&id, now_ms);
        }
    }

    // ── Maintenance: pending-fill retry + maker→taker escalation + exits ────
    pub fn tick(&mut self, now_ms: i64) -> CoreResult<()> {
        // Flush the market-data archive at most once a second, so a reader of the
        // live file stays close behind without paying a write syscall every tick
        // (P-1.3).
        if let Some(a) = self.event_archive.as_mut() {
            a.flush_if_due(now_ms, 1000);
        }

        // Retry buffered fills (orders registered since the event arrived).
        let pending = self.ome.drain_pending(now_ms)?;
        for d in pending {
            self.apply_delta_effects(d, now_ms);
        }

        // Resume entries when the breaker cooldown elapses.
        if self.breaker.maybe_resume(now_ms) {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::RiskRejected,
                message: "breaker cooldown elapsed — resuming new entries".into(),
            });
        }

        // Persist finalized near-miss records (blocked signals + their paths).
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.take_near_misses(now_ms))
                .unwrap_or_default();
            if !recs.is_empty() {
                if let Err(e) = crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs) {
                    tracing::warn!(error = %e, path = %path, "near-miss persist failed");
                }
            }
        }

        // Evaluate exits for every open position and place a closing SELL.
        self.run_exit_checks(now_ms)?;

        // Escalate due maker_then_taker orders: cancel maker, cross as taker.
        let due: Vec<(String, Decimal, Decimal, Side, String, String, String, String, String, i64)> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.escalate_at_ms.map(|t| t <= now_ms).unwrap_or(false))
            .map(|o| {
                (
                    o.order_id.clone(),
                    o.price,
                    o.size - o.filled_size,
                    o.side,
                    o.token_id.clone(),
                    o.condition_id.clone(),
                    o.strategy.clone(),
                    o.asset.clone(),
                    o.direction.clone(),
                    o.round_slot,
                )
            })
            .collect();
        for (id, price, remaining, side, token, condition, strategy, asset, direction, slot) in due {
            self.cancel(&id, now_ms)?;
            if remaining <= Decimal::ZERO {
                continue;
            }
            let req = OrderRequest {
                token_id: token,
                condition_id: condition,
                side,
                mode: FillPolicy::Taker,
                price,
                size: remaining,
                internal_key: format!("{id}:escalated"),
                strategy,
                asset,
                direction,
                round_slot: slot,
            };
            self.place(req, 0, now_ms)?;
        }
        Ok(())
    }

    /// Check exits and submit a closing SELL for each triggered position.
    ///
    /// Two sources converge on ONE submission loop so both are subject to the
    /// same rules (live-sell dedup, `sell_shares`, risk/ledger/sign in `place`):
    ///  - the kernel's automated exit policy (`check_exits`), suppressed by
    ///    `auto_exits_enabled = false`;
    ///  - explicit strategy close intents (`StrategySignal`), which like a
    ///    manual flatten are honoured even when automated exits are disabled,
    ///    but still cannot bypass the kill switch/risk/dedup/position checks.
    fn run_exit_checks(&mut self, now_ms: i64) -> CoreResult<()> {
        // Consume this cycle's intents up front: an intent on a token with no
        // open position is dropped, never left to fire on a later position.
        let intents = std::mem::take(&mut self.strategy_exits);
        if self.positions.open_positions().is_empty() {
            return Ok(());
        }
        // Always re-value open positions from the latest books so the dashboard's
        // unrealized PnL / HWM move even when automated exits are disabled.
        let books = self.books.clone();
        let book_fn = |token: &str| {
            books.get(token).map(|b| {
                crate::model::OrderbookSnapshot::from_levels(
                    token.to_string(),
                    b.bids.clone(),
                    b.asks.clone(),
                    now_ms,
                )
            })
        };
        self.positions.valuate(&book_fn, now_ms);

        // A unit of closing work resolved against a concrete open position.
        #[derive(Clone)]
        struct ExitJob {
            position_id: String,
            token: String,
            condition_id: String,
            price: Decimal,
            reason: ExitReason,
            use_maker: bool,
            internal_key: String,
            strategy: String,
            asset: String,
            direction: String,
        }

        let mut jobs: Vec<ExitJob> = Vec::new();
        let mut has_job: HashSet<String> = HashSet::new();

        if self.config.auto_exits_enabled {
            for req in self.positions.check_exits(&book_fn, now_ms) {
                let Some(pos) = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| p.id == req.position_id)
                    .cloned()
                else {
                    continue;
                };
                has_job.insert(pos.id.clone());
                jobs.push(ExitJob {
                    position_id: pos.id.clone(),
                    token: pos.token_id.clone(),
                    condition_id: pos.condition_id.clone(),
                    price: req.exit_price,
                    reason: req.reason,
                    use_maker: req.use_maker,
                    internal_key: format!("exit:{}:{:?}", pos.token_id, req.reason),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    direction: pos.direction.as_str().to_string(),
                });
            }
        }

        // Strategy-signal exits: resolve the token to a live position and price
        // off its latest valuation, exactly like a manual flatten.
        for intent in intents {
            let Some(pos) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == intent.token_id)
                .cloned()
            else {
                continue;
            };
            if !has_job.insert(pos.id.clone()) {
                continue; // an automated/policy exit already closes it this cycle
            }
            let price = if pos.current_price > Decimal::ZERO { pos.current_price } else { Decimal::new(1, 2) };
            let tag: String = intent
                .reason
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
                .take(32)
                .collect();
            jobs.push(ExitJob {
                position_id: pos.id.clone(),
                token: pos.token_id.clone(),
                condition_id: pos.condition_id.clone(),
                price,
                reason: ExitReason::StrategySignal,
                use_maker: false,
                internal_key: format!("exit-strategy:{}:{}", pos.token_id, tag),
                strategy: pos.strategy.clone(),
                asset: pos.asset.clone(),
                direction: pos.direction.as_str().to_string(),
            });
        }

        for job in jobs {
            // Skip if a sell order for this position is already live.
            let already_live = self
                .ome
                .live_for(&job.token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live());
            if already_live {
                continue;
            }
            let Some(size) = self.positions.sell_shares(&job.position_id) else { continue };
            let mode = if job.use_maker { FillPolicy::Maker } else { FillPolicy::Taker };
            let order = OrderRequest {
                token_id: job.token.clone(),
                condition_id: job.condition_id,
                side: Side::Sell,
                mode,
                price: job.price,
                size,
                internal_key: job.internal_key,
                strategy: job.strategy,
                asset: job.asset,
                direction: job.direction,
                round_slot: 0,
            };
            // Record the intended exit reason so a full sell fill closes with it.
            self.exit_reasons.insert(job.token.clone(), job.reason);
            if let Err(e) = self.place(order, 0, now_ms) {
                self.emit_error(e);
            }
        }
        Ok(())
    }
}

fn parse_direction(s: &str) -> SignalDirection {
    match s.to_ascii_lowercase().as_str() {
        "down" => SignalDirection::Down,
        _ => SignalDirection::Up,
    }
}

/// The narrowed surface handed to an extension when it is enabled. It carries
/// only an event sink clone and a snapshot of strategy names — deliberately NOT
/// the signer, venue client, UDS socket, order manager or any internal state.
struct CoreExtensionContext {
    tx: Option<mpsc::UnboundedSender<Event>>,
    strategies: Vec<String>,
}

impl crate::extension::ExtensionContext for CoreExtensionContext {
    fn emit(&self, event: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }
    fn strategy_names(&self) -> Vec<String> {
        self.strategies.clone()
    }
    fn log(&self, message: &str) {
        tracing::info!(target: "extension", "{message}");
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Decimal → JSON number for the diagnostics payloads, matching the `crate::decimal`
/// wire convention (Node sees a plain number, not a string).
fn dec_json(d: Decimal) -> serde_json::Value {
    use rust_decimal::prelude::ToPrimitive;
    serde_json::Value::from(d.to_f64().unwrap_or_default())
}

/// Feed/decision counters for P4 observability.
#[derive(Debug, Default, Clone)]
struct CoreStats {
    books: u64,
    tops: u64,
    spots: u64,
    rounds: u64,
    evaluations: u64,
    signals: u64,
    place_rejected: u64,
    /// Entries rejected by a configured per-strategy cap (P-1.1).
    strategy_limit_rejected: u64,
}

/// Session-scoped per-strategy aggregates (P-1.1 accounting).
#[derive(Debug, Default, Clone)]
struct StrategyAccounting {
    placed: u64,
    rejected: u64,
    limit_rejected: u64,
    closed_trades: u64,
    wins: u64,
    losses: u64,
    fees_usd: Decimal,
    net_pnl_usd: Decimal,
}

/// Compact round info for the UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundView {
    pub slot: i64,
    pub age_sec: i64,
    pub time_left_sec: i64,
    pub markets: usize,
    pub can_trade: bool,
    /// Per-asset live prices (from the local book mid, falling back to the
    /// scanner's last price). For the UI Prices section.
    pub market_prices: Vec<MarketPriceView>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPriceView {
    pub asset: String,
    pub up: Decimal,
    pub down: Decimal,
}

#[cfg(test)]
mod books_mirror_tests {
    use super::*;
    use crate::risk::RiskConfig;
    use rust_decimal_macros::dec;

    /// Regression: a Rust-native feed tick (engine_on_data) must mirror the book
    /// into Core.books so the exit path can value open positions. Previously
    /// Core.books stayed empty under --feed-ws, so currentPrice never moved and
    /// no TP/trailing/SL exit could fire (positions looked frozen at entry).
    #[test]
    fn engine_feed_populates_core_books_and_enables_exits() {
        let cfg = CoreConfig {
            risk: RiskConfig { max_order_notional: dec!(100), ..Default::default() },
            dry_seed_balance: dec!(1000),
            round_duration_sec: 900,
            auto_exits_enabled: true,
            ..Default::default()
        };
        let mut c = Core::new(cfg);
        c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig::default()));
        let now = 1_000_000_000i64;
        let slot = now / 1000 / 900;
        let end = (slot + 1) * 900 * 1000;

        // Feed a round + a book the way the Rust feed layer does.
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![crate::model::CryptoMarket {
                    asset: "BTC".into(), condition_id: "c".into(), question_id: "q".into(),
                    up_token_id: "tok".into(), down_token_id: "d".into(),
                    up_price: dec!(0.6), down_price: dec!(0.4),
                    expires_at_ms: end, round_slot: slot, neg_risk: true, question: "?".into(),
                }],
                now_ms: now,
            },
            now,
        );
        c.engine_on_data(
            crate::engine::DataEvent::Book {
                token_id: "tok".into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now,
            },
            now,
        );
        // Open a position directly, then feed a profitable book via engine_on_data
        // and tick: the exit path must now see the higher price and exit.
        let req = crate::model::OrderRequest {
            token_id: "tok".into(), condition_id: "c".into(), side: crate::model::Side::Buy,
            mode: crate::model::FillPolicy::Taker, price: dec!(0.43), size: dec!(10),
            internal_key: "k".into(), strategy: "spread_arb".into(), asset: "BTC".into(),
            direction: "up".into(), round_slot: slot,
        };
        c.place(req, 0, now).unwrap();
        assert_eq!(c.positions().open_positions().len(), 1);
        let entry = c.positions().open_positions()[0].entry_price;

        // Feed a much higher book through the ENGINE path (the bug's path).
        c.engine_on_data(
            crate::engine::DataEvent::Book {
                token_id: "tok".into(),
                bids: vec![(dec!(0.95), dec!(100))],
                asks: vec![(dec!(0.97), dec!(100))],
                now_ms: now + 1000,
            },
            now + 1000,
        );
        c.tick(now + 1200).unwrap();

        // Position closed with a profit, and the recorded exit reflects the
        // higher price (not frozen at entry).
        assert_eq!(c.positions().open_positions().len(), 0, "exit engine must close on profit");
        let closed = &c.positions().closed_positions()[0];
        assert!(closed.exit_price > entry, "exit must be above entry (was frozen before fix)");
        assert!(closed.net_pnl_usd > Decimal::ZERO);
    }
}

#[cfg(test)]
mod round_expiry_tests {
    use super::*;
    use crate::risk::RiskConfig;
    use rust_decimal_macros::dec;

    /// Regression: a filled entry must expire at the END of its round slot, not
    /// the start. The old formula (slot * duration) put expiry in the past and
    /// force-exited every position on arrival (hold=0s, "never trades").
    #[test]
    fn filled_entry_expires_at_round_end_not_start() {
        let cfg = CoreConfig {
            risk: RiskConfig { max_order_notional: dec!(100), ..Default::default() },
            dry_seed_balance: dec!(1000),
            round_duration_sec: 300,
            ..Default::default()
        };
        let duration = cfg.round_duration_sec;
        let mut c = Core::new(cfg);
        // A round slot that started 60s ago; its end is 240s from now.
        let now_ms = 1_000_000_000i64;
        let slot = now_ms / 1000 / duration;
        let req = OrderRequest {
            token_id: "tok".into(),
            condition_id: "c".into(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price: dec!(0.40),
            size: dec!(10),
            internal_key: "k".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: slot,
        };
        c.place(req, 0, now_ms).unwrap();
        let pos = &c.positions().open_positions()[0];
        let expected_end = (slot + 1) * duration * 1000;
        assert_eq!(pos.expires_at_ms, expected_end, "expiry must be the round END");
        assert!(pos.expires_at_ms > now_ms, "expiry must be in the future");
        // time_left must be comfortably positive (not an instant force-exit).
        let time_left = (pos.expires_at_ms - now_ms) / 1000;
        assert!(time_left > 0 && time_left <= duration, "time_left={time_left}");
    }
}

#[cfg(test)]
mod shadow_evolution_tests {
    use super::*;
    use crate::risk::RiskConfig;
    use rust_decimal_macros::dec;

    /// Acceptance: enabling Shadow Evolution attaches the hot-swap handle to the
    /// engine, and a parameter change is visible to the engine on the next read
    /// (no restart). Also: enabling is opt-in and rollback restores the prior set.
    #[test]
    fn hot_swap_reaches_the_engine_and_rolls_back() {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig::default(),
            dry_seed_balance: dec!(1000),
            shadow_evolution_enabled: false,
            ..Default::default()
        });
        c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig::default()));
        assert!(!c.shadow_evolution().is_enabled());
        assert!(!c.engine.as_ref().unwrap().has_hot_params());

        // Enable → engine gains the hot-swap handle.
        c.shadow_evolution_enable(1000);
        assert!(c.engine.as_ref().unwrap().has_hot_params());

        let before = c.engine.as_ref().unwrap().current_spread_arb().trend_max_entry_price;
        // Simulate an applied evolution via the operator override path (+3%).
        let mut newp = c.shadow_evolution().current_params();
        newp.trend_max_entry_price = before * dec!(1.03);
        c.shadow_evolution_apply(newp.clone(), 1500).unwrap();
        let after = c.engine.as_ref().unwrap().current_spread_arb().trend_max_entry_price;
        assert_eq!(after, before * dec!(1.03), "engine must observe hot-swapped params");

        // Rollback restores the previous set.
        let _ = c.shadow_evolution_rollback(2000);
        let restored = c.engine.as_ref().unwrap().current_spread_arb().trend_max_entry_price;
        assert_eq!(restored, before, "rollback must restore prior params in the engine");
    }

    #[test]
    fn disabled_by_default_is_fully_inert() {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig::default(),
            ..Default::default()
        });
        c.enable_engine(crate::engine::Engine::new(crate::engine::EngineConfig::default()));
        assert!(!c.shadow_evolution().is_enabled());
        assert_eq!(c.shadow_evolution().variant_count(), 0);
        c.shadow_evolution_evaluate(1000);
        assert_eq!(c.shadow_evolution().history(10).len(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::RiskConfig;
    use rust_decimal_macros::dec;

    fn dry_core(balance: Decimal) -> Core {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig { max_order_notional: dec!(3), ..Default::default() },
            dry_seed_balance: Decimal::from(10_000),
            ..Default::default()
        });
        c.set_balance(balance);
        c
    }

    fn order(mode: FillPolicy, price: Decimal, size: Decimal, key: &str) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode,
            price,
            size,
            internal_key: key.into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    #[test]
    fn taker_fills_immediately_and_spends() {
        let mut c = dry_core(dec!(10));
        let (id, st) = c.place(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1).unwrap();
        assert_eq!(st, OrderStatus::Filled);
        assert_eq!(c.ome().get(&id).unwrap().filled_size, dec!(5));
        // 10 - 0.4*5 = 8; reservation fully consumed.
        assert_eq!(c.ledger().balance(), dec!(8));
        assert_eq!(c.ledger().reserved(), dec!(0));
    }

    #[test]
    fn maker_rests_until_book_crosses() {
        let mut c = dry_core(dec!(10));
        // BUY maker 0.40*5 = 2.0 reserved, not filled yet.
        let (id, st) = c.place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1).unwrap();
        assert_eq!(st, OrderStatus::Live);
        assert_eq!(c.ledger().available(), dec!(8));

        // Ask 0.45 does not cross a 0.40 bid → still live.
        c.book_snapshot("tok", vec![], vec![(dec!(0.45), dec!(100))], 2);
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);

        // Ask drops to 0.40 → fills at the resting 0.40.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 3);
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Filled);
        assert_eq!(c.ledger().balance(), dec!(8));
    }

    #[test]
    fn maker_then_taker_escalates_after_timeout() {
        let mut c = dry_core(dec!(10));
        let (id, st) = c.place(order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"), 1000, 1).unwrap();
        assert_eq!(st, OrderStatus::Live);
        // Book never crosses within the maker window.
        c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2);
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);
        // At timeout: maker cancelled, taker order crosses immediately.
        c.tick(1002).unwrap();
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
        // A new escalated taker order exists and is filled.
        let filled: Vec<_> = c.ome().all().into_iter().filter(|o| o.status == OrderStatus::Filled).collect();
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].filled_size, dec!(5));
        assert_eq!(c.ledger().balance(), dec!(8));
    }

    #[test]
    fn cancel_releases_reservation() {
        let mut c = dry_core(dec!(10));
        let (id, _) = c.place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1).unwrap();
        assert_eq!(c.ledger().available(), dec!(8));
        c.cancel(&id, 2).unwrap();
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
        assert_eq!(c.ledger().available(), dec!(10));
    }

    #[test]
    fn risk_and_ledger_reject_oversize_and_kill() {
        let mut c = dry_core(dec!(10));
        // notional 0.4*10 = 4 > cap 3 → rejected before reservation.
        let e = c.place(order(FillPolicy::Taker, dec!(0.4), dec!(10), "k1"), 0, 1).unwrap_err();
        assert_eq!(e.code, CoreErrorCode::RiskRejected);

        // Balance gate: only 2 available after a 2 reservation → a 3 buy fails.
        let mut c2 = dry_core(dec!(2));
        let e2 = c2.place(order(FillPolicy::Taker, dec!(0.3), dec!(10), "k2"), 0, 1).unwrap_err();
        assert_eq!(e2.code, CoreErrorCode::InsufficientFunds);

        let mut c3 = dry_core(dec!(10));
        c3.kill("manual".into());
        let e3 = c3.place(order(FillPolicy::Taker, dec!(0.4), dec!(1), "k3"), 0, 1).unwrap_err();
        assert_eq!(e3.code, CoreErrorCode::KillSwitchActive);
    }

    #[test]
    fn buy_fill_opens_position_and_exit_closes_it() {
        let mut c = dry_core(dec!(100));
        // Taker BUY 0.40 x 5 fills → position opened.
        let (id, st) = c.place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 1).unwrap();
        assert_eq!(st, OrderStatus::Filled);
        assert_eq!(c.positions().open_positions().len(), 1);

        // Push a book showing a large profit → tick must emit a closing SELL.
        c.book_snapshot("tok", vec![(dec!(0.99), dec!(100))], vec![(dec!(1.0), dec!(100))], 2);
        c.tick(2).unwrap();
        // Exit SELL (taker) crosses immediately at 0.99 → position closed.
        assert_eq!(c.positions().open_positions().len(), 0, "position should be closed");
        assert_eq!(c.positions().closed_positions().len(), 1);
        let closed = &c.positions().closed_positions()[0];
        assert!(closed.net_pnl_usd > Decimal::ZERO, "expected profit, got {}", closed.net_pnl_usd);
        let _ = id;
    }

    #[test]
    fn forced_exit_closes_at_a_loss() {
        let mut c = dry_core(dec!(100));
        // Buy at 0.40 (round_slot 1 → expires at the END of slot 1 = 1_800_000ms).
        c.place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 0).unwrap();
        assert_eq!(c.positions().open_positions().len(), 1);
        // Push a lower book, then tick at the force-exit horizon (100s left).
        c.book_snapshot("tok", vec![(dec!(0.30), dec!(100))], vec![(dec!(0.31), dec!(100))], 1);
        c.tick(1_800_000 - 100).unwrap();
        assert_eq!(c.positions().open_positions().len(), 0, "force exit should close the position");
        let closed = &c.positions().closed_positions()[0];
        assert!(closed.net_pnl_usd < Decimal::ZERO, "expected a loss, got {}", closed.net_pnl_usd);
    }

    #[test]
    fn entry_is_blocked_by_daily_loss_limit() {
        let mut c = dry_core(dec!(100));
        // Shrink the daily loss cap so one losing trade trips it.
        let mut pc = c.config.positions.clone();
        pc.max_daily_loss_usd = dec!(1);
        pc.asset_cooldown_sec = 0;
        pc.loss_cooldown_sec = 0;
        pc.stop_loss_cooldown_sec = 0;
        pc.exit_cooldown_sec = 0;
        c.positions.set_config(pc);
        c.place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 0).unwrap();
        c.book_snapshot("tok", vec![(dec!(0.20), dec!(100))], vec![(dec!(0.21), dec!(100))], 1);
        c.tick(1_800_000 - 100).unwrap();
        assert!(c.positions().daily_pnl() < dec!(-1));
        let e = c.place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k2"), 0, 900_000).unwrap_err();
        assert_eq!(e.code, CoreErrorCode::RiskRejected);
        assert!(e.message.to_lowercase().contains("daily"), "got {}", e.message);
    }
}

/// P-1.1: host dispatch to the builtin strategy through Core, with per-strategy
/// caps and per-strategy accounting.
#[cfg(test)]
mod strategy_dispatch_tests {
    use super::*;
    use crate::engine::{DataEvent, Engine, EngineConfig};
    use crate::model::CryptoMarket;
    use crate::risk::RiskConfig;
    use rust_decimal_macros::dec;

    /// Mirrors the production engine knobs closely enough to trade the feed below
    /// (same shape as the engine.rs unit tests).
    fn engine_cfg() -> EngineConfig {
        EngineConfig {
            scanner: crate::scanner::ScannerConfig {
                assets: vec!["BTC".into()],
                round_duration_sec: 900,
                min_round_age_sec: 0,
                min_time_left_sec: 0,
            },
            trend: crate::signal::TrendConfig {
                confirm_sec: 5,
                ratio: dec!(0.5),
                min_price: dec!(0.5),
                broken_price: dec!(0.35),
                window_floor_ms: 0,
            },
            spread_arb: crate::signal::SpreadArbConfig {
                trend_max_entry_price: dec!(0.45),
                ..Default::default()
            },
            max_orderbook_stale_ms: 8000,
            momentum_window_sec: 30,
            momentum_tol_pct: dec!(0.03),
            size_usd: dec!(2.5),
            min_shares: dec!(10),
            max_shares: dec!(10),
            strategy_sizes: HashMap::new(),
        }
    }

    fn core_with_engine(limits: HashMap<String, StrategyLimit>) -> Core {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig { max_order_notional: dec!(100), ..Default::default() },
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            round_duration_sec: 900,
            auto_exits_enabled: true,
            strategy_limits: limits,
            ..Default::default()
        });
        c.enable_engine(Engine::new(engine_cfg()));
        c
    }

    // ── E2-a: per-strategy sizing + independent quotas ──────────────────────

    /// A test strategy that dips on an explicit asset list. Declaring the assets
    /// lets several strategies share one cycle without fighting over a token.
    struct TargetDip {
        name: String,
        buy_below: Decimal,
        assets: Vec<String>,
    }
    impl crate::strategies::EngineStrategy for TargetDip {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
        fn on_round(&mut self, _slot: i64) {}
        fn find_candidates(
            &mut self,
            ctx: &crate::strategies::StrategyCtx<'_>,
        ) -> Vec<crate::signal::TradeSignal> {
            let mut out = Vec::new();
            for market in ctx.markets() {
                if !self.assets.contains(&market.asset) {
                    continue;
                }
                for (token_id, direction) in [
                    (&market.up_token_id, crate::model::SignalDirection::Up),
                    (&market.down_token_id, crate::model::SignalDirection::Down),
                ] {
                    let Some(book) = ctx.fresh_book(token_id) else { continue };
                    if book.mid_price <= self.buy_below {
                        out.push(crate::signal::TradeSignal {
                            strategy: self.name.clone(),
                            asset: market.asset.clone(),
                            direction,
                            token_id: token_id.clone(),
                            condition_id: market.condition_id.clone(),
                            price: book.mid_price,
                            reason: "target dip".into(),
                        });
                    }
                }
            }
            out
        }
    }

    const DIPS: [(&str, &[&str]); 3] = [("small", &["BTC"]), ("mid", &["ETH"]), ("unset", &["SOL"])];

    /// Core with a three-asset engine and the given dip strategies registered
    /// enabled. `spread_arb` is switched off so only the test strategies emit.
    ///
    /// The engine is built through `CoreConfig::engine_config()` — the ONE
    /// production mapping — so these tests exercise the same per-strategy sizing
    /// projection the live server and the backtester use.
    fn core_with_dips(
        limits: HashMap<String, StrategyLimit>,
        dips: &[(&str, &[&str])],
        global_max_positions: usize,
    ) -> Core {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig { max_order_notional: dec!(100), ..Default::default() },
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            round_duration_sec: 900,
            min_round_age_sec: 0,
            auto_exits_enabled: false,
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            positions: crate::position::PositionConfig {
                max_positions: global_max_positions,
                exit: crate::exit_policy::ExitConfig { min_time_left_sec: 0, ..Default::default() },
                ..Default::default()
            },
            strategy_limits: limits,
            ..Default::default()
        });
        let cfg = c.config().engine_config();
        let mut eng = Engine::new(cfg);
        for (name, assets) in dips {
            eng.register_user_strategy(
                Box::new(TargetDip {
                    name: (*name).to_string(),
                    buy_below: dec!(0.45),
                    assets: assets.iter().map(|a| (*a).to_string()).collect(),
                }),
                "test".into(),
            )
            .unwrap();
            assert!(eng.set_strategy_enabled(name, true));
        }
        assert!(eng.set_strategy_enabled("spread_arb", false), "isolate the test strategies");
        c.enable_engine(eng);
        c
    }

    fn three_markets(now: i64) -> Vec<CryptoMarket> {
        let _ = now;
        ["BTC", "ETH", "SOL"]
            .iter()
            .map(|a| CryptoMarket {
                asset: (*a).into(),
                condition_id: format!("cond_{a}"),
                question_id: format!("q_{a}"),
                up_token_id: format!("{}_up", a.to_lowercase()),
                down_token_id: format!("{}_down", a.to_lowercase()),
                up_price: dec!(0.6),
                down_price: dec!(0.4),
                expires_at_ms: 1_800_000,
                round_slot: 1,
                neg_risk: true,
                question: format!("{a} up or down"),
            })
            .collect()
    }

    /// Push a 0.44-mid book on the UP token of each given asset. Only the listed
    /// assets dip, so a cycle can be aimed at one strategy.
    fn feed_dip_on(c: &mut Core, now: i64, assets: &[&str]) {
        for asset in assets {
            let token = format!("{}_up", asset.to_lowercase());
            c.engine_on_data(
                DataEvent::Book {
                    token_id: token,
                    bids: vec![(dec!(0.43), dec!(100))],
                    asks: vec![(dec!(0.45), dec!(100))],
                    now_ms: now,
                },
                now,
            );
        }
    }

    /// Round + a 0.44-mid book on every asset's up token. Each strategy that
    /// targets one of those assets emits one entry priced at 0.44.
    fn feed_three_asset_dip(c: &mut Core, now: i64) {
        c.engine_on_data(DataEvent::RoundMarkets { markets: three_markets(now), now_ms: now }, now);
        feed_dip_on(c, now + 1_000, &["BTC", "ETH", "SOL"]);
    }

    /// Cross the book so the resting maker bid fills and a position opens.
    fn fill(c: &mut Core, asset: &str, now: i64) {
        let token = format!("{}_up", asset.to_lowercase());
        c.book_snapshot(&token, vec![(dec!(0.42), dec!(100))], vec![(dec!(0.42), dec!(100))], now);
    }

    fn limits_of(rows: &[(&str, StrategyLimit)]) -> HashMap<String, StrategyLimit> {
        rows.iter().map(|(n, l)| ((*n).to_string(), l.clone())).collect()
    }

    #[test]
    fn three_strategies_size_independently_in_one_cycle() {
        // Global band is [10,10] at a 2.5u budget; two strategies tighten it and
        // one inherits it. All three trade the same cycle without interfering.
        let limits = limits_of(&[
            ("small", StrategyLimit {
                size_usd: Some(dec!(1)), min_shares: Some(dec!(2)), max_shares: Some(dec!(2)),
                ..Default::default()
            }),
            ("mid", StrategyLimit {
                size_usd: Some(dec!(2)), min_shares: Some(dec!(4)), max_shares: Some(dec!(4)),
                ..Default::default()
            }),
        ]);
        let mut c = core_with_dips(limits, &DIPS, 5);
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 1_000), 3, "one entry per strategy: {:#?}", c.list_orders());

        let sizes: Vec<(String, Decimal)> =
            c.list_orders().iter().map(|o| (o.strategy.clone(), o.size)).collect();
        assert_eq!(
            sizes.iter().find(|(s, _)| s == "small").map(|(_, v)| *v),
            Some(dec!(2)),
            "1u/0.44 → 2 shares inside [2,2]: {sizes:?}"
        );
        assert_eq!(
            sizes.iter().find(|(s, _)| s == "mid").map(|(_, v)| *v),
            Some(dec!(4)),
            "2u/0.44 ≈ 5 → clamped to the strategy's 4-share ceiling: {sizes:?}"
        );
        assert_eq!(
            sizes.iter().find(|(s, _)| s == "unset").map(|(_, v)| *v),
            Some(dec!(10)),
            "no override must fall back to the global lot: {sizes:?}"
        );

        let stats = c.strategy_stats();
        let small = strategy_entry(&stats, "small");
        assert_eq!(small["sizingSource"], "strategy");
        assert_eq!(dec_of(&small["effectiveSizeUsd"]), dec!(1));
        assert_eq!(dec_of(&small["effectiveMinShares"]), dec!(2));
        assert_eq!(dec_of(&small["effectiveMaxShares"]), dec!(2));
        let unset = strategy_entry(&stats, "unset");
        assert_eq!(unset["sizingSource"], "global");
        assert_eq!(dec_of(&unset["effectiveSizeUsd"]), dec!(2.5));
        assert_eq!(dec_of(&unset["effectiveMaxShares"]), dec!(10));
    }

    #[test]
    fn per_strategy_quota_is_independent_across_strategies() {
        // `capped` may hold zero positions and targets two assets; `other` has no
        // cap and targets a third. In one cycle the cap rejects both of `capped`'s
        // candidates while `other` trades untouched — the quotas are per strategy,
        // and one strategy being full never starves another.
        let limits = limits_of(&[(
            "capped",
            StrategyLimit {
                max_open_positions: Some(0),
                size_usd: Some(dec!(1)), min_shares: Some(dec!(2)), max_shares: Some(dec!(2)),
                ..Default::default()
            },
        )]);
        let mut c = core_with_dips(limits, &[("capped", &["BTC", "ETH"]), ("other", &["SOL"])], 5);
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 1_000), 1, "only `other` may place: {:#?}", c.list_orders());
        assert_eq!(c.engine_stats()["strategyLimitRejected"], 2, "both capped candidates were rejected");
        assert_eq!(c.list_orders()[0].strategy, "other");

        let stats = c.strategy_stats();
        let capped = strategy_entry(&stats, "capped");
        assert_eq!(capped["ordersPlaced"], 0);
        assert_eq!(capped["limitRejected"], 2, "counted against the capped strategy only");
        assert_eq!(capped["maxOpenPositions"], 0);
        assert_eq!(dec_of(&capped["effectiveMaxShares"]), dec!(2), "its own lot is 2 shares");
        let other = strategy_entry(&stats, "other");
        assert_eq!(other["ordersPlaced"], 1, "the sibling still trades");
        assert_eq!(other["limitRejected"], 0);
        assert_eq!(other["maxOpenPositions"], serde_json::Value::Null, "unset = uncapped");
        assert_eq!(dec_of(&other["effectiveSizeUsd"]), dec!(2.5), "and inherits the global sizing");
    }

    #[test]
    fn a_strategy_cap_counts_only_its_own_open_positions() {
        // `capped` allows one position and gets exactly one: it takes BTC, fills
        // it, and is then rejected on ETH — while `other`'s SOL position is
        // invisible to `capped`'s quota (and vice versa).
        let limits = limits_of(&[(
            "capped",
            StrategyLimit { max_open_positions: Some(1), ..Default::default() },
        )]);
        let mut c = core_with_dips(limits, &[("capped", &["BTC", "ETH"]), ("other", &["SOL"])], 5);
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 1_000), 3, "two capped candidates + one sibling");

        // Fill BTC (capped) and SOL (other): two live positions, one per strategy.
        fill(&mut c, "BTC", now + 2_000);
        fill(&mut c, "SOL", now + 2_000);
        let open = c.positions().open_positions();
        assert_eq!(open.len(), 2, "{open:#?}");
        assert_eq!(open.iter().filter(|p| p.strategy == "capped").count(), 1);
        assert_eq!(open.iter().filter(|p| p.strategy == "other").count(), 1);
    }


    #[test]
    fn the_global_position_ceiling_still_binds_across_strategies() {
        // Per-strategy quotas do not lift the global capacity gate. Open two
        // positions with two strategies, then let a THIRD strategy find a dip:
        // its entry is rejected by the shared global ceiling, not by its own cap.
        let mut c = core_with_dips(
            HashMap::new(),
            &[("a", &["BTC"]), ("b", &["ETH"]), ("c", &["SOL"])],
            2,
        );
        let now = 1_000_000i64;
        c.engine_on_data(DataEvent::RoundMarkets { markets: three_markets(now), now_ms: now }, now);

        feed_dip_on(&mut c, now + 1_000, &["BTC"]);
        assert_eq!(c.engine_evaluate(now + 1_000), 1, "a takes BTC");
        fill(&mut c, "BTC", now + 2_000);
        feed_dip_on(&mut c, now + 3_000, &["ETH"]);
        assert_eq!(c.engine_evaluate(now + 3_000), 1, "b takes ETH");
        fill(&mut c, "ETH", now + 4_000);
        assert_eq!(c.positions().open_positions().len(), 2, "global ceiling reached");

        feed_dip_on(&mut c, now + 5_000, &["SOL"]);
        assert_eq!(c.engine_evaluate(now + 5_000), 0, "the third strategy must be gated out");
        assert_eq!(c.engine_stats()["placeRejected"], 1, "rejected by the global capacity gate");
        assert_eq!(c.engine_stats()["strategyLimitRejected"], 0, "and NOT by a per-strategy quota");
        assert_eq!(c.positions().open_positions().len(), 2);

        let stats = c.strategy_stats();
        // No strategy configured a quota, so all three report uncapped yet the
        // global gate still held the line.
        for name in ["a", "b", "c"] {
            assert_eq!(strategy_entry(&stats, name)["maxOpenPositions"], serde_json::Value::Null);
        }
        assert_eq!(strategy_entry(&stats, "c")["ordersRejected"], 1);
    }


    #[test]
    fn a_greedy_strategy_override_is_clamped_to_the_global_risk_band() {
        // sizeUsd 100 / maxShares 999 / minShares 0 all lose to the globals; the
        // report still marks the strategy as sizing-scoped.
        let limits = limits_of(&[(
            "small",
            StrategyLimit {
                size_usd: Some(dec!(100)),
                min_shares: Some(dec!(0)),
                max_shares: Some(dec!(999)),
                ..Default::default()
            },
        )]);
        let mut c = core_with_dips(limits, &[("small", &["BTC"])], 5);
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 1_000), 1);
        assert_eq!(c.list_orders()[0].size, dec!(10), "global ceiling must win");

        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "small");
        assert_eq!(s["sizingSource"], "strategy");
        assert_eq!(dec_of(&s["effectiveSizeUsd"]), dec!(2.5), "notional clamped to the global budget");
        assert_eq!(dec_of(&s["effectiveMaxShares"]), dec!(10), "share ceiling clamped to the global");
        assert_eq!(dec_of(&s["effectiveMinShares"]), dec!(10), "floor raised to the global min");
    }

    fn market(now: i64) -> CryptoMarket {
        let slot = now / 1000 / 900;
        CryptoMarket {
            asset: "BTC".into(), condition_id: "cond".into(), question_id: "q".into(),
            up_token_id: "up".into(), down_token_id: "down".into(),
            up_price: dec!(0.6), down_price: dec!(0.4),
            expires_at_ms: (slot + 1) * 900 * 1000, round_slot: slot,
            neg_risk: true, question: "BTC up or down".into(),
        }
    }

    /// Round + confirmed UP trend + calm spot + one dip book: the builtin emits
    /// exactly one spread_arb entry (0.43 x 10 = 4.30 USD) on the next cycle.
    fn feed_entry_setup(c: &mut Core, now: i64) {
        c.engine_on_data(DataEvent::RoundMarkets { markets: vec![market(now)], now_ms: now }, now);
        for i in 0..12 {
            let t = now + i * 1000;
            c.engine_on_data(
                DataEvent::Book {
                    token_id: "up".into(),
                    bids: vec![(dec!(0.55), dec!(100))],
                    asks: vec![(dec!(0.57), dec!(100))],
                    now_ms: t,
                },
                t,
            );
        }
        let t = now + 12_000;
        c.engine_on_data(
            DataEvent::Book {
                token_id: "up".into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: t,
            },
            t,
        );
        c.engine_on_data(DataEvent::Spot { asset: "BTC".into(), price: dec!(60000), now_ms: t }, t);
    }

    /// The payload decimals arrive as JSON numbers (Node wire convention).
    fn dec_of(v: &serde_json::Value) -> Decimal {
        match v {
            serde_json::Value::Number(n) => Decimal::from_str_exact(&n.to_string()),
            serde_json::Value::String(s) => Decimal::from_str_exact(s.trim()),
            other => panic!("not a decimal: {other}"),
        }
        .unwrap_or_else(|e| panic!("not a decimal: {v} ({e})"))
    }

    fn strategy_entry<'a>(stats: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
        stats.iter().find(|s| s["name"] == name).unwrap_or_else(|| panic!("no stats for {name}: {stats:?}"))
    }

    #[test]
    fn default_config_has_no_limits_and_places_the_entry() {
        let mut c = core_with_engine(HashMap::new());
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 12_000), 1, "no caps configured must not change behaviour");
        assert_eq!(c.engine_stats()["strategyLimitRejected"], 0);

        // Fill the resting maker entry so exposure is live, then read the ledger.
        c.book_snapshot("up", vec![(dec!(0.42), dec!(100))], vec![(dec!(0.42), dec!(100))], now + 13_000);
        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "spread_arb");
        assert_eq!(s["enabled"], true);
        assert_eq!(s["source"], "builtin");
        assert_eq!(s["ordersPlaced"], 1);
        assert_eq!(s["openPositions"], 1);
        assert_eq!(dec_of(&s["openNotionalUsd"]), dec!(4.30));
    }

    #[test]
    fn strategy_position_cap_rejects_the_entry_and_is_counted() {
        let mut limits = HashMap::new();
        limits.insert(
            "spread_arb".to_string(),
            StrategyLimit { max_open_positions: Some(0), ..Default::default() },
        );
        let mut c = core_with_engine(limits);
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 12_000), 0, "position cap 0 must block the entry");
        assert!(c.ome().live_orders().is_empty(), "no order may reach the OME");
        assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "spread_arb");
        assert_eq!(s["limitRejected"], 1);
        assert_eq!(s["ordersPlaced"], 0);
        assert_eq!(s["openPositions"], 0);
    }

    #[test]
    fn strategy_notional_cap_blocks_below_and_passes_above_the_entry() {
        // Entry notional is 0.43 * 10 = 4.30: a 1.00 cap rejects, a 10.00 cap passes.
        let mut tight = HashMap::new();
        tight.insert(
            "spread_arb".to_string(),
            StrategyLimit { max_open_notional_usd: Some(dec!(1)), ..Default::default() },
        );
        let mut c = core_with_engine(tight);
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 12_000), 0, "notional cap 1.00 must block a 4.30 entry");
        assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);

        let mut loose = HashMap::new();
        loose.insert(
            "spread_arb".to_string(),
            StrategyLimit { max_open_notional_usd: Some(dec!(10)), ..Default::default() },
        );
        let mut c2 = core_with_engine(loose);
        feed_entry_setup(&mut c2, now);
        assert_eq!(c2.engine_evaluate(now + 12_000), 1, "cap above the entry notional must not block");
        assert_eq!(c2.engine_stats()["strategyLimitRejected"], 0);
    }

    #[test]
    fn closed_trade_lands_in_the_strategy_ledger() {
        let mut c = core_with_engine(HashMap::new());
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 12_000), 1);
        assert_eq!(c.positions().open_positions().len(), 0, "maker entry rests until the book crosses");

        // Fill the resting bid (ask 0.42 crosses the 0.43 bid), then push a
        // +100%-ish book so the exit engine closes the position at a profit.
        c.book_snapshot("up", vec![(dec!(0.42), dec!(100))], vec![(dec!(0.42), dec!(100))], now + 13_000);
        assert_eq!(c.positions().open_positions().len(), 1, "crossing ask must fill the maker entry");
        c.book_snapshot("up", vec![(dec!(0.95), dec!(100))], vec![(dec!(0.97), dec!(100))], now + 14_000);
        c.tick(now + 14_200).unwrap();
        assert_eq!(c.positions().open_positions().len(), 0, "profit target must close the position");

        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "spread_arb");
        assert_eq!(s["closedTrades"], 1);
        assert_eq!(s["wins"], 1);
        assert_eq!(s["losses"], 0);
        assert!(dec_of(&s["netPnlUsd"]) > Decimal::ZERO, "expected positive realized PnL");
        assert!(dec_of(&s["feesUsd"]) > Decimal::ZERO, "taker exit pays a fee");
        assert_eq!(dec_of(&s["openNotionalUsd"]), Decimal::ZERO, "closed position leaves no exposure");
    }
}

/// P-1.2: the dry matcher honours the configured [`crate::sim::FillModel`]. The
/// default is the identity (existing tests cover it); these pin down the opt-in
/// deviations and the maker-only nature of the latency/probability gates.
#[cfg(test)]
mod fill_model_tests {
    use super::*;
    use crate::risk::RiskConfig;
    use crate::sim::FillModel;
    use rust_decimal_macros::dec;

    fn core_with(model: FillModel) -> Core {
        Core::new(CoreConfig {
            mode: Mode::Dry,
            risk: RiskConfig { max_order_notional: dec!(100), ..Default::default() },
            dry_seed_balance: dec!(1000),
            auto_exits_enabled: false,
            trade_log_path: None,
            order_log_path: None,
            position_log_path: None,
            fill_model: model,
            ..Default::default()
        })
    }

    fn buy(mode: FillPolicy, price: Decimal, size: Decimal) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode,
            price,
            size,
            internal_key: "k1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    #[test]
    fn default_model_fills_a_crossing_maker_at_its_own_limit() {
        let mut c = core_with(FillModel::default());
        let (id, _) = c.place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000).unwrap();
        c.book_snapshot("tok", vec![(dec!(0.39), dec!(100))], vec![(dec!(0.40), dec!(100))], 1_100);
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Filled);
        let pos = &c.positions().open_positions()[0];
        assert_eq!(pos.entry_price, dec!(0.40), "a maker fill is priced by your own quote");
    }

    #[test]
    fn taker_slippage_worsens_the_fill_price() {
        let mut c = core_with(FillModel { taker_slippage_ticks: 2, ..FillModel::default() });
        let (id, st) = c.place(buy(FillPolicy::Taker, dec!(0.40), dec!(10)), 0, 1_000).unwrap();
        assert_eq!(st, OrderStatus::Filled);
        // 2 ticks = 0.02: a taker buy pays up rather than filling at the limit.
        assert_eq!(c.ome().get(&id).unwrap().avg_fill_price, Some(dec!(0.42)));
        assert_eq!(c.positions().open_positions()[0].entry_price, dec!(0.42));
    }

    #[test]
    fn maker_latency_delays_the_crossing_fill() {
        let mut c = core_with(FillModel { maker_latency_ms: 5_000, ..FillModel::default() });
        let (id, _) = c.place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000).unwrap();
        // Crossed 100 ms after submission: the venue could not have the order yet.
        c.book_snapshot("tok", vec![(dec!(0.39), dec!(100))], vec![(dec!(0.40), dec!(100))], 1_100);
        assert_eq!(c.positions().open_positions().len(), 0, "no fill inside the latency window");
        assert!(c.ome().get(&id).unwrap().status.is_live(), "the order stays live");
        // Crossed again after the window: fills.
        c.book_snapshot("tok", vec![(dec!(0.39), dec!(100))], vec![(dec!(0.40), dec!(100))], 6_100);
        assert_eq!(c.positions().open_positions().len(), 1, "fills once the latency has elapsed");
        assert_eq!(c.positions().open_positions()[0].entry_price, dec!(0.40));
    }

    #[test]
    fn zero_fill_probability_never_fills_on_a_crossing() {
        let mut c = core_with(FillModel { maker_fill_prob_bps: 0, ..FillModel::default() });
        let (id, _) = c.place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000).unwrap();
        for t in [1_100i64, 2_000, 9_000] {
            c.book_snapshot("tok", vec![(dec!(0.39), dec!(100))], vec![(dec!(0.40), dec!(100))], t);
        }
        assert_eq!(c.positions().open_positions().len(), 0, "a losing queue draw keeps the order unfilled");
        assert!(c.ome().get(&id).unwrap().status.is_live());
        assert_eq!(c.ome().get(&id).unwrap().filled_size, Decimal::ZERO);
    }
}
