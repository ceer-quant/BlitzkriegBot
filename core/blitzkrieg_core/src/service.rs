//! Core service — assembles the OME, ledger, risk gate and dry matcher and
//! implements every typed command. Synchronous except for the event sink, so it
//! is fully unit-testable; the async UDS layer and live venue sit on top.

use crate::ipc::schema::Event;
use crate::ledger::Ledger;
use crate::model::*;
use crate::ome::{FillDelta, Ome, SubmitParams};
use crate::position::{OpenParams, PositionConfig, PositionManager};
use crate::risk::{LossBreakers, RiskConfig, RiskGate};
use crate::shadow_evolution::{
    EvolutionOutcome, EvolutionStatus, MutableParams, ShadowEvolution, ShadowEvolutionConfig,
};
use crate::sim::{Book, rests_on_book};
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
    /// Strategies to switch ON after the engine is installed, by name (E4-a).
    /// The builtins start in a fixed state (`spread_arb` on, `trend_follow` off),
    /// so adding a strategy can never change what an existing session trades;
    /// this is how an operator opts a session into one. Unknown names are
    /// reported and ignored, never fatal.
    pub enabled_strategies: Vec<String>,
    /// Strategies to switch OFF after the engine is installed, by name. Applied
    /// after `enabled_strategies`, so the explicit "off" wins if both name the
    /// same strategy.
    pub disabled_strategies: Vec<String>,
    /// Directory scanned at startup for user-layer strategy libraries
    /// (`*.dylib`/`*.so`): every library found is dlopen'd and enabled, so
    /// dropping a file into the folder is the whole installation procedure.
    /// `None` = no directory (auto-load off). Load failures are reported and
    /// skipped, never fatal — a broken file must not brick the kernel.
    pub strategy_dir: Option<String>,
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
    ///
    /// The strategy selection is applied through the same `set_strategy_enabled`
    /// the IPC method uses, so a session started with `--enable-strategy` and one
    /// toggled at runtime end up in exactly the same state (including the Shadow
    /// Evolution re-registration the toggle triggers).
    pub fn install_engine(&self, core: &mut Core) {
        core.enable_engine(crate::engine::Engine::new(self.engine_config()));
        self.load_strategy_dir(core);
        for name in &self.enabled_strategies {
            if !core.set_strategy_enabled(name, true) {
                tracing::warn!(
                    strategy = name,
                    "unknown strategy requested at startup; ignored"
                );
            }
        }
        for name in &self.disabled_strategies {
            if !core.set_strategy_enabled(name, false) {
                tracing::warn!(
                    strategy = name,
                    "unknown strategy requested at startup; ignored"
                );
            }
        }
    }

    /// Auto-load every strategy library under `strategy_dir` and enable it.
    ///
    /// This is the "drop a strategy in the folder and it works" path: the tree
    /// is walked (bounded depth) so both a bare `libfoo.dylib` and a
    /// `cargo build --release` product under `target/release/` are found, each
    /// library goes through the same policy-checked dlopen the IPC
    /// `strategy.load` uses, and a load that fails is reported and skipped —
    /// one broken file never blocks the rest or the kernel itself. Duplicate
    /// copies of the same strategy (debug + release) are skipped after the
    /// first registration: the engine refuses a second instance of a live name.
    /// Runs BEFORE `enabled_strategies` so a config that names a just-loaded
    /// library enables it in the same pass.
    fn load_strategy_dir(&self, core: &mut Core) {
        #[cfg(feature = "strategy-loading")]
        let Some(dir) = &self.strategy_dir else {
            return;
        };
        #[cfg(feature = "strategy-loading")]
        {
            let mut libs = Vec::new();
            collect_strategy_libs(std::path::Path::new(dir), 0, &mut libs);
            libs.sort();
            for path in libs {
                let receipt = core.load_strategy_lib(&path.to_string_lossy());
                // The success receipt is exactly "<name>@<version> registered
                // into the engine dispatch…"; a duplicate registration is
                // REJECTED with a message that itself contains the word
                // "registered", so the match must be on the full success shape.
                // A duplicate is benign — debug and release builds of the same
                // crate both land in the tree — so it is reported as skipped,
                // not failed, and the first copy (sorted order) wins.
                if receipt.contains("already registered") {
                    eprintln!(
                        "blitzkrieg-core: strategy auto-load skipped (duplicate): {}",
                        path.display()
                    );
                    continue;
                }
                let ok = receipt.contains("registered into the engine dispatch");
                eprintln!(
                    "blitzkrieg-core: strategy auto-load {}: {}",
                    if ok { "ok" } else { "FAILED" },
                    receipt
                );
                if !ok {
                    continue;
                }
                let name = receipt.split('@').next().unwrap_or_default().to_string();
                if name.is_empty() {
                    continue;
                }
                if !core.set_strategy_enabled(&name, true) {
                    tracing::warn!(strategy = %name, "auto-loaded but could not be enabled");
                }
            }
        }
        #[cfg(not(feature = "strategy-loading"))]
        {
            let _ = core;
        }
    }
}

/// Depth-bounded walk gathering `*.dylib`/`*.so` files under `dir`.
///
/// Bounded because `strategy_dir` may point at a crate checkout whose `target/`
/// tree is enormous; 6 levels comfortably covers `target/<profile>/deps/` while
/// never descending into the dependency graph's own build dirs. Symlinks are
/// not followed (a loop must not hang startup); unreadable subtrees are skipped.
fn collect_strategy_libs(dir: &std::path::Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
    const MAX_DEPTH: usize = 6;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            collect_strategy_libs(&path, depth + 1, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("dylib") | Some("so")
        ) {
            out.push(path);
        }
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
    /// Rolling metrics window (seconds). Settable from the config file, which
    /// expresses it in minutes (`evaluation_window_minutes`).
    pub evaluation_window_secs: Option<i64>,
    /// Per-evolution step ceiling (Lock 1). `main` clamps this to the built-in
    /// ceiling before it gets here — a config file may tighten the gradient lock
    /// but nothing may widen it.
    pub max_gradient: Option<Decimal>,
    /// Directory for the per-strategy audit files (`<dir>/<strategy>.jsonl`).
    /// Defaults to `data/evolution`; a harness points it at a scratch directory
    /// so an experiment never writes into the operator's real audit history.
    pub audit_dir: Option<String>,
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
            enabled_strategies: Vec::new(),
            disabled_strategies: Vec::new(),
            strategy_dir: None,
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

/// One position selected by `Core::flatten`, in the order its destructuring
/// loop consumes: id, token, condition, shares, strategy, asset, direction,
/// current price.
type FlattenTarget = (
    String,
    String,
    String,
    Decimal,
    String,
    String,
    String,
    Decimal,
);

/// One due `maker_then_taker` order selected by `Core::tick`, in the order its
/// destructuring loop consumes: id, price, remaining, side, token, condition,
/// strategy, asset, direction, round slot.
type EscalationTarget = (
    String,
    Decimal,
    Decimal,
    Side,
    String,
    String,
    String,
    String,
    String,
    i64,
);

pub struct Core {
    config: CoreConfig,
    ome: Ome,
    ledger: Ledger,
    risk: RiskGate,
    /// Per-strategy consecutive-loss breakers (KI-10 / D-18 A). The daily loss
    /// cap and kill switch stay global — see [`LossBreakers`].
    breaker: LossBreakers,
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
        let trade_db = config
            .trade_log_path
            .as_ref()
            .map(crate::trade_db::TradeDb::new);
        let order_db = config
            .order_log_path
            .as_ref()
            .map(crate::order_db::OrderDb::new);
        let position_db = config
            .position_log_path
            .as_ref()
            .map(crate::position_db::PositionDb::new);
        let shadow_cfg = {
            let mut c = ShadowEvolutionConfig {
                enabled: config.shadow_evolution_enabled,
                ..Default::default()
            };
            if let Some(t) = &config.shadow_evolution_tuning {
                if let Some(v) = t.min_sample_count {
                    c.min_sample_count = v;
                }
                if let Some(v) = t.min_win_rate_improvement {
                    c.min_win_rate_improvement = v;
                }
                if let Some(v) = t.min_profit_factor_improvement {
                    c.min_profit_factor_improvement = v;
                }
                if let Some(v) = t.min_observation_secs {
                    c.min_observation_secs = v;
                }
                if let Some(v) = t.cooldown_secs {
                    c.cooldown_secs = v;
                }
                if let Some(v) = t.variant_count {
                    c.variant_count = v;
                }
                if let Some(v) = t.evaluation_window_secs {
                    c.evaluation_window_secs = v;
                }
                if let Some(v) = t.max_gradient {
                    // Belt and braces: the caller clamps too, but this is the one
                    // place the lock could be widened, so it re-checks.
                    c.max_gradient = v.min(crate::config::MAX_GRADIENT_CEILING);
                }
                if let Some(v) = &t.audit_dir {
                    c.audit_dir = v.clone();
                }
            }
            // D-2: variant exits must replay the SAME policy the live position
            // manager runs, or the counterfactual is judged against an exit
            // mechanism the live path never uses.
            c.exit_cfg = config.positions.exit.clone();
            c
        };
        let breaker = LossBreakers::new(config.max_consecutive_losses, config.breaker_cooldown_sec);
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
        // A locally-settling mode has no venue to reconcile against; seed the
        // local cash so the reserve/overspend gate is meaningful for embedders
        // that don't seed it. Uses the predicate, not `== Dry`: the equality form
        // is invisible to the compiler and left ReadOnly funding at zero.
        if config.mode.settles_locally() {
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
                use crate::extension::Extension;
                let mut reg = crate::extension::ExtensionRegistry::new();
                // Built-in example extension (installed, not enabled by default).
                // Its config file is read and checked against the registered
                // extension (KI-11): the file is no longer inert, and a config
                // that has drifted from the code is reported at startup.
                let ext = crate::extension::builtins::BinanceSpotExtension::new();
                let path =
                    crate::extension::config_path_for(crate::extension::EXTENSIONS_DIR, ext.name());
                let cfg = crate::config::ExtensionConfig::load(&path);
                for w in &cfg.warnings {
                    tracing::warn!(extension = ext.name(), "extension config: {w}");
                }
                for k in &cfg.unknown_keys {
                    tracing::warn!(
                        extension = ext.name(),
                        key = k,
                        "extension config key not understood (ignored)"
                    );
                }
                for d in &cfg.declared_only {
                    tracing::info!(
                        extension = ext.name(),
                        key = %d.key,
                        reason = d.reason,
                        "extension config declares a setting the kernel does not act on"
                    );
                }
                let kind = format!("{:?}", ext.extension_type()).to_lowercase();
                for problem in cfg.check_against(ext.name(), ext.version(), &kind) {
                    tracing::warn!(extension = ext.name(), "extension config: {problem}");
                }
                reg.install(Box::new(ext), cfg.path);
                reg
            },
            shadow_evolution: ShadowEvolution::new(shadow_cfg, &[]),
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
        let Some(db) = self.order_db.as_ref() else {
            return 0;
        };
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
        let Some(db) = self.position_db.as_ref() else {
            return 0;
        };
        let loaded = db.load();
        if loaded.is_empty() {
            return 0;
        }
        let n = loaded.len();
        self.positions.restore_open(loaded);
        if n > 0 {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!(
                    "recovered {n} open position(s) from the position log after restart"
                ),
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
        self.engine
            .as_ref()
            .map(|e| e.supported_strategies())
            .unwrap_or_default()
    }
    /// Enabled strategy names.
    pub fn enabled_strategy_names(&self) -> Vec<String> {
        self.engine
            .as_ref()
            .map(|e| e.enabled_strategies())
            .unwrap_or_default()
    }
    /// Toggle a strategy on the driving engine. Returns false when the engine
    /// is not attached or the name is unknown.
    ///
    /// A toggle changes which strategies Shadow Evolution watches (it registers
    /// around the enabled set), so the manager is rewired immediately: switching a
    /// strategy on brings its parameters and variants in, switching it off drops
    /// them. No restart, and nothing is traded by a strategy that is off.
    pub fn set_strategy_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let ok = self
            .engine
            .as_mut()
            .map(|e| e.set_strategy_enabled(name, enabled))
            .unwrap_or(false);
        if ok {
            self.rewire_hot_params();
        }
        ok
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
                return "Failed: strategy engine not attached (load libraries after engine init)"
                    .into();
            };
            match load_foreign(p) {
                Ok(loaded) => {
                    let name = loaded.name.clone();
                    let version = loaded.version.clone();
                    // E2-b: the load receipt names the gates the library declared
                    // unnecessary, so an opt-out is visible before it is enabled.
                    let declared = if loaded.gate_exemptions.any() {
                        format!(
                            "; declares gate exemptions: {}",
                            loaded.gate_exemptions.gates().join(",")
                        )
                    } else {
                        String::new()
                    };
                    // E2-c: the same receipt names the evolvable knobs, so "not
                    // evolvable" (no optional symbol) is distinguishable at load
                    // from "evolvable but nothing applied yet".
                    let evolvable = if loaded.evolvable_knobs.is_empty() {
                        "; not evolvable (no knobs declared)".to_string()
                    } else {
                        let names: Vec<&str> = loaded
                            .evolvable_knobs
                            .iter()
                            .map(|k| k.name.as_str())
                            .collect();
                        format!("; declares evolvable knobs: {}", names.join(","))
                    };
                    match engine.register_user_strategy(
                        Box::new(loaded.strategy),
                        format!("dylib:{}", p.display()),
                    ) {
                        Ok(_) => {
                            // The new strategy's declaration must reach the manager
                            // too: a library loaded AFTER evolution was enabled would
                            // otherwise never get a unit, so it could not evolve at
                            // all until a restart.
                            self.rewire_hot_params();
                            format!(
                                "{name}@{version} registered into the engine dispatch (disabled{declared}{evolvable})"
                            )
                        }
                        Err(reason) => format!("rejected: {reason}"),
                    }
                }
                Err(outcome) => format!("{outcome:?}"),
            }
        }
        #[cfg(not(feature = "strategy-loading"))]
        {
            let outcome = crate::strategy_engine::loader::load_strategy(std::path::Path::new(path));
            format!("{outcome:?}")
        }
    }

    /// Unload a dynamic strategy library (E9-b): drop its instance from the
    /// live dispatch and `dlclose` it. The engine refuses in-tree or still-
    /// enabled strategies; here we additionally refuse an OPEN POSITION held
    /// under this strategy's name — a strategy that still owns exposure is not
    /// removed from under the exit machinery. Auditable by design: the outcome
    /// string is what the operator sees on the IPC receipt.
    pub fn unload_strategy(&mut self, name: &str) -> String {
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == name)
            .count();
        if open > 0 {
            return format!(
                "rejected: {name} still holds {open} open position(s) — close them before unloading"
            );
        }
        match self
            .engine
            .as_mut()
            .map(|e| e.unregister_user_strategy(name))
        {
            Some(Ok(true)) => format!("{name} unloaded (engine dispatch removed); library freed"),
            Some(Ok(false)) => format!("not found: no strategy named {name}"),
            Some(Err(reason)) => format!("rejected: {reason}"),
            None => "Failed: strategy engine not attached".into(),
        }
    }

    /// Load + swap in one call (E9-b `strategy.reload`): load the NEW library
    /// file first, and only when it parses and registers the SAME name does
    /// the old instance drop. A reload failure leaves the old instance live —
    /// a broken build can never blank a running dispatch. Because the swap is
    /// name-keyed, after-reload enable state is preserved.
    pub fn reload_strategy(&mut self, name: &str, path: &str) -> String {
        // 1. The old one must be removable-by-name (enabled/in-tree/open-pos
        //    guards all run below, BEFORE the new instance replaces it).
        let source = self
            .engine
            .as_ref()
            .map(|e| e.strategy_source(name).map(|s| s.to_string()))
            .unwrap_or(None);
        let Some(old_source) = source else {
            return format!("not found: no strategy named {name}");
        };
        let was_enabled = self.enabled_names_contains(name);
        // Temporary disable is NOT needed — the swap runs below; but a still-
        // enabled strategy cannot be swapped by the engine's guard, so the
        // load is tried first and the guard happens after.
        let open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.strategy == name)
            .count();
        if open > 0 {
            return format!(
                "rejected: {name} still holds {open} open position(s) — a reload with exposure in flight is undefined; close first"
            );
        }

        // 2. Load the new instance in a SCRATCH seat (a temp name would confuse
        //    profiles; instead we stage the strategy outside the engine only if
        //    the name differs). The kernel's loader gives us the Received view;
        //    both old and new live in the dispatch under the SAME name below.
        let old = self.unload_strategy(name);
        if !old.starts_with(name) && !old.contains("unloaded") {
            // rejected/not-found/Failed — refuse before loading new
            return format!("reload aborted: {old}");
        }
        let receipt = self.load_strategy_lib(path);
        if !receipt.contains("registered") {
            return format!("reload FAILED (old instance already dropped): {receipt}");
        }
        // Preserve previous enable state.
        if was_enabled {
            self.set_strategy_enabled(name, true);
        }
        let _ = old_source; // receipt text already carries the new source
        format!("reload OK: {receipt}")
    }

    fn enabled_names_contains(&self, name: &str) -> bool {
        self.enabled_strategy_names().contains(&name.to_string())
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
    ) -> Vec<(
        String,
        crate::extension::ExtensionType,
        crate::extension::ExtensionState,
    )> {
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
        let ctx = CoreExtensionContext {
            tx: self.tx.clone(),
            strategies: self.strategy_names(),
        };
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

    /// All-time closed-trade summary for the UI (totalTrades / wins / losses /
    /// totalNetPnl / …). Prefers the persisted summary tracked by the trade db;
    /// falls back to totals recomputed over the JSONL (or in-memory history)
    /// when no db is attached.
    pub fn trade_summary(&self) -> serde_json::Value {
        if let Some(db) = self.trade_db.as_ref() {
            return serde_json::to_value(db.summary()).unwrap_or(serde_json::Value::Null);
        }
        // Fallback: aggregate over whatever history we can reach.
        let empty = Vec::new();
        let rows = self.recent_trades(0);
        let rows = if rows.is_empty() { &empty } else { &rows };
        let mut s = serde_json::json!({
            "totalTrades": 0u64, "wins": 0u64, "losses": 0u64, "winRate": 0.0,
            "totalGrossPnl": 0.0, "totalFees": 0.0, "totalNetPnl": 0.0,
            "avgHoldTimeSec": 0.0, "bestTradePnl": 0.0, "worstTradePnl": 0.0,
        });
        let (mut wins, mut losses, mut gross, mut fees, mut net, mut best, mut worst, mut holds) = (
            0u64,
            0u64,
            0.0,
            0.0,
            0.0,
            f64::NEG_INFINITY,
            f64::INFINITY,
            0.0,
        );
        for r in rows.iter() {
            let num = |k: &str| r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let net_i = num("netPnlUsd");
            if net_i >= 0.0 {
                wins += 1
            } else {
                losses += 1
            }
            gross += num("grossPnlUsd");
            fees += num("feesUsd");
            net += net_i;
            best = best.max(net_i);
            worst = worst.min(net_i);
            holds += num("holdTimeSec");
        }
        let n = (wins + losses) as f64;
        s["totalTrades"] = serde_json::json!(wins + losses);
        s["wins"] = serde_json::json!(wins);
        s["losses"] = serde_json::json!(losses);
        s["winRate"] = serde_json::json!(if n > 0.0 {
            wins as f64 * 100.0 / n
        } else {
            0.0
        });
        s["totalGrossPnl"] = serde_json::json!(gross);
        s["totalFees"] = serde_json::json!(fees);
        s["totalNetPnl"] = serde_json::json!(net);
        s["avgHoldTimeSec"] = serde_json::json!(if n > 0.0 { holds / n } else { 0.0 });
        s["bestTradePnl"] = serde_json::json!(if rows.is_empty() { 0.0 } else { best });
        s["worstTradePnl"] = serde_json::json!(if rows.is_empty() { 0.0 } else { worst });
        s
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
        let start = if limit > 0 && closed.len() > limit {
            closed.len() - limit
        } else {
            0
        };
        closed[start..]
            .iter()
            .map(|c| {
                serde_json::to_value(crate::trade_db::TradeRecord::from_closed(c))
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect()
    }

    // ── Shadow Evolution (opt-in) ───────────────────────────────────────────
    pub fn shadow_evolution(&self) -> &ShadowEvolution {
        &self.shadow_evolution
    }

    /// Operator override for ONE strategy: validate + hot-swap + audit. `params`
    /// must name exactly one strategy; a multi-strategy bag is refused rather than
    /// partially applied.
    pub fn shadow_evolution_apply(
        &mut self,
        params: MutableParams,
        now_ms: i64,
    ) -> Result<(), String> {
        self.shadow_evolution.apply_params(params, now_ms)
    }

    /// Attach the per-strategy parameter registry to the driving engine so an
    /// evolution takes effect on the next tick with no restart. Also re-reads the
    /// strategies' knob declarations (their twins are built from them).
    ///
    /// While evolution is DISABLED the overlay is detached (`None`): strategies
    /// then run on the config the host pushed, which is byte-for-byte the
    /// pre-evolution behaviour (acceptance: "关闭进化时行为与改动前一致").
    fn rewire_hot_params(&mut self) {
        if let Some(e) = self.engine.as_mut() {
            if self.shadow_evolution.is_enabled() {
                // The whole host set, enabled or not: a declaration is read off the
                // live instance (E2-c), and `register_strategies` removes the cell of
                // anything ABSENT — so filtering by `enabled` here would delete a
                // switched-off strategy's evolved parameters and rollback anchor,
                // making a toggle lossy. A disabled strategy simply never emits the
                // candidates that would consume them.
                self.shadow_evolution
                    .register_strategies(&e.strategy_refs());
                e.set_hot_params(Some(self.shadow_evolution.registry()));
            } else {
                e.set_hot_params(None);
            }
        }
    }

    pub fn shadow_evolution_enable(&mut self, now_ms: i64) -> bool {
        self.shadow_evolution.enable(now_ms);
        self.rewire_hot_params();
        true
    }

    pub fn shadow_evolution_disable(&mut self) -> bool {
        self.shadow_evolution.disable();
        self.rewire_hot_params();
        true
    }

    /// Aggregate status across every evolvable strategy (kept for callers that
    /// predate per-strategy status). `shadow_evolution_status_for` is the precise
    /// per-strategy query.
    pub fn shadow_evolution_status(&self, now_ms: i64) -> EvolutionStatus {
        self.shadow_evolution.aggregate_status(now_ms)
    }

    /// One strategy's status (`None` = not evolvable).
    pub fn shadow_evolution_status_for(
        &self,
        strategy: &str,
        now_ms: i64,
    ) -> Option<EvolutionStatus> {
        self.shadow_evolution.status(strategy, now_ms)
    }

    /// Roll back ONE strategy to the parameters in force before its last change.
    pub fn shadow_evolution_rollback(
        &mut self,
        strategy: &str,
        now_ms: i64,
    ) -> Result<EvolutionOutcome, String> {
        self.shadow_evolution.rollback(strategy, now_ms)
    }

    pub fn shadow_evolution_variants(
        &mut self,
        now_ms: i64,
    ) -> Vec<crate::shadow_evolution::VariantView> {
        self.shadow_evolution.variant_views(now_ms)
    }

    /// Audit history. `None` = every strategy; `Some(name)` = that strategy's file.
    pub fn shadow_evolution_history(
        &self,
        strategy: Option<&str>,
        limit: usize,
    ) -> Vec<crate::shadow_evolution::audit::AuditRecord> {
        self.shadow_evolution.history(strategy, limit)
    }

    /// Feed the evolution engine a round's markets plus the opening book seeds the
    /// live engine is about to replay, so every twin's confirmation clock starts on
    /// the same tick as the live strategy's.
    pub fn shadow_evolution_on_round(
        &mut self,
        markets: &[CryptoMarket],
        seeds: &[(String, crate::model::OrderbookSnapshot)],
        now_ms: i64,
    ) {
        self.shadow_evolution.on_round(markets, seeds, now_ms);
    }

    /// Feed the evolution engine a book tick (observation only). Twins derive
    /// their own trend state from the books they see, exactly as an external
    /// strategy does, so no externally-computed confirmation flag is passed in.
    pub fn shadow_evolution_on_tick(
        &mut self,
        token_id: &str,
        book: &crate::model::OrderbookSnapshot,
        now_ms: i64,
    ) {
        self.shadow_evolution.on_tick(token_id, book, now_ms);
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
            EvolutionOutcome::RolledBack { strategy, from, to } => {
                // Rollback restores the pre-change parameters for ONE strategy; the
                // audit file already records it. Emitting an applied-shaped signal
                // keeps the UI's "parameters changed" path single.
                self.emit(Event::EvolutionApplied {
                    signal: crate::shadow_evolution::EvolveSignal::new(
                        format!("rollback-{strategy}"),
                        crate::ipc::server::now_ms(),
                        strategy,
                        from,
                        to,
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
        // E2-c: each strategy declares its own evolvable knobs, so the manager can
        // only build its units once the strategies exist. This also attaches the
        // parameter registry, so installing an engine never leaves the overlay
        // unwired.
        self.rewire_hot_params();
    }
    pub fn has_engine(&self) -> bool {
        self.engine.is_some()
    }

    /// Whether the driving engine currently holds a Shadow Evolution parameter
    /// overlay (E2-c / #28). `false` while evolution is off, because the overlay
    /// is DETACHED rather than merely ignored — the observable form of the
    /// acceptance criterion "evolution off ⇒ behaviour unchanged". A strategy can
    /// then only read the config the host pushed through `on_config`.
    pub fn has_hot_params(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.has_hot_params())
    }

    /// Install the running data feed's subscription control (P4). Called by the
    /// market plugin when its `DataFeed` starts, so the core stays unaware of
    /// any concrete feed implementation.
    pub fn set_subscription(
        &mut self,
        control: std::sync::Arc<dyn blitzkrieg_market_api::SubscriptionControl>,
    ) {
        self.feed = Some(control);
    }

    /// Subscribe the round's tokens on the running orderbook feed (P4).
    /// No-op when feeds are disabled.
    pub async fn subscribe_feed_tokens(&self, tokens: Vec<String>) {
        if let Some(feed) = &self.feed
            && !tokens.is_empty()
        {
            feed.set_tokens(tokens);
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
        let mirrored_token = match &ev {
            crate::engine::DataEvent::Book {
                token_id,
                bids,
                asks,
                ..
            } => {
                self.books.insert(
                    token_id.clone(),
                    crate::sim::Book {
                        bids: bids.clone(),
                        asks: asks.clone(),
                    },
                );
                Some(token_id.clone())
            }
            crate::engine::DataEvent::TopOfBook {
                token_id,
                best_bid,
                best_ask,
                ..
            } => {
                let mut bids = Vec::new();
                let mut asks = Vec::new();
                if let Some(b) = best_bid
                    && *b > Decimal::ZERO
                {
                    bids.push((*b, Decimal::ONE));
                }
                if let Some(a) = best_ask
                    && *a > Decimal::ZERO
                {
                    asks.push((*a, Decimal::ONE));
                }
                // Only overwrite when we actually have a side (don't clobber a
                // full book with an empty top-of-book update).
                if !bids.is_empty() || !asks.is_empty() {
                    self.books
                        .insert(token_id.clone(), crate::sim::Book { bids, asks });
                    Some(token_id.clone())
                } else {
                    None
                }
            }
            _ => None,
        };

        // KI-1: the market path must also settle resting maker orders the way
        // `book_snapshot` does — a crossing feed event is exactly when a
        // dry maker fill would happen at the venue, and skipping it made every
        // dry entry escalate to taker (a systematically more expensive economy).
        if let Some(token) = mirrored_token
            && self.config.mode.settles_locally()
        {
            let ids: Vec<String> = self
                .ome
                .live_orders()
                .into_iter()
                .filter(|o| o.token_id == token && rests_on_book(o.mode))
                .map(|o| o.order_id.clone())
                .collect();
            for id in ids {
                self.try_maker_fill(&id, now_ms);
            }
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
                crate::engine::DataEvent::Book {
                    token_id,
                    now_ms: t,
                    ..
                }
                | crate::engine::DataEvent::TopOfBook {
                    token_id,
                    now_ms: t,
                    ..
                } => Some(ShadowFeed::Book(token_id.clone(), *t)),
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
                let book = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.book_snapshot(&token_id));
                if let Some(book) = book {
                    self.shadow_evolution_on_tick(&token_id, &book, t);
                }
            }
            Some(ShadowFeed::Round(markets, t)) => {
                // Seed the twins with the same opening mids the live engine replays
                // for this round (engine.rs RoundMarkets), so confirmation clocks
                // start together instead of one book late.
                let tokens: Vec<String> = markets
                    .iter()
                    .flat_map(|m| [m.up_token_id.clone(), m.down_token_id.clone()])
                    .collect();
                let seeds: Vec<(String, crate::model::OrderbookSnapshot)> = self
                    .engine
                    .as_ref()
                    .map(|e| {
                        tokens
                            .into_iter()
                            .filter_map(|t| e.book_snapshot(&t).map(|b| (t, b)))
                            .collect()
                    })
                    .unwrap_or_default();
                self.shadow_evolution_on_round(&markets, &seeds, t)
            }
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
        // E2-b: an honoured gate exemption is always logged (and counted), so an
        // opt-out is auditable rather than a silent hole in the entry gates.
        for rec in engine.take_exemptions() {
            tracing::info!(target: "strategy", "{}", rec.audit_line());
        }
        self.stats.signals += orders.len() as u64;
        let tokens: Vec<(String, crate::model::OrderRequest)> = orders
            .into_iter()
            .map(|o| (o.token_id.clone(), o))
            .collect();
        let mut placed = 0;
        for (token, req) in tokens {
            let name = req.strategy.clone();
            if let Some(limit) = self.config.strategy_limits.get(&name).cloned()
                && let Err(reason) = self.strategy_limit_ok(&req, &limit)
            {
                // E9-c: the reason existed before but was discarded — bucket
                // it so operator tooling can answer "why did this strategy
                // stop placing?".
                let bucket = if reason.contains("position cap") {
                    "limit.positionCap"
                } else if reason.contains("notional cap") {
                    "limit.notionalCap"
                } else {
                    "limit.other"
                };
                self.stats.strategy_limit_rejected += 1;
                let acc = self.strategy_accounting.entry(name).or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes.entry(bucket.into()).or_default() += 1;
                continue;
            }
            match self.place(req, self.config.entry_maker_timeout_ms, now_ms) {
                Ok((_id, _)) => {
                    self.strategy_accounting.entry(name).or_default().placed += 1;
                    if let Some(engine) = self.engine.as_mut() {
                        engine.note_order_placed(&token);
                    }
                    placed += 1;
                }
                Err(err) => {
                    self.stats.place_rejected += 1;
                    let bucket = classify_rejection(&err);
                    // E9-c: attach the live reason text verbatim to the receipt
                    // log line so per-strategy accounting and logs tell the
                    // same story.
                    tracing::info!(target: "strategy",
                        "entry rejected: strategy={name} cause={bucket} reason={}",
                        err.message);
                    let acc = self.strategy_accounting.entry(name).or_default();
                    acc.rejected += 1;
                    *acc.rejection_causes.entry(bucket).or_default() += 1;
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
        if let Some(max_pos) = limit.max_open_positions
            && open.len() >= max_pos
        {
            return Err(format!(
                "strategy {} at position cap ({}/{})",
                req.strategy,
                open.len(),
                max_pos
            ));
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
                let acc = self
                    .strategy_accounting
                    .get(&name)
                    .cloned()
                    .unwrap_or_default();
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
                // E2-b: what this strategy DECLARED it does not need, next to how
                // often a gate actually stopped it and how often an exemption let
                // a candidate through — so an opt-out is never invisible beside an
                // un-exempted strategy's rejections.
                let declared = self
                    .engine
                    .as_ref()
                    .and_then(|e| e.strategy_gate_exemptions(&name))
                    .unwrap_or_default();
                let tally = self
                    .engine
                    .as_ref()
                    .map(|e| e.gate_tally(&name))
                    .unwrap_or_default();
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
                    // ── E2-b declared gate exemptions + per-strategy gate counts ──
                    "gateExemptions": declared.gates(),
                    "blockedTiming": tally.blocked_timing,
                    "blockedMomentum": tally.blocked_momentum,
                    "gateExemptedTiming": tally.exempted_timing,
                    "gateExemptedMomentum": tally.exempted_momentum,
                    "ordersPlaced": acc.placed,
                    "ordersRejected": acc.rejected,
                    "limitRejected": acc.limit_rejected,
                    // E9-c: cause → count; only counted buckets appear.
                    "rejectionCauses": if acc.rejection_causes.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(acc.rejection_causes)
                    },
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
                // Per-strategy attribution (E2-b / #27): the totals stay global,
                // and `byStrategy` names whose candidates each block belonged to.
                let mut by_strategy = serde_json::Map::new();
                for name in self.strategy_names() {
                    let t = e.gate_tally(&name);
                    if t.blocked_timing == 0 && t.blocked_momentum == 0 {
                        continue;
                    }
                    by_strategy.insert(
                        name,
                        serde_json::json!({
                            "timing": t.blocked_timing,
                            "momentum": t.blocked_momentum,
                        }),
                    );
                }
                let declared: Vec<serde_json::Value> = e
                    .declared_gate_exemptions()
                    .into_iter()
                    .map(|(name, x)| serde_json::json!({ "strategy": name, "gates": x.gates() }))
                    .collect();
                serde_json::json!({
                    "timing": e.blocked_timing_count(),
                    "momentum": e.blocked_momentum_count(),
                    "byStrategy": by_strategy,
                    "declaredExemptions": declared,
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
                MarketPriceView {
                    asset: m.asset.clone(),
                    up,
                    down,
                }
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
                // Cash actually moved on this position, so an external monitor
                // can check the ledger identity without waiting for a flat book.
                // `entry_cost_usd` is the TOTAL entry notional (what left the
                // ledger); `cost_usd` is only the basis still held, so it is the
                // former the identity must sum.
                entry_cost_usd: p.flows.entry_cost_usd,
                cost_usd: p.cost_usd,
                entry_fee_usd: p.flows.entry_fee_usd,
                proceeds_usd: p.flows.proceeds_usd,
                exit_fee_usd: p.flows.exit_fee_usd,
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
        self.ome.submit(SubmitParams {
            order_id: id.clone(),
            request: req,
            submitted_at_ms: now_ms,
        })?;
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
    ///
    /// E17: the fee follows the fill's RESOLVED role (`d.role`), and the cash
    /// movement and the position accrual are computed from ONE fee value, so the
    /// ledger and the trade record cannot disagree about what was paid.
    fn apply_delta_effects(&mut self, d: FillDelta, now_ms: i64) {
        let px = d.price;
        // A maker fill pays no fee; a taker fill pays taker_fee_pct(price) on the
        // fill's own notional. `d.role` is what the fill actually did — the old
        // `match d.mode` read the REQUESTED policy, which charged a
        // MakerThenTaker order the taker fee while its position record said maker.
        // Fee applies to the incremental delta (not the cumulative), so partial
        // fills charge exactly once per share; rollbacks (delta <= 0) charge none.
        let mut fee_usd = Decimal::ZERO;
        if d.delta > Decimal::ZERO {
            let notional = px * d.delta;
            let fee_pct = if d.role.is_maker() {
                Decimal::ZERO
            } else {
                crate::exit_policy::taker_fee_pct(px)
            };
            fee_usd = (fee_pct / Decimal::ONE_HUNDRED) * notional;
            match d.side {
                Side::Buy => {
                    self.ledger.settle_buy_fill(&d.order_id, notional);
                    self.ledger.charge_fee(fee_usd);
                }
                Side::Sell => self.ledger.settle_sell_fill(notional, fee_usd),
            }
        } else {
            // Rollback/correction: revert the cash with no fee.
            match d.side {
                Side::Buy => self.ledger.settle_sell_fill(-px * d.delta, Decimal::ZERO),
                Side::Sell => self.ledger.settle_buy_fill(&d.order_id, -px * d.delta),
            }
        }
        self.project_fill_delta(&d, fee_usd, now_ms);
        self.emit_fill(d);
    }

    /// Keep the position book in sync with fills: BUY opens/averages-in, SELL
    /// reduces/closes. One position per token, so token_id identifies it.
    ///
    /// `fee_usd` is the cash fee the ledger just charged for this same delta, so
    /// the position's cost basis is the money actually spent (E17-d).
    fn project_fill_delta(&mut self, d: &FillDelta, fee_usd: Decimal, now_ms: i64) {
        let px = d.price;
        let token = &d.token_id;
        match d.side {
            Side::Buy => {
                let existing = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| &p.token_id == token)
                    .map(|p| p.id.clone());
                let id = match existing {
                    Some(id) => id,
                    None => {
                        let direction = parse_direction(&d.direction);
                        // A round slot N covers [N*dur, (N+1)*dur): the market
                        // expires at the END of the slot. (Using N*dur would place
                        // expiry in the past and force-exit immediately.)
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
                            expires_at_ms,
                            was_maker: d.role.is_maker(),
                            target_exit_price: None,
                        };
                        self.positions.open(p, now_ms).id
                    }
                };
                // Accrue the basis, the fee and the role from the actual fill.
                // `d.delta` is SIGNED: a FAILED rollback reverses the accrual by
                // the same amounts the ledger just refunded, so a rolled-back
                // entry leaves neither cash nor shares behind.
                self.positions
                    .apply_entry_fill(&id, d.delta, px, fee_usd, d.role);
                // A fully rolled-back entry has no shares and no basis: drop the
                // shell so it cannot block `can_open` or render as an empty row.
                self.positions.drop_if_empty(&id);
                self.persist_positions();
            }
            Side::Sell => {
                let found = self
                    .positions
                    .open_positions()
                    .iter()
                    .find(|p| &p.token_id == token)
                    .map(|p| p.id.clone());
                match found {
                    Some(id) => {
                        // Accrue the exit first, so the close reads final flows.
                        let left = self
                            .positions
                            .apply_exit_fill(&id, d.delta, px, fee_usd, d.role);
                        // A reversal that is already accounted for on a CLOSED
                        // trade has nothing open to accrue onto; the cash is
                        // refunded but the realized record stands. Say so — a
                        // silent drop here would be exactly the drift E17 exists
                        // to remove.
                        if d.delta < Decimal::ZERO && left.is_none() {
                            self.emit(Event::Error {
                                error: CoreError::new(
                                    CoreErrorCode::Internal,
                                    format!(
                                        "exit rollback {} for token {token} has no open position to reverse",
                                        d.order_id
                                    ),
                                ),
                            });
                            return;
                        }
                        match left {
                            // Sub-grid remainder: nothing sellable is left, so the
                            // position is done — it must close, or it would sit on
                            // the books forever with an untradeable stub of shares.
                            Some(left)
                                if left <= Decimal::ZERO
                                    || crate::position::floor_to_grid(left) == Decimal::ZERO =>
                            {
                                let reason = self
                                    .exit_reasons
                                    .remove(token)
                                    .unwrap_or(ExitReason::Manual);
                                let was_maker = d.role.is_maker();
                                if let Some(closed) =
                                    self.positions.close(&id, px, reason, was_maker, now_ms)
                                {
                                    self.persist_positions();
                                    self.on_position_closed(&closed, now_ms);
                                }
                            }
                            Some(_) => {
                                self.persist_positions();
                            }
                            None => {}
                        }
                    }
                    None if d.delta < Decimal::ZERO => {
                        // No position at all for the token: the reversal cannot be
                        // applied. Never swallow it.
                        self.emit(Event::Error {
                            error: CoreError::new(
                                CoreErrorCode::Internal,
                                format!(
                                    "exit rollback {} for token {token} has no open position to reverse",
                                    d.order_id
                                ),
                            ),
                        });
                    }
                    None => {}
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
            let acc = self
                .strategy_accounting
                .entry(closed.strategy.clone())
                .or_default();
            acc.closed_trades += 1;
            if closed.net_pnl_usd >= Decimal::ZERO {
                acc.wins += 1;
            } else {
                acc.losses += 1;
            }
            acc.fees_usd += entry_fee + exit_fee;
            acc.net_pnl_usd += closed.net_pnl_usd;
        }
        // KI-10 / D-18 A: the streak is attributed to the position's own
        // strategy, so only that leg freezes. `PositionClosed` already carries
        // the strategy name, so no new data is needed for attribution.
        let tripped = self
            .breaker
            .record(&closed.strategy, closed.net_pnl_usd, now_ms);
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
                    "consecutive-loss breaker tripped for strategy {}: {} losses, halting its new entries {}s",
                    closed.strategy,
                    self.breaker.consecutive_losses(&closed.strategy),
                    self.config.breaker_cooldown_sec
                ),
            });
        }
    }

    /// Map a venue order id to a core order (for user-WS events). Returns the
    /// core id when known.
    pub fn core_id_for_venue(&self, venue_or_core: &str) -> Option<String> {
        self.ome
            .by_venue_or_id(venue_or_core)
            .map(|o| o.order_id.clone())
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
    pub fn bind_venue(
        &mut self,
        core_id: &str,
        venue_order_id: String,
        now_ms: i64,
    ) -> CoreResult<()> {
        self.ome.bind_venue(core_id, venue_order_id, now_ms)?;
        self.ome.mark_live(core_id, now_ms)?;
        self.emit_order(core_id);
        Ok(())
    }

    /// Run a reconciliation sweep against a venue snapshot. Applies missed fills
    /// and repairs ghost/partial orders, emitting a report event.
    pub fn reconcile(
        &mut self,
        snap: crate::reconcile::VenueSnapshot,
    ) -> CoreResult<crate::reconcile::ReconcileReport> {
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
                    .filter(|a| {
                        matches!(a, crate::reconcile::ReconcileAction::MarkedCancelled { .. })
                    })
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
            self.emit(Event::Fill {
                delta: d.into(),
                order: o.clone(),
            });
        }
    }

    fn new_order_id(&mut self) -> String {
        // "live" is reserved for orders that can actually reach a venue. A
        // read-only order is not live — stamping it `live_` would put a
        // misleading label on the one field an operator reads to decide what is
        // real, so it is named for what it is.
        let prefix = match self.config.mode {
            Mode::Live => "live",
            Mode::ReadOnly => "readonly",
            Mode::Dry => "dry",
        };
        let id = format!("{prefix}_{}", self.next_id);
        self.next_id += 1;
        id
    }

    // ── Risk ───────────────────────────────────────────────────────────────
    pub fn kill(&mut self, reason: String) {
        self.risk.kill(reason.clone());
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::KillSwitchActive,
            message: reason,
        });
    }
    /// Flush pending near-miss records to disk (call on shutdown).
    pub fn flush_near_misses(&mut self) {
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.flush_near_misses())
                .unwrap_or_default();
            if !recs.is_empty()
                && let Err(e) =
                    crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs)
            {
                tracing::warn!(error = %e, path = %path, "near-miss flush failed");
            }
        }
    }

    pub fn resume(&mut self) {
        self.risk.resume();
    }
    pub fn is_killed(&self) -> bool {
        self.risk.is_killed()
    }

    /// Hot-swap the live risk gate configuration without a restart (test and
    /// runtime-config path; `place()` reads it on every check).
    pub fn risk_config_mut(&mut self) -> &mut RiskConfig {
        self.risk.config_mut()
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
        self.place_inner(req, maker_timeout_ms, now_ms, true)
    }

    /// Submit a leg that COMPLETES an already-open position rather than starting
    /// a new one — the maker→taker escalation of the same entry.
    ///
    /// The entry gates (`can_open`, breaker) exist to stop a SECOND position on
    /// an asset. When the maker leg filled only partially, the position already
    /// exists, so `can_open` rejects the escalated remainder with "Already in
    /// {asset}" and the leg is silently dropped: the venue-driven partial fill
    /// stays half-filled forever. A LIVE-only defect — dry maker fills are always
    /// full fills. The risk gate still applies to the escalated notional.
    fn place_escalated(
        &mut self,
        req: OrderRequest,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        self.place_inner(req, 0, now_ms, false)
    }

    fn place_inner(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
        entry_gates: bool,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        self.risk.check(&req)?;

        // Entry gates apply to opening BUY orders only; exits (SELL) are never
        // blocked by capacity, breaker or cooldowns.
        if entry_gates && req.side == Side::Buy {
            // KI-10: only the ORDER'S OWN strategy's streak can block it — a
            // losing leg no longer vetoes every other strategy's entries.
            if self.breaker.is_halted(&req.strategy, now_ms) {
                return Err(CoreError::new(
                    CoreErrorCode::RiskRejected,
                    format!(
                        "breaker active until {} for strategy {}",
                        self.breaker.halted_until_ms(&req.strategy),
                        req.strategy
                    ),
                ));
            }
            let direction = parse_direction(&req.direction);
            if let Err(reason) = self
                .positions
                .can_open(Some(&req.asset), Some(direction), now_ms)
            {
                return Err(CoreError::new(CoreErrorCode::RiskRejected, reason));
            }
        }

        let id = self.new_order_id();

        // Reserve BUY notional up front (prevents over-commitment).
        if req.side == Side::Buy {
            self.ledger.reserve(&id, req.price * req.size)?;
        }

        self.ome.submit(SubmitParams {
            order_id: id.clone(),
            request: req,
            submitted_at_ms: now_ms,
        })?;
        self.place_after_submit(&id, maker_timeout_ms, now_ms)?;

        let status = self
            .ome
            .get(&id)
            .map(|o| o.status)
            .unwrap_or(OrderStatus::Pending);
        self.emit_order(&id);
        Ok((id, status))
    }

    fn place_after_submit(
        &mut self,
        id: &str,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<()> {
        let order =
            self.ome.get(id).cloned().ok_or_else(|| {
                CoreError::new(CoreErrorCode::Internal, "order missing after submit")
            })?;

        match self.config.mode {
            // Dry and ReadOnly share the settle path: neither has a venue that
            // can report a fill, so waiting would leave the order Pending
            // forever and the ledger would never move. What separates them is
            // egress, not accounting — see `Mode::readonly`.
            Mode::Dry | Mode::ReadOnly => {
                self.ome.mark_live(id, now_ms)?;
                match order.mode {
                    FillPolicy::Taker => {
                        // Cross immediately and fully at the buffered limit,
                        // worsened by the fill model's taker slippage (identity by
                        // default, so the live/dry path is unchanged).
                        let fill_price = self
                            .config
                            .fill_model
                            .apply_slippage(order.side, order.price);
                        self.authoritative_fill(id, order.size, fill_price, false, now_ms)?;
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
    ///
    /// `maker` is the role this execution actually had, which the dry matcher
    /// knows exactly: it either crossed a resting bid (`false`) or rested and was
    /// hit (`true`). Stating it keeps DRY and LIVE on the same authority — the
    /// execution itself — instead of DRY inferring from policy and LIVE reading
    /// the venue.
    fn authoritative_fill(
        &mut self,
        id: &str,
        cumulative: Decimal,
        price: Decimal,
        maker: bool,
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
            maker: Some(maker),
        };
        if let Some(d) = self.ome.apply_fill(fill, now_ms)? {
            self.apply_delta_effects(d, now_ms);
        }
        // Event-stream completeness: a fill applied HERE is outside `place`, so
        // `place`'s trailing `emit_order` cannot cover it. Without this final
        // update the order log / backtester only ever saw the resting LIVE state
        // (the Fill event alone carries no terminal status), so a cross-filled
        // maker read as still-open to every event consumer.
        self.emit_order(id);
        Ok(())
    }

    /// Fill a resting maker order if the latest book crosses its limit.
    fn try_maker_fill(&mut self, id: &str, now_ms: i64) {
        let Some(order) = self.ome.get(id).cloned() else {
            return;
        };
        if !order.status.is_live() || order.filled_size >= order.size {
            return;
        }
        let crosses = self
            .books
            .get(&order.token_id)
            .map(|b| b.crosses(&order))
            .unwrap_or(false);
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
        if let Err(e) = self.authoritative_fill(id, order.size, order.price, true, now_ms) {
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
        let targets: Vec<FlattenTarget> = self
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
            if self
                .ome
                .live_for(&token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live())
            {
                continue;
            }
            let price = if current > Decimal::ZERO {
                current
            } else {
                Decimal::new(1, 2)
            };
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

        // Resume entries when a per-strategy breaker cooldown elapses (KI-10):
        // each strategy resumes independently and is reported by name.
        for strategy in self.breaker.maybe_resume_all(now_ms) {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::RiskRejected,
                message: format!(
                    "breaker cooldown elapsed — resuming new entries for strategy {strategy}"
                ),
            });
        }

        // Persist finalized near-miss records (blocked signals + their paths).
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.take_near_misses(now_ms))
                .unwrap_or_default();
            if !recs.is_empty()
                && let Err(e) =
                    crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs)
            {
                tracing::warn!(error = %e, path = %path, "near-miss persist failed");
            }
        }

        // Evaluate exits for every open position and place a closing SELL.
        self.run_exit_checks(now_ms)?;

        // Escalate due maker_then_taker orders: cancel maker, cross as taker.
        let due: Vec<EscalationTarget> = self
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
        for (id, price, remaining, side, token, condition, strategy, asset, direction, slot) in due
        {
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
            self.place_escalated(req, now_ms)?;
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
            let price = if pos.current_price > Decimal::ZERO {
                pos.current_price
            } else {
                Decimal::new(1, 2)
            };
            let tag: String = intent
                .reason
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
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
            let Some(size) = self.positions.sell_shares(&job.position_id) else {
                continue;
            };
            let mode = if job.use_maker {
                FillPolicy::Maker
            } else {
                FillPolicy::Taker
            };
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    /// E9-c: rejection-cause buckets for this strategy. The cause label is the
    /// machine-parseable part (see `classify_rejection`), so an operator can
    /// see WHY entries bounced without grep-ing logs. Only counted buckets are
    /// serialized (empty map → absent from the row).
    rejection_causes: std::collections::BTreeMap<String, u64>,
    closed_trades: u64,
    wins: u64,
    losses: u64,
    fees_usd: Decimal,
    net_pnl_usd: Decimal,
}

/// E9-c: classify a rejection message into a stable bucket label. Messages
/// come from `Risk::check`, `breaker.is_halted`, `positions.can_open`,
/// `strategy_limit_ok` and the ledger/OME reserve paths — matched by their
/// canonical prefixes so wording tweaks elsewhere stay visible: anything
/// unrecognized falls into a `other:<len-capped text>` bucket rather than
/// being silently lumped with a wrong cause.
fn classify_rejection(err: &crate::model::CoreError) -> String {
    let msg = &err.message;
    let code_label = |c: crate::model::CoreErrorCode| {
        use crate::model::CoreErrorCode as E;
        match c {
            E::KillSwitchActive => "killswitch".to_string(),
            E::RiskRejected => "risk".to_string(),
            E::InsufficientFunds => "ledger.reserve".to_string(),
            E::WouldCross | E::InvalidTickSize | E::InvalidSize => "ome".to_string(),
            _ => "other".to_string(),
        }
    };

    if msg.contains("breaker active until") {
        "risk:breaker".to_string()
    } else if msg.starts_with("Max positions")
        || msg.starts_with("Already in")
        || msg.starts_with("Daily loss limit")
    {
        format!(
            "positions.{}",
            msg.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join("_")
                .to_lowercase()
        )
    } else if msg.starts_with("SL cooldown") || msg.starts_with("Exit cooldown") {
        "positions.exitCooldown".to_string()
    } else if msg.starts_with("Loss cooldown") || msg.starts_with("Asset cooldown") {
        "positions.lossCooldown".to_string()
    } else if msg.contains("position cap") || msg.contains("notional cap") {
        "limit.cap".to_string()
    } else if msg.starts_with("notional") && msg.contains("exceeds per-order cap") {
        "risk.perOrderCap".to_string()
    } else if msg.contains("outside") && msg.contains("price band") {
        "risk.priceBand".to_string()
    } else {
        format!("{}.{}", code_label(err.code), "other")
    }
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
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            round_duration_sec: 900,
            auto_exits_enabled: true,
            ..Default::default()
        };
        let mut c = Core::new(cfg);
        c.enable_engine(crate::engine::Engine::new(
            crate::engine::EngineConfig::default(),
        ));
        let now = 1_000_000_000i64;
        let slot = now / 1000 / 900;
        let end = (slot + 1) * 900 * 1000;

        // Feed a round + a book the way the Rust feed layer does.
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![crate::model::CryptoMarket {
                    asset: "BTC".into(),
                    condition_id: "c".into(),
                    question_id: "q".into(),
                    up_token_id: "tok".into(),
                    down_token_id: "d".into(),
                    up_price: dec!(0.6),
                    down_price: dec!(0.4),
                    expires_at_ms: end,
                    round_slot: slot,
                    neg_risk: true,
                    question: "?".into(),
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
            token_id: "tok".into(),
            condition_id: "c".into(),
            side: crate::model::Side::Buy,
            mode: crate::model::FillPolicy::Taker,
            price: dec!(0.43),
            size: dec!(10),
            internal_key: "k".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: slot,
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
        assert_eq!(
            c.positions().open_positions().len(),
            0,
            "exit engine must close on profit"
        );
        let closed = &c.positions().closed_positions()[0];
        assert!(
            closed.exit_price > entry,
            "exit must be above entry (was frozen before fix)"
        );
        assert!(closed.net_pnl_usd > Decimal::ZERO);
    }

    /// KI-1 regression: a resting maker order must fill when the MARKET path
    /// (`engine_on_data`) delivers a crossing book — not only when a
    /// `books.snapshot` RPC arrives. Before the fix the feed path only mirrored
    /// the book, so every dry entry sat until its maker→taker escalation and
    /// dry's fill/fee economy was systematically taker-side.
    #[test]
    fn engine_feed_cross_fills_resting_maker() {
        let cfg = CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            round_duration_sec: 900,
            ..Default::default()
        };
        let mut c = Core::new(cfg);
        let now = 1_000_000_000i64;
        let slot = now / 1000 / 900;
        let end = (slot + 1) * 900 * 1000;

        // Register the round the way the feed does, then seed the book.
        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![crate::model::CryptoMarket {
                    asset: "BTC".into(),
                    condition_id: "c".into(),
                    question_id: "q".into(),
                    up_token_id: "tok".into(),
                    down_token_id: "d".into(),
                    up_price: dec!(0.6),
                    down_price: dec!(0.4),
                    expires_at_ms: end,
                    round_slot: slot,
                    neg_risk: true,
                    question: "?".into(),
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

        // Rest a maker BUY at 0.43 via the normal entry path.
        let req = crate::model::OrderRequest {
            token_id: "tok".into(),
            condition_id: "c".into(),
            side: crate::model::Side::Buy,
            mode: crate::model::FillPolicy::Maker,
            price: dec!(0.43),
            size: dec!(10),
            internal_key: "k".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: slot,
        };
        let id = c.place(req, 0, now).unwrap().0;
        assert_eq!(
            c.ome.get(&id).map(|o| o.status),
            Some(crate::model::OrderStatus::Live),
            "maker rests while the book is above its limit"
        );

        // A feed book whose ask crosses the resting bid: the MAKER path must
        // fill it at the resting limit (maker role — no taker fee).
        c.engine_on_data(
            crate::engine::DataEvent::Book {
                token_id: "tok".into(),
                bids: vec![(dec!(0.41), dec!(100))],
                asks: vec![(dec!(0.42), dec!(100))],
                now_ms: now + 500,
            },
            now + 500,
        );
        let order = c.ome.get(&id).expect("order present");
        assert_eq!(order.status, crate::model::OrderStatus::Filled);
        assert_eq!(order.filled_size, dec!(10));
        assert_eq!(
            order.avg_fill_price,
            Some(dec!(0.43)),
            "fills at the resting limit, not the crossing ask"
        );
        // Maker fills are free (E17): the ledger holds only the notional.
        let bal = c.ledger().balance();
        assert!(
            bal < dec!(1000) && bal > dec!(957),
            "reserved notional 4.30 spent, no taker fee charged (got {bal:?})"
        );
    }

    /// The complementary invariant: in the default fill model a NON-crossing
    /// feed book must NOT fill the resting maker (no phantom fills).
    #[test]
    fn engine_feed_does_not_fill_maker_without_cross() {
        let cfg = CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            round_duration_sec: 900,
            ..Default::default()
        };
        let mut c = Core::new(cfg);
        let now = 1_000_000_000i64;
        c.engine_on_data(
            crate::engine::DataEvent::Book {
                token_id: "tok".into(),
                bids: vec![(dec!(0.43), dec!(100))],
                asks: vec![(dec!(0.45), dec!(100))],
                now_ms: now,
            },
            now,
        );
        let req = crate::model::OrderRequest {
            token_id: "tok".into(),
            condition_id: "c".into(),
            side: crate::model::Side::Buy,
            mode: crate::model::FillPolicy::Maker,
            price: dec!(0.43),
            size: dec!(10),
            internal_key: "k".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: now / 1000 / 900,
        };
        let id = c.place(req, 0, now).unwrap().0;
        // Ask 0.44 > limit 0.43: still resting after many feed ticks.
        for i in 1..5 {
            c.engine_on_data(
                crate::engine::DataEvent::Book {
                    token_id: "tok".into(),
                    bids: vec![(dec!(0.43), dec!(100))],
                    asks: vec![(dec!(0.44), dec!(100))],
                    now_ms: now + i * 100,
                },
                now + i * 100,
            );
        }
        assert_eq!(
            c.ome.get(&id).map(|o| o.status),
            Some(crate::model::OrderStatus::Live),
            "non-crossing feed must not fill"
        );
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
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
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
        assert_eq!(
            pos.expires_at_ms, expected_end,
            "expiry must be the round END"
        );
        assert!(pos.expires_at_ms > now_ms, "expiry must be in the future");
        // time_left must be comfortably positive (not an instant force-exit).
        let time_left = (pos.expires_at_ms - now_ms) / 1000;
        assert!(
            time_left > 0 && time_left <= duration,
            "time_left={time_left}"
        );
    }
}

#[cfg(test)]
mod shadow_evolution_tests {
    use super::*;
    use crate::risk::RiskConfig;
    use crate::shadow_evolution::{MutableParams, StrategyParams};
    use rust_decimal_macros::dec;

    /// One strategy's override bag, built the way the IPC handler builds it.
    fn bag(strategy: &str, knob: &str, value: Decimal) -> MutableParams {
        let mut p = StrategyParams::new();
        p.set(knob, value);
        let mut m = MutableParams::new();
        m.set_strategy(strategy, p);
        m
    }

    fn cap(c: &Core) -> Decimal {
        c.engine
            .as_ref()
            .unwrap()
            .current_spread_arb()
            .trend_max_entry_price
    }

    /// Acceptance: enabling Shadow Evolution attaches the per-strategy hot-swap
    /// registry to the engine, and a parameter change is visible to the engine on
    /// the next read (no restart). Also: enabling is opt-in, apply is per-strategy
    /// and rollback restores the prior set for that strategy only.
    #[test]
    fn hot_swap_reaches_the_engine_and_rolls_back() {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig::default(),
            dry_seed_balance: dec!(1000),
            shadow_evolution_enabled: false,
            ..Default::default()
        });
        c.enable_engine(crate::engine::Engine::new(
            crate::engine::EngineConfig::default(),
        ));
        assert!(!c.shadow_evolution().is_enabled());
        assert!(!c.engine.as_ref().unwrap().has_hot_params());

        // Enable → engine gains the hot-swap handle.
        c.shadow_evolution_enable(1000);
        assert!(c.engine.as_ref().unwrap().has_hot_params());
        assert_eq!(
            c.shadow_evolution().strategy_names(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string()
            ],
            "every hosted strategy that declares knobs is evolved; the chase leg \
             starts disabled but its declaration is read off the live instance"
        );

        let before = cap(&c);
        // Simulate an applied evolution via the operator override path (+3%).
        c.shadow_evolution_apply(
            bag("spread_arb", "trend_max_entry_price", before * dec!(1.03)),
            1500,
        )
        .unwrap();
        assert_eq!(
            cap(&c),
            before * dec!(1.03),
            "engine must observe hot-swapped params"
        );

        // Rollback restores the previous set — for that strategy only.
        c.shadow_evolution_rollback("spread_arb", 2000).unwrap();
        assert_eq!(
            cap(&c),
            before,
            "rollback must restore prior params in the engine"
        );
        assert_eq!(
            c.shadow_evolution().evolution_count("spread_arb"),
            0,
            "rollback is not an evolution"
        );

        // A manual apply is now traced; the audit is per strategy.
        let recs = c.shadow_evolution_history(Some("spread_arb"), 10);
        assert_eq!(recs.len(), 2, "manual apply + rollback");
        assert!(recs[0].manual);
        assert!(recs.iter().all(|r| r.strategy == "spread_arb"));
        assert!(c.shadow_evolution_history(Some("nope"), 10).is_empty());
    }

    /// Apply names exactly one strategy: a multi-strategy bag is refused rather
    /// than partially applied, and an unknown strategy is an error (not a silent
    /// no-op that another strategy's change could be mistaken for).
    #[test]
    fn apply_and_rollback_are_per_strategy() {
        let mut c = Core::new(CoreConfig {
            dry_seed_balance: dec!(1000),
            ..Default::default()
        });
        c.enable_engine(crate::engine::Engine::new(
            crate::engine::EngineConfig::default(),
        ));
        c.shadow_evolution_enable(0);

        let before = cap(&c);
        let mut both = bag("spread_arb", "trend_max_entry_price", before * dec!(1.03));
        both.set_strategy("dog_strategy", StrategyParams::new());
        assert!(
            c.shadow_evolution_apply(both, 1000).is_err(),
            "one strategy at a time"
        );
        assert_eq!(cap(&c), before, "a refused apply must not move anything");

        assert!(
            c.shadow_evolution_apply(bag("nope", "x", dec!(1)), 1100)
                .is_err()
        );
        assert!(c.shadow_evolution_rollback("nope", 1200).is_err());
        assert!(
            c.shadow_evolution_rollback("spread_arb", 1300).is_err(),
            "nothing to roll back yet"
        );

        // An undeclared knob for spread_arb is refused: a proposal can never
        // smuggle in a field the strategy did not open to evolution.
        assert!(
            c.shadow_evolution_apply(bag("spread_arb", "hard_stop_loss_pct", dec!(1)), 1400)
                .is_err()
        );
        assert_eq!(cap(&c), before);
    }

    #[test]
    fn disabled_by_default_is_fully_inert() {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig::default(),
            ..Default::default()
        });
        c.enable_engine(crate::engine::Engine::new(
            crate::engine::EngineConfig::default(),
        ));
        let before = cap(&c);
        assert!(!c.shadow_evolution().is_enabled());
        assert_eq!(c.shadow_evolution().variant_count(), 0);
        assert!(
            !c.engine.as_ref().unwrap().has_hot_params(),
            "no overlay while disabled"
        );
        c.shadow_evolution_evaluate(1000);
        assert_eq!(c.shadow_evolution_history(None, 10).len(), 0);
        // Nothing is registered and nothing moves, so the strategy runs on the
        // config the host pushed — the pre-evolution behaviour exactly.
        assert!(c.shadow_evolution().strategy_names().is_empty());
        assert!(c.shadow_evolution().current_params().is_empty());
        assert!(c.shadow_evolution_status_for("spread_arb", 0).is_none());
        assert_eq!(c.shadow_evolution_status(0), EvolutionStatus::Disabled);
        c.shadow_evolution_apply(bag("spread_arb", "trend_max_entry_price", dec!(0.50)), 1200)
            .unwrap_err();
        assert_eq!(cap(&c), before, "a disabled engine cannot be written to");

        // Enabling registers the declared names and attaches the overlay; the
        // published values are the declaration, not an invented parameter.
        c.shadow_evolution_enable(2000);
        assert_eq!(
            c.shadow_evolution().strategy_names(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string()
            ],
            "E4-a hosts a second evolvable builtin; both declare knobs"
        );
        assert!(c.engine.as_ref().unwrap().has_hot_params());
        assert_eq!(
            c.shadow_evolution()
                .current_params()
                .get("spread_arb", "trend_max_entry_price"),
            Some(before),
        );
        assert_eq!(
            cap(&c),
            before,
            "attaching the overlay must not itself move a value"
        );

        // Disabling DETACHES it again: back to the untouched base config.
        c.shadow_evolution_apply(
            bag("spread_arb", "trend_max_entry_price", before * dec!(1.03)),
            2100,
        )
        .unwrap();
        assert_ne!(cap(&c), before);
        c.shadow_evolution_disable();
        assert!(!c.engine.as_ref().unwrap().has_hot_params());
        assert_eq!(
            cap(&c),
            before,
            "disabling restores the base config exactly"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::RiskConfig;
    use rust_decimal_macros::dec;

    #[test]
    fn strategy_lib_collector_finds_dylibs_and_skips_other_files() {
        let dir = std::env::temp_dir().join(format!("bk-scan-test-{}", std::process::id()));
        let nested = dir.join("target/release");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("liba.dylib"), b"").unwrap();
        std::fs::write(nested.join("libb.so"), b"").unwrap();
        std::fs::write(dir.join("readme.txt"), b"").unwrap();
        std::fs::write(dir.join("Cargo.toml"), b"").unwrap();
        let mut out = Vec::new();
        collect_strategy_libs(&dir, 0, &mut out);
        let mut names: Vec<String> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["liba.dylib", "libb.so"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn strategy_lib_collector_respects_depth_bound() {
        let dir = std::env::temp_dir().join(format!("bk-deep-test-{}", std::process::id()));
        let mut deep = dir.clone();
        for i in 0..8 {
            deep = deep.join(format!("l{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("toodeep.dylib"), b"").unwrap();
        let mut out = Vec::new();
        collect_strategy_libs(&dir, 0, &mut out);
        assert!(out.is_empty(), "beyond MAX_DEPTH must not be collected");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn dry_core(balance: Decimal) -> Core {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(3),
                ..Default::default()
            },
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
        let (id, st) = c
            .place(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
            .unwrap();
        assert_eq!(st, OrderStatus::Filled);
        assert_eq!(c.ome().get(&id).unwrap().filled_size, dec!(5));
        // 10 - 0.4*5 = 8, minus the taker entry fee (1.8% of 2 = 0.036);
        // reservation fully consumed.
        assert_eq!(c.ledger().balance(), Decimal::new(7964, 3));
        assert_eq!(c.ledger().reserved(), dec!(0));
    }

    #[test]
    fn maker_rests_until_book_crosses() {
        let mut c = dry_core(dec!(10));
        // BUY maker 0.40*5 = 2.0 reserved, not filled yet.
        let (id, st) = c
            .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
            .unwrap();
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
        let (id, st) = c
            .place(
                order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
                1000,
                1,
            )
            .unwrap();
        assert_eq!(st, OrderStatus::Live);
        // Book never crosses within the maker window.
        c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2);
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);
        // At timeout: maker cancelled, taker order crosses immediately.
        c.tick(1002).unwrap();
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
        // A new escalated taker order exists and is filled.
        let filled: Vec<_> = c
            .ome()
            .all()
            .into_iter()
            .filter(|o| o.status == OrderStatus::Filled)
            .collect();
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].filled_size, dec!(5));
        assert_eq!(c.ledger().balance(), Decimal::new(7964, 3));
    }

    #[test]
    fn cancel_releases_reservation() {
        let mut c = dry_core(dec!(10));
        let (id, _) = c
            .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
            .unwrap();
        assert_eq!(c.ledger().available(), dec!(8));
        c.cancel(&id, 2).unwrap();
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
        assert_eq!(c.ledger().available(), dec!(10));
    }

    #[test]
    fn risk_and_ledger_reject_oversize_and_kill() {
        let mut c = dry_core(dec!(10));
        // notional 0.4*10 = 4 > cap 3 → rejected before reservation.
        let e = c
            .place(order(FillPolicy::Taker, dec!(0.4), dec!(10), "k1"), 0, 1)
            .unwrap_err();
        assert_eq!(e.code, CoreErrorCode::RiskRejected);

        // Balance gate: only 2 available after a 2 reservation → a 3 buy fails.
        let mut c2 = dry_core(dec!(2));
        let e2 = c2
            .place(order(FillPolicy::Taker, dec!(0.3), dec!(10), "k2"), 0, 1)
            .unwrap_err();
        assert_eq!(e2.code, CoreErrorCode::InsufficientFunds);

        let mut c3 = dry_core(dec!(10));
        c3.kill("manual".into());
        let e3 = c3
            .place(order(FillPolicy::Taker, dec!(0.4), dec!(1), "k3"), 0, 1)
            .unwrap_err();
        assert_eq!(e3.code, CoreErrorCode::KillSwitchActive);
    }

    #[test]
    fn buy_fill_opens_position_and_exit_closes_it() {
        let mut c = dry_core(dec!(100));
        // Taker BUY 0.40 x 5 fills → position opened.
        let (id, st) = c
            .place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 1)
            .unwrap();
        assert_eq!(st, OrderStatus::Filled);
        assert_eq!(c.positions().open_positions().len(), 1);

        // Push a book showing a large profit → tick must emit a closing SELL.
        c.book_snapshot(
            "tok",
            vec![(dec!(0.99), dec!(100))],
            vec![(dec!(1.0), dec!(100))],
            2,
        );
        c.tick(2).unwrap();
        // Exit SELL (taker) crosses immediately at 0.99 → position closed.
        assert_eq!(
            c.positions().open_positions().len(),
            0,
            "position should be closed"
        );
        assert_eq!(c.positions().closed_positions().len(), 1);
        let closed = &c.positions().closed_positions()[0];
        assert!(
            closed.net_pnl_usd > Decimal::ZERO,
            "expected profit, got {}",
            closed.net_pnl_usd
        );
        let _ = id;
    }

    #[test]
    fn forced_exit_closes_at_a_loss() {
        let mut c = dry_core(dec!(100));
        // Buy at 0.40 (round_slot 1 → expires at the END of slot 1 = 1_800_000ms).
        c.place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 0)
            .unwrap();
        assert_eq!(c.positions().open_positions().len(), 1);
        // Push a lower book, then tick at the force-exit horizon (100s left).
        c.book_snapshot(
            "tok",
            vec![(dec!(0.30), dec!(100))],
            vec![(dec!(0.31), dec!(100))],
            1,
        );
        c.tick(1_800_000 - 100).unwrap();
        assert_eq!(
            c.positions().open_positions().len(),
            0,
            "force exit should close the position"
        );
        let closed = &c.positions().closed_positions()[0];
        assert!(
            closed.net_pnl_usd < Decimal::ZERO,
            "expected a loss, got {}",
            closed.net_pnl_usd
        );
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
        c.place(order(FillPolicy::Taker, dec!(0.40), dec!(5), "k1"), 0, 0)
            .unwrap();
        c.book_snapshot(
            "tok",
            vec![(dec!(0.20), dec!(100))],
            vec![(dec!(0.21), dec!(100))],
            1,
        );
        c.tick(1_800_000 - 100).unwrap();
        assert!(c.positions().daily_pnl() < dec!(-1));
        let e = c
            .place(
                order(FillPolicy::Taker, dec!(0.40), dec!(5), "k2"),
                0,
                900_000,
            )
            .unwrap_err();
        assert_eq!(e.code, CoreErrorCode::RiskRejected);
        assert!(
            e.message.to_lowercase().contains("daily"),
            "got {}",
            e.message
        );
    }

    /// KI-10 / D-18 option A end-to-end: three losing closes in ONE strategy must
    /// freeze only that strategy. The old global breaker vetoed every other leg's
    /// entries (TREND_FOLLOW_HOLDOUT_REPORT §3.2: `ordersRejected=655`).
    #[test]
    fn a_losing_streak_freezes_only_its_own_strategy() {
        let mut c = dry_core(dec!(100));
        let mut pc = c.config.positions.clone();
        pc.max_positions = 4;
        pc.max_daily_loss_usd = dec!(1_000); // isolate the consecutive-loss breaker
        pc.stop_loss_cooldown_sec = 0;
        pc.exit_cooldown_sec = 0;
        pc.asset_cooldown_sec = 0;
        pc.loss_cooldown_sec = 0;
        c.positions.set_config(pc);

        let buy = |strategy: &str, asset: &str, token: &str| OrderRequest {
            token_id: token.into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price: dec!(0.40),
            size: dec!(5),
            internal_key: format!("{strategy}:{asset}"),
            strategy: strategy.into(),
            asset: asset.into(),
            direction: "up".into(),
            round_slot: 1,
        };
        let lose_once = |c: &mut Core, asset: &str, token: &str| {
            c.place(buy("dog", asset, token), 0, 0).unwrap();
            c.book_snapshot(
                token,
                vec![(dec!(0.30), dec!(100))],
                vec![(dec!(0.31), dec!(100))],
                1,
            );
            c.tick(1_800_000 - 100).unwrap();
        };
        // Three distinct assets, so per-asset cooldowns cannot mask the breaker.
        for asset in ["A1", "A2", "A3"] {
            lose_once(&mut c, asset, &format!("tok-{asset}"));
        }
        assert_eq!(c.breaker.consecutive_losses("dog"), 3);
        assert!(
            c.breaker.is_halted("dog", 1_800_000),
            "the losing strategy's own breaker must trip"
        );
        assert!(
            !c.breaker.is_halted("trend_follow", 1_800_000),
            "an unrelated strategy must NOT be frozen by another leg's streak"
        );

        // The frozen leg is refused, and the refusal names it (the panel's
        // `risk:breaker` attribution matches on "breaker active until").
        let e = c
            .place(buy("dog", "A4", "tok-A4"), 0, 1_800_000)
            .unwrap_err();
        assert_eq!(e.code, CoreErrorCode::RiskRejected);
        assert!(
            e.message.contains("breaker active until") && e.message.contains("dog"),
            "got {}",
            e.message
        );
        // A DIFFERENT strategy enters normally through the same core.
        c.place(buy("trend_follow", "B1", "tok-B1"), 0, 1_800_000)
            .unwrap();
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
    use crate::shadow_evolution::{MutableParams, StrategyParams};
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
            trend_follow: Default::default(),
            mean_reversion: Default::default(),
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
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
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
        /// Shared entry gates this strategy declares unnecessary (E2-b / #27).
        gates: crate::strategies::GateExemptions,
    }
    impl crate::strategies::EngineStrategy for TargetDip {
        fn name(&self) -> &str {
            &self.name
        }
        fn on_book(&mut self, _t: &str, _s: &crate::model::OrderbookSnapshot, _n: i64) {}
        fn on_round(&mut self, _slot: i64) {}
        fn gate_exemptions(&self) -> crate::strategies::GateExemptions {
            self.gates
        }
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
                    let Some(book) = ctx.fresh_book(token_id) else {
                        continue;
                    };
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

    const DIPS: [(&str, &[&str]); 3] =
        [("small", &["BTC"]), ("mid", &["ETH"]), ("unset", &["SOL"])];

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
        core_with_declared_dips(limits, dips, global_max_positions, 0, |_| {
            crate::strategies::GateExemptions::none()
        })
    }

    /// Same as [`core_with_dips`], but each strategy also declares the shared
    /// entry gates it does not need (E2-b / #27) and the round-timing window is
    /// set up front — the engine takes its scanner config from
    /// `CoreConfig::engine_config()` at construction, so a later `config_mut`
    /// write would not reach it (nor would one reach the `RiskGate`, which
    /// snapshots `config.risk` the same way).
    fn core_with_declared_dips(
        limits: HashMap<String, StrategyLimit>,
        dips: &[(&str, &[&str])],
        global_max_positions: usize,
        min_round_age_sec: i64,
        gates: impl Fn(&str) -> crate::strategies::GateExemptions,
    ) -> Core {
        core_with_declared_dips_and_risk(
            limits,
            dips,
            global_max_positions,
            min_round_age_sec,
            RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            gates,
        )
    }

    /// The full form: also chooses the risk config the gate is built from.
    fn core_with_declared_dips_and_risk(
        limits: HashMap<String, StrategyLimit>,
        dips: &[(&str, &[&str])],
        global_max_positions: usize,
        min_round_age_sec: i64,
        risk: RiskConfig,
        gates: impl Fn(&str) -> crate::strategies::GateExemptions,
    ) -> Core {
        let mut c = Core::new(CoreConfig {
            risk,
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            round_duration_sec: 900,
            min_round_age_sec,
            auto_exits_enabled: false,
            assets: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            positions: crate::position::PositionConfig {
                max_positions: global_max_positions,
                exit: crate::exit_policy::ExitConfig {
                    min_time_left_sec: 0,
                    ..Default::default()
                },
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
                    gates: gates(name),
                }),
                "test".into(),
            )
            .unwrap();
            assert!(eng.set_strategy_enabled(name, true));
        }
        assert!(
            eng.set_strategy_enabled("spread_arb", false),
            "isolate the test strategies"
        );
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
        c.engine_on_data(
            DataEvent::RoundMarkets {
                markets: three_markets(now),
                now_ms: now,
            },
            now,
        );
        feed_dip_on(c, now + 1_000, &["BTC", "ETH", "SOL"]);
    }

    /// Cross the book so the resting maker bid fills and a position opens.
    fn fill(c: &mut Core, asset: &str, now: i64) {
        let token = format!("{}_up", asset.to_lowercase());
        c.book_snapshot(
            &token,
            vec![(dec!(0.42), dec!(100))],
            vec![(dec!(0.42), dec!(100))],
            now,
        );
    }

    fn limits_of(rows: &[(&str, StrategyLimit)]) -> HashMap<String, StrategyLimit> {
        rows.iter()
            .map(|(n, l)| ((*n).to_string(), l.clone()))
            .collect()
    }

    /// One strategy's override bag for the startup-selection tests, built the way
    /// the IPC handler builds it (the shadow-evolution test module has its own).
    fn one_param(strategy: &str, knob: &str, value: Decimal) -> MutableParams {
        let mut p = StrategyParams::new();
        p.set(knob, value);
        let mut m = MutableParams::new();
        m.set_strategy(strategy, p);
        m
    }

    // ── E4-a: startup strategy selection ────────────────────────────────────

    /// Installing an engine with a selection puts the session in exactly the state
    /// an operator would reach with `strategy.enable`/`strategy.disable`: the
    /// selection goes through the same toggle (so Shadow Evolution is rewired the
    /// same way), an unknown name is ignored rather than fatal, and disabling wins
    /// over enabling when both name the same strategy.
    #[test]
    fn startup_selection_matches_a_runtime_toggle() {
        let install = |cfg: CoreConfig| {
            let mut c = Core::new(cfg.clone());
            cfg.install_engine(&mut c);
            c
        };
        let base = CoreConfig {
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            ..Default::default()
        };

        // Default: the incumbent trades, the chase leg does not.
        let plain = install(base.clone());
        assert_eq!(
            plain.enabled_strategy_names(),
            vec!["spread_arb".to_string()]
        );
        assert_eq!(
            plain.strategy_names(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string()
            ],
            "both builtins are hosted; only one is on"
        );

        // Opt in by flag: same result as toggling it at runtime.
        let by_flag = install(CoreConfig {
            enabled_strategies: vec!["trend_follow".into()],
            ..base.clone()
        });
        let mut by_toggle = install(base.clone());
        assert!(by_toggle.set_strategy_enabled("trend_follow", true));

        assert_eq!(
            by_flag.enabled_strategy_names(),
            vec!["spread_arb".to_string(), "trend_follow".to_string()]
        );
        assert_eq!(
            by_flag.enabled_strategy_names(),
            by_toggle.enabled_strategy_names()
        );

        // An explicit "off" wins over an "on" for the same name, and a name the
        // engine does not host is ignored without taking the session down.
        let both = install(CoreConfig {
            enabled_strategies: vec!["trend_follow".into(), "dog_strategy".into()],
            disabled_strategies: vec!["trend_follow".into()],
            ..base.clone()
        });
        assert_eq!(
            both.enabled_strategy_names(),
            vec!["spread_arb".to_string()]
        );
        assert_eq!(
            both.strategy_names(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string()
            ],
            "an unknown name must not register anything"
        );
    }

    /// Shadow Evolution sees every evolvable strategy the engine hosts, whether it
    /// is enabled or not (E2-c): the knob declaration is read off the live
    /// instance, and `register_strategies` drops the cell of anything it is not
    /// handed — so keying the set off `enabled` would make a runtime disable throw
    /// away that strategy's evolved parameters and rollback anchor.
    ///
    /// This is the regression guard for exactly that: toggle the chase leg off and
    /// back on, and the value applied to it must still be in force.
    #[test]
    fn evolution_keeps_the_cell_of_a_disabled_strategy_across_a_toggle() {
        let base = CoreConfig {
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            ..Default::default()
        };
        let mut c = Core::new(base.clone());
        base.install_engine(&mut c);
        c.shadow_evolution_enable(0);
        // Registered from the start even though it starts disabled.
        assert_eq!(
            c.shadow_evolution().strategy_names(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string()
            ],
        );
        assert!(c.shadow_evolution_status_for("trend_follow", 0).is_some());
        // Its declared knobs are the strategy's own, not an invented set.
        let names: Vec<String> = c
            .shadow_evolution()
            .declared_knobs("trend_follow")
            .into_iter()
            .map(|k| k.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "momentum_window_sec".to_string(),
                "min_move_pct".to_string(),
                "min_confirm_price".to_string(),
                "break_price".to_string(),
                "max_entry_price".to_string(),
                "max_spread_pct".to_string(),
            ]
        );

        assert!(c.set_strategy_enabled("trend_follow", true));
        assert!(
            c.shadow_evolution_apply(one_param("trend_follow", "min_move_pct", dec!(3.1)), 1_200)
                .is_ok()
        );
        let evolved = c
            .shadow_evolution()
            .params_for("trend_follow")
            .unwrap()
            .get("min_move_pct");
        assert_eq!(evolved, Some(dec!(3.1)));

        // Off and on again: same cell, same value — a toggle is not a reset.
        assert!(c.set_strategy_enabled("trend_follow", false));
        assert!(c.set_strategy_enabled("trend_follow", true));
        assert_eq!(
            c.shadow_evolution()
                .params_for("trend_follow")
                .unwrap()
                .get("min_move_pct"),
            Some(dec!(3.1)),
            "a runtime toggle must not discard the strategy's evolved value"
        );
    }

    #[test]
    fn three_strategies_size_independently_in_one_cycle() {
        // Global band is [10,10] at a 2.5u budget; two strategies tighten it and
        // one inherits it. All three trade the same cycle without interfering.
        let limits = limits_of(&[
            (
                "small",
                StrategyLimit {
                    size_usd: Some(dec!(1)),
                    min_shares: Some(dec!(2)),
                    max_shares: Some(dec!(2)),
                    ..Default::default()
                },
            ),
            (
                "mid",
                StrategyLimit {
                    size_usd: Some(dec!(2)),
                    min_shares: Some(dec!(4)),
                    max_shares: Some(dec!(4)),
                    ..Default::default()
                },
            ),
        ]);
        let mut c = core_with_dips(limits, &DIPS, 5);
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(
            c.engine_evaluate(now + 1_000),
            3,
            "one entry per strategy: {:#?}",
            c.list_orders()
        );

        let sizes: Vec<(String, Decimal)> = c
            .list_orders()
            .iter()
            .map(|o| (o.strategy.clone(), o.size))
            .collect();
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
                size_usd: Some(dec!(1)),
                min_shares: Some(dec!(2)),
                max_shares: Some(dec!(2)),
                ..Default::default()
            },
        )]);
        let mut c = core_with_dips(
            limits,
            &[("capped", &["BTC", "ETH"]), ("other", &["SOL"])],
            5,
        );
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(
            c.engine_evaluate(now + 1_000),
            1,
            "only `other` may place: {:#?}",
            c.list_orders()
        );
        assert_eq!(
            c.engine_stats()["strategyLimitRejected"],
            2,
            "both capped candidates were rejected"
        );
        assert_eq!(c.list_orders()[0].strategy, "other");

        let stats = c.strategy_stats();
        let capped = strategy_entry(&stats, "capped");
        assert_eq!(capped["ordersPlaced"], 0);
        assert_eq!(
            capped["limitRejected"], 2,
            "counted against the capped strategy only"
        );
        assert_eq!(capped["maxOpenPositions"], 0);
        assert_eq!(
            dec_of(&capped["effectiveMaxShares"]),
            dec!(2),
            "its own lot is 2 shares"
        );
        let other = strategy_entry(&stats, "other");
        assert_eq!(other["ordersPlaced"], 1, "the sibling still trades");
        assert_eq!(other["limitRejected"], 0);
        assert_eq!(
            other["maxOpenPositions"],
            serde_json::Value::Null,
            "unset = uncapped"
        );
        assert_eq!(
            dec_of(&other["effectiveSizeUsd"]),
            dec!(2.5),
            "and inherits the global sizing"
        );
    }

    #[test]
    fn a_strategy_cap_counts_only_its_own_open_positions() {
        // `capped` allows one position and gets exactly one: it takes BTC, fills
        // it, and is then rejected on ETH — while `other`'s SOL position is
        // invisible to `capped`'s quota (and vice versa).
        let limits = limits_of(&[(
            "capped",
            StrategyLimit {
                max_open_positions: Some(1),
                ..Default::default()
            },
        )]);
        let mut c = core_with_dips(
            limits,
            &[("capped", &["BTC", "ETH"]), ("other", &["SOL"])],
            5,
        );
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(
            c.engine_evaluate(now + 1_000),
            3,
            "two capped candidates + one sibling"
        );

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
        c.engine_on_data(
            DataEvent::RoundMarkets {
                markets: three_markets(now),
                now_ms: now,
            },
            now,
        );

        feed_dip_on(&mut c, now + 1_000, &["BTC"]);
        assert_eq!(c.engine_evaluate(now + 1_000), 1, "a takes BTC");
        fill(&mut c, "BTC", now + 2_000);
        feed_dip_on(&mut c, now + 3_000, &["ETH"]);
        assert_eq!(c.engine_evaluate(now + 3_000), 1, "b takes ETH");
        fill(&mut c, "ETH", now + 4_000);
        assert_eq!(
            c.positions().open_positions().len(),
            2,
            "global ceiling reached"
        );

        feed_dip_on(&mut c, now + 5_000, &["SOL"]);
        assert_eq!(
            c.engine_evaluate(now + 5_000),
            0,
            "the third strategy must be gated out"
        );
        assert_eq!(
            c.engine_stats()["placeRejected"],
            1,
            "rejected by the global capacity gate"
        );
        assert_eq!(
            c.engine_stats()["strategyLimitRejected"],
            0,
            "and NOT by a per-strategy quota"
        );
        assert_eq!(c.positions().open_positions().len(), 2);

        let stats = c.strategy_stats();
        // No strategy configured a quota, so all three report uncapped yet the
        // global gate still held the line.
        for name in ["a", "b", "c"] {
            assert_eq!(
                strategy_entry(&stats, name)["maxOpenPositions"],
                serde_json::Value::Null
            );
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
        assert_eq!(
            dec_of(&s["effectiveSizeUsd"]),
            dec!(2.5),
            "notional clamped to the global budget"
        );
        assert_eq!(
            dec_of(&s["effectiveMaxShares"]),
            dec!(10),
            "share ceiling clamped to the global"
        );
        assert_eq!(
            dec_of(&s["effectiveMinShares"]),
            dec!(10),
            "floor raised to the global min"
        );
    }

    // ── E2-b: per-strategy gate opt-out (#27) ───────────────────────────────

    /// Shut for every strategy that did not declare the timing window unnecessary.
    const WINDOW_SHUT: i64 = 10_000;

    #[test]
    fn a_declared_timing_exemption_places_an_order_the_gate_would_have_blocked() {
        let mut c = core_with_declared_dips(
            HashMap::new(),
            &[("fader", &["BTC"]), ("gated", &["ETH"])],
            5,
            WINDOW_SHUT,
            |name| crate::strategies::GateExemptions {
                timing: name == "fader",
                momentum: false,
            },
        );
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(
            c.engine_evaluate(now + 1_000),
            1,
            "only the declaring strategy may trade: {:#?}",
            c.list_orders()
        );
        assert_eq!(c.list_orders()[0].strategy, "fader");
        assert_eq!(
            c.engine_stats()["blocked"]["timing"],
            1,
            "the sibling stayed gated"
        );

        let stats = c.strategy_stats();
        let fader = strategy_entry(&stats, "fader");
        assert_eq!(fader["gateExemptions"], serde_json::json!(["timing"]));
        assert_eq!(
            fader["gateExemptedTiming"], 1,
            "the honoured exemption is counted"
        );
        assert_eq!(fader["blockedTiming"], 0);
        let gated = strategy_entry(&stats, "gated");
        assert_eq!(gated["gateExemptions"], serde_json::json!([]));
        assert_eq!(gated["blockedTiming"], 1);
        assert_eq!(gated["gateExemptedTiming"], 0);
        assert_eq!(gated["ordersPlaced"], 0);
    }

    #[test]
    fn blocked_gate_counts_are_attributable_to_a_specific_strategy() {
        let mut c = core_with_declared_dips(
            HashMap::new(),
            &[("a", &["BTC"]), ("b", &["ETH"])],
            5,
            WINDOW_SHUT,
            |_| crate::strategies::GateExemptions::none(),
        );
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 1_000), 0, "both are gated");

        let blocked = c.engine_stats()["blocked"].clone();
        assert_eq!(blocked["timing"], 2, "global total");
        assert_eq!(blocked["byStrategy"]["a"]["timing"], 1, "{blocked}");
        assert_eq!(blocked["byStrategy"]["b"]["timing"], 1, "{blocked}");
        assert_eq!(blocked["byStrategy"]["a"]["momentum"], 0, "{blocked}");
        // Only mean_reversion declares an exemption (momentum) among the hosted
        // builtins; the test strategies declare nothing beyond it.
        assert_eq!(
            blocked["declaredExemptions"],
            serde_json::json!([{ "strategy": "mean_reversion", "gates": ["momentum"] }])
        );
    }

    #[test]
    fn declared_exemptions_are_reported_in_engine_stats() {
        let c = core_with_declared_dips(
            HashMap::new(),
            &[("fader", &["BTC"]), ("plain", &["ETH"])],
            5,
            0,
            |name| crate::strategies::GateExemptions {
                timing: name == "fader",
                momentum: name == "fader",
            },
        );
        let declared = c.engine_stats()["blocked"]["declaredExemptions"].clone();
        assert_eq!(
            declared,
            serde_json::json!([
                { "strategy": "mean_reversion", "gates": ["momentum"] },
                { "strategy": "fader", "gates": ["timing", "momentum"] }
            ]),
            "hosted and user declarations are both listed"
        );
    }

    #[test]
    fn a_gate_exemption_never_bypasses_a_safety_boundary() {
        // The strongest form of the acceptance criterion: a strategy that declares
        // BOTH entry gates unnecessary still cannot get past the global risk gate,
        // the kill switch or the global capacity ceiling — those live in
        // `Core::place`, far below the entry gates.
        // The risk gate snapshots `config.risk` at construction, so the per-order
        // notional ceiling is set here: 1 USD vs the 0.44 x 10 = 4.40 order.
        let mut c = core_with_declared_dips_and_risk(
            HashMap::new(),
            &[("all_in", &["BTC"])],
            5,
            WINDOW_SHUT,
            RiskConfig {
                max_order_notional: dec!(1),
                ..Default::default()
            },
            |_| crate::strategies::GateExemptions::all(),
        );
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);

        assert_eq!(
            c.engine_evaluate(now + 1_000),
            0,
            "risk gate must still reject"
        );
        assert_eq!(c.engine_stats()["placeRejected"], 1);
        assert!(c.list_orders().is_empty());
        assert!(
            c.strategy_stats().iter().any(|s| {
                s["name"] == "all_in"
                    && s["gateExemptions"] == serde_json::json!(["timing", "momentum"])
            }),
            "the declaration is still reported even though the order was refused"
        );

        // Kill switch: identical exemption, hard stop wins.
        c.kill("test".into());
        assert_eq!(
            c.engine_evaluate(now + 2_000),
            0,
            "kill switch is not exemptible"
        );
        assert!(c.positions().open_positions().is_empty());

        // Global capacity ceiling: also outside the entry gates.
        let mut c2 = core_with_declared_dips(
            HashMap::new(),
            &[("all_in", &["BTC"])],
            0,
            WINDOW_SHUT,
            |_| crate::strategies::GateExemptions::all(),
        );
        feed_three_asset_dip(&mut c2, now);
        assert_eq!(
            c2.engine_evaluate(now + 1_000),
            0,
            "global capacity is not exemptible"
        );
        assert_eq!(c2.engine_stats()["placeRejected"], 1);

        // And the exemption cannot conjure a market (structural precondition).
        let mut c3 = core_with_declared_dips(HashMap::new(), &[("all_in", &["BTC"])], 5, 0, |_| {
            crate::strategies::GateExemptions::all()
        });
        assert_eq!(
            c3.engine_evaluate(now),
            0,
            "no round markets → nothing to exempt"
        );
    }

    #[test]
    fn a_gate_exemption_does_not_lift_the_daily_loss_cap() {
        // Realise a loss through the ordinary exit path, then confirm the
        // exempting strategy is refused exactly like any other.
        let mut c = core_with_declared_dips(HashMap::new(), &[("all_in", &["BTC"])], 5, 0, |_| {
            crate::strategies::GateExemptions::all()
        });
        c.config_mut().auto_exits_enabled = true;
        c.config_mut().positions.max_daily_loss_usd = dec!(0.0001);
        let now = 1_000_000i64;
        feed_three_asset_dip(&mut c, now);
        assert_eq!(
            c.engine_evaluate(now + 1_000),
            1,
            "the exemption still lets the entry in"
        );
        fill(&mut c, "BTC", now + 2_000);
        assert_eq!(c.positions().open_positions().len(), 1);

        // Crash the book and tick at the force-exit horizon → a realized loss.
        let token = "btc_up";
        c.book_snapshot(
            token,
            vec![(dec!(0.05), dec!(100))],
            vec![(dec!(0.06), dec!(100))],
            now + 3_000,
        );
        c.tick(1_800_000 - 100_000).unwrap();
        assert!(
            c.positions().daily_pnl() < Decimal::ZERO,
            "expected a realized loss"
        );

        // A fresh dip on the NEXT round: the exempted strategy is refused by the
        // daily-loss cap, proving the exemption stops at the entry gates.
        let now2 = 1_800_000i64;
        feed_three_asset_dip(&mut c, now2);
        let before = c.list_orders().len();
        assert_eq!(
            c.engine_evaluate(now2 + 1_000),
            0,
            "daily loss cap is not exemptible"
        );
        assert_eq!(c.list_orders().len(), before, "no new order was tracked");
    }

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

    /// Round + confirmed UP trend + calm spot + one dip book: the builtin emits
    /// exactly one spread_arb entry (0.43 x 10 = 4.30 USD) on the next cycle.
    fn feed_entry_setup(c: &mut Core, now: i64) {
        c.engine_on_data(
            DataEvent::RoundMarkets {
                markets: vec![market(now)],
                now_ms: now,
            },
            now,
        );
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
        c.engine_on_data(
            DataEvent::Spot {
                asset: "BTC".into(),
                price: dec!(60000),
                now_ms: t,
            },
            t,
        );
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
        stats
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("no stats for {name}: {stats:?}"))
    }

    #[test]
    fn default_config_has_no_limits_and_places_the_entry() {
        let mut c = core_with_engine(HashMap::new());
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(
            c.engine_evaluate(now + 12_000),
            1,
            "no caps configured must not change behaviour"
        );
        assert_eq!(c.engine_stats()["strategyLimitRejected"], 0);

        // Fill the resting maker entry so exposure is live, then read the ledger.
        c.book_snapshot(
            "up",
            vec![(dec!(0.42), dec!(100))],
            vec![(dec!(0.42), dec!(100))],
            now + 13_000,
        );
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
            StrategyLimit {
                max_open_positions: Some(0),
                ..Default::default()
            },
        );
        let mut c = core_with_engine(limits);
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(
            c.engine_evaluate(now + 12_000),
            0,
            "position cap 0 must block the entry"
        );
        assert!(
            c.ome().live_orders().is_empty(),
            "no order may reach the OME"
        );
        assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "spread_arb");
        assert_eq!(s["limitRejected"], 1);
        assert_eq!(s["ordersPlaced"], 0);
        assert_eq!(s["openPositions"], 0);
    }

    #[test]
    fn rejection_causes_report_position_cap_bucket() {
        // E9-c: the limit rejection lands in a machine-parseable bucket next
        // to the live counters, so an operator can see WHY without logs.
        let mut limits = HashMap::new();
        limits.insert(
            "spread_arb".to_string(),
            StrategyLimit {
                max_open_positions: Some(0),
                ..Default::default()
            },
        );
        let mut c = core_with_engine(limits);
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 12_000), 0);
        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "spread_arb");
        assert_eq!(
            s["rejectionCauses"]["limit.positionCap"], 1,
            "position-cap rejection must be bucketed: {s}"
        );
        assert_eq!(
            s["ordersRejected"], 0,
            "cap rejections are not place rejections"
        );
        // A strategy that never placed has NO causes object at all.
        let clean_core = core_with_engine(HashMap::new());
        let clean_stats = clean_core.strategy_stats();
        let clean = strategy_entry(&clean_stats, "spread_arb");
        assert!(clean["rejectionCauses"].is_null());
    }

    #[test]
    fn rejection_causes_report_per_order_cap_risk_bucket() {
        // E9-c: a Risk::check per-order-cap rejection inside place() is
        // bucketed as risk.perOrderCap (previously the reason was dropped).
        let mut c = core_with_engine(HashMap::new());
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        c.risk_config_mut().max_order_notional = dec!(1);
        assert_eq!(
            c.engine_evaluate(now + 12_000),
            0,
            "1 USD per-order cap must reject the 4.30 entry"
        );
        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "spread_arb");
        assert_eq!(s["ordersRejected"], 1);
        assert_eq!(
            s["rejectionCauses"]["risk.perOrderCap"], 1,
            "must count and bucket the risk reason: {s}"
        );
    }

    #[test]
    fn strategy_notional_cap_blocks_below_and_passes_above_the_entry() {
        // Entry notional is 0.43 * 10 = 4.30: a 1.00 cap rejects, a 10.00 cap passes.
        let mut tight = HashMap::new();
        tight.insert(
            "spread_arb".to_string(),
            StrategyLimit {
                max_open_notional_usd: Some(dec!(1)),
                ..Default::default()
            },
        );
        let mut c = core_with_engine(tight);
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(
            c.engine_evaluate(now + 12_000),
            0,
            "notional cap 1.00 must block a 4.30 entry"
        );
        assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);

        let mut loose = HashMap::new();
        loose.insert(
            "spread_arb".to_string(),
            StrategyLimit {
                max_open_notional_usd: Some(dec!(10)),
                ..Default::default()
            },
        );
        let mut c2 = core_with_engine(loose);
        feed_entry_setup(&mut c2, now);
        assert_eq!(
            c2.engine_evaluate(now + 12_000),
            1,
            "cap above the entry notional must not block"
        );
        assert_eq!(c2.engine_stats()["strategyLimitRejected"], 0);
    }

    #[test]
    fn closed_trade_lands_in_the_strategy_ledger() {
        let mut c = core_with_engine(HashMap::new());
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 12_000), 1);
        assert_eq!(
            c.positions().open_positions().len(),
            0,
            "maker entry rests until the book crosses"
        );

        // Fill the resting bid (ask 0.42 crosses the 0.43 bid), then push a
        // +100%-ish book so the exit engine closes the position at a profit.
        c.book_snapshot(
            "up",
            vec![(dec!(0.42), dec!(100))],
            vec![(dec!(0.42), dec!(100))],
            now + 13_000,
        );
        assert_eq!(
            c.positions().open_positions().len(),
            1,
            "crossing ask must fill the maker entry"
        );
        c.book_snapshot(
            "up",
            vec![(dec!(0.95), dec!(100))],
            vec![(dec!(0.97), dec!(100))],
            now + 14_000,
        );
        c.tick(now + 14_200).unwrap();
        assert_eq!(
            c.positions().open_positions().len(),
            0,
            "profit target must close the position"
        );

        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "spread_arb");
        assert_eq!(s["closedTrades"], 1);
        assert_eq!(s["wins"], 1);
        assert_eq!(s["losses"], 0);
        assert!(
            dec_of(&s["netPnlUsd"]) > Decimal::ZERO,
            "expected positive realized PnL"
        );
        // E17: the entry rests on the book and is hit (maker), and the profit
        // target rests at the bid and is hit (maker), so this fixture pays NO
        // fee. Before the fix the entry was charged the taker rate while the
        // record claimed maker — that phantom fee is what made "fees > 0" pass,
        // and it is exactly the drift E17 removed. Pin the equality that
        // actually matters: the fee total IS the record's, and the PnL IS cash.
        let rec = &c.positions().closed_positions()[0];
        let rec_fees = (rec.entry_fee_pct / Decimal::ONE_HUNDRED) * rec.entry_price * rec.shares
            + (rec.exit_fee_pct / Decimal::ONE_HUNDRED) * rec.exit_price * rec.shares;
        assert_eq!(
            rec_fees,
            Decimal::ZERO,
            "both legs rested on the book, so nothing was charged"
        );
        assert_eq!(
            dec_of(&s["feesUsd"]),
            rec_fees,
            "the ledger's fee total must equal the trade record's, not exceed it"
        );
        assert_eq!(
            dec_of(&s["netPnlUsd"]),
            rec.net_pnl_usd,
            "strategy PnL must be the cash the ledger moved"
        );
        assert_eq!(
            dec_of(&s["openNotionalUsd"]),
            Decimal::ZERO,
            "closed position leaves no exposure"
        );
    }

    /// The cash ledger must equal `seed + Σ netPnlUsd` once nothing is reserved
    /// and no position is open — the identity #75 set out to establish, and the
    /// E17 acceptance criterion.
    ///
    /// How this fixture used to fail (drift 0.0817722), for the record. A
    /// `MakerThenTaker` entry rests at the bid, gets hit, and its profit target
    /// exits as a maker; three places disagreed about what kind of fill happened:
    ///
    /// 1. ENTRY — the position recorded `was_maker_entry = true` and a 0 entry
    ///    fee, while `apply_delta_effects` read the escalated order's
    ///    `mode = MakerThenTaker` and charged the TAKER fee. Cash sat BELOW the
    ///    record by that fee.
    /// 2. EXIT — the closing delta covered 9.99 of the 10 shares (the old flat
    ///    0.01 share buffer), so cash received 0.01 × exit_price less proceeds
    ///    than the record's gross assumed.
    /// 3. EXIT — the record re-derived `was_maker_exit = false` and charged a
    ///    taker exit fee, while the delta's `mode = Maker` charged none. Cash sat
    ///    ABOVE the record by that fee.
    ///
    /// The E17 fix removes the disagreement rather than the symptom: the fee now
    /// follows `FillDelta.role` (resolved from the fill), the position accrues the
    /// fee the ledger actually charged, the flat share buffer is gone (exits floor
    /// to the 0.01 share grid), and a sub-grid remainder is written off inside
    /// `net`. See `MIGRATION_LOG.md`.
    #[test]
    fn dry_balance_is_seed_plus_realized_net() {
        const SEED: Decimal = dec!(1000);
        let mut c = core_with_engine(HashMap::new());
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 12_000), 1);
        // Fill the resting bid, then mark up so the profit target closes it.
        c.book_snapshot(
            "up",
            vec![(dec!(0.42), dec!(100))],
            vec![(dec!(0.42), dec!(100))],
            now + 13_000,
        );
        c.book_snapshot(
            "up",
            vec![(dec!(0.95), dec!(100))],
            vec![(dec!(0.97), dec!(100))],
            now + 14_000,
        );
        c.tick(now + 14_200).unwrap();
        assert_eq!(c.positions().open_positions().len(), 0);
        assert_eq!(c.ledger().reserved(), Decimal::ZERO);

        let net: Decimal = c
            .positions()
            .closed_positions()
            .iter()
            .map(|p| p.net_pnl_usd)
            .sum();
        assert!(
            net > Decimal::ZERO,
            "the fixture is meant to close at a profit, got {net}"
        );
        let parts: Vec<String> = c
            .positions()
            .closed_positions()
            .iter()
            .map(|p| {
                let ef = (p.entry_fee_pct / Decimal::ONE_HUNDRED) * p.entry_price * p.shares;
                let xf = (p.exit_fee_pct / Decimal::ONE_HUNDRED) * p.exit_price * p.shares;
                format!(
                    "id={} entry={} exit={} shares={} mkEntry={} mkExit={} entryFee={} exitFee={} gross={} net={}",
                    p.id, p.entry_price, p.exit_price, p.shares, p.was_maker_entry,
                    p.was_maker_exit, ef, xf, p.pnl_usd, p.net_pnl_usd
                )
            })
            .collect();
        assert_eq!(
            c.ledger().balance(),
            SEED + net,
            "cash must equal principal + realized net once nothing is open; closed={parts:?}"
        );
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
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
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
        let (id, _) = c
            .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();
        c.book_snapshot(
            "tok",
            vec![(dec!(0.39), dec!(100))],
            vec![(dec!(0.40), dec!(100))],
            1_100,
        );
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Filled);
        let pos = &c.positions().open_positions()[0];
        assert_eq!(
            pos.entry_price,
            dec!(0.40),
            "a maker fill is priced by your own quote"
        );
    }

    #[test]
    fn taker_slippage_worsens_the_fill_price() {
        let mut c = core_with(FillModel {
            taker_slippage_ticks: 2,
            ..FillModel::default()
        });
        let (id, st) = c
            .place(buy(FillPolicy::Taker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();
        assert_eq!(st, OrderStatus::Filled);
        // 2 ticks = 0.02: a taker buy pays up rather than filling at the limit.
        assert_eq!(c.ome().get(&id).unwrap().avg_fill_price, Some(dec!(0.42)));
        assert_eq!(c.positions().open_positions()[0].entry_price, dec!(0.42));
    }

    #[test]
    fn maker_latency_delays_the_crossing_fill() {
        let mut c = core_with(FillModel {
            maker_latency_ms: 5_000,
            ..FillModel::default()
        });
        let (id, _) = c
            .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();
        // Crossed 100 ms after submission: the venue could not have the order yet.
        c.book_snapshot(
            "tok",
            vec![(dec!(0.39), dec!(100))],
            vec![(dec!(0.40), dec!(100))],
            1_100,
        );
        assert_eq!(
            c.positions().open_positions().len(),
            0,
            "no fill inside the latency window"
        );
        assert!(
            c.ome().get(&id).unwrap().status.is_live(),
            "the order stays live"
        );
        // Crossed again after the window: fills.
        c.book_snapshot(
            "tok",
            vec![(dec!(0.39), dec!(100))],
            vec![(dec!(0.40), dec!(100))],
            6_100,
        );
        assert_eq!(
            c.positions().open_positions().len(),
            1,
            "fills once the latency has elapsed"
        );
        assert_eq!(c.positions().open_positions()[0].entry_price, dec!(0.40));
    }

    #[test]
    fn zero_fill_probability_never_fills_on_a_crossing() {
        let mut c = core_with(FillModel {
            maker_fill_prob_bps: 0,
            ..FillModel::default()
        });
        let (id, _) = c
            .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();
        for t in [1_100i64, 2_000, 9_000] {
            c.book_snapshot(
                "tok",
                vec![(dec!(0.39), dec!(100))],
                vec![(dec!(0.40), dec!(100))],
                t,
            );
        }
        assert_eq!(
            c.positions().open_positions().len(),
            0,
            "a losing queue draw keeps the order unfilled"
        );
        assert!(c.ome().get(&id).unwrap().status.is_live());
        assert_eq!(c.ome().get(&id).unwrap().filled_size, Decimal::ZERO);
    }
}

/// ── E17: accounting precision ────────────────────────────────────────────────
///
/// The invariant under test is the version plan's core E17 acceptance criterion:
/// with nothing reserved and no position open,
///
/// ```text
///     ledger.balance() == seed + Σ closed.net_pnl_usd
/// ```
///
/// Every scenario is driven through the REAL fill path (dry matcher, live
/// user-WS ingest, or a reconciliation gap — all three meet at
/// `apply_delta_effects`), so what is asserted is the production arithmetic and
/// not a reimplementation of it in the test.
///
/// The matrix covers the maker/taker × partial-fill combinations the plan names
/// (1/10, 5/10, 9.99/10, 10/10) on BOTH legs, plus the escalation path that used
/// to produce the 0.0817722 drift. Enabled by the `account-precision` feature,
/// which is on by default so a plain `cargo test` covers it too.
#[cfg(all(test, feature = "account-precision"))]
mod account_precision_tests {
    use super::*;
    use crate::model::OrderRole;
    use rust_decimal_macros::dec;

    const SEED: Decimal = dec!(1000);

    fn core() -> Core {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: SEED,
            // Keep the fixture's arithmetic to the position, not the scheduler.
            auto_exits_enabled: false,
            ..Default::default()
        });
        c.set_balance(SEED);
        c
    }

    fn req(side: Side, mode: FillPolicy, price: Decimal, size: Decimal, key: &str) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side,
            mode,
            price,
            size,
            internal_key: key.into(),
            strategy: "acc".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    fn fill(order_id: &str, trade: &str, side: Side, price: Decimal, size: Decimal) -> Fill {
        Fill {
            order_id: order_id.into(),
            trade_id: Some(trade.into()),
            token_id: "tok".into(),
            side,
            price,
            size,
            status: FillStatus::Confirmed,
            ts_ms: 0,
            tx_hash: None,
            // The harness reports no role, so the OME falls back to the order's
            // fill policy — correct here, because these fills are the ones the
            // policy would produce. A test that needs the VENUE to overrule the
            // policy uses `fill_as` below.
            maker: None,
        }
    }

    /// A fill carrying the venue's OWN maker/taker report, which outranks the
    /// order's fill policy (E17-b).
    fn fill_as(
        order_id: &str,
        trade: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        maker: bool,
    ) -> Fill {
        Fill {
            maker: Some(maker),
            ..fill(order_id, trade, side, price, size)
        }
    }

    /// Σ net PnL over the closed book.
    fn realized(c: &Core) -> Decimal {
        c.positions()
            .closed_positions()
            .iter()
            .map(|p| p.net_pnl_usd)
            .sum()
    }

    /// The E17 identity, asserted with the offending numbers in the message so a
    /// regression is diagnosable from CI output alone.
    fn assert_reconciled(c: &Core, scenario: &str) {
        assert_eq!(
            c.positions().open_positions().len(),
            0,
            "[{scenario}] the scenario must end flat"
        );
        let net = realized(c);
        let detail: Vec<String> = c
            .positions()
            .closed_positions()
            .iter()
            .map(|p| {
                format!(
                    "{} entry={} exit={} shares={} basis={} entryFee={} exitFee={} entryRole={:?} exitRole={:?} dust={} gross={} net={}",
                    p.id, p.entry_price, p.exit_price, p.shares, p.cost_usd,
                    p.entry_fee_pct, p.exit_fee_pct, p.entry_role, p.exit_role,
                    p.dust_shares, p.pnl_usd, p.net_pnl_usd
                )
            })
            .collect();
        assert_eq!(
            c.ledger().balance(),
            SEED + net,
            "[{scenario}] balance must equal seed + realized net; closed={detail:?}"
        );
    }

    /// A BUY that only partly filled still holds the UNFILLED remainder's
    /// reservation until the order is cancelled — a real (pre-existing) property
    /// of the ledger, since the remainder is still committed on the book. The
    /// E17 identity is about cash, so the harness releases the remainder the way
    /// the venue would (cancel what will never fill) before asserting.
    fn cancel_open_buys(c: &mut Core, now_ms: i64) {
        let residual: Vec<String> = c
            .ome()
            .live_orders()
            .into_iter()
            .filter(|o| o.side == Side::Buy && o.filled_size < o.size)
            .map(|o| o.order_id.clone())
            .collect();
        for id in residual {
            c.cancel(&id, now_ms).unwrap();
        }
    }

    /// A round trip at an explicit fill shape, driven the way the VENUE drives it:
    /// the order is registered pending (no dry auto-fill) and the fills arrive as
    /// cumulative reports. The entry delivers `entry_partial` of the requested
    /// size; the exit is then sized to what was actually held and delivers
    /// `exit_partial` of that, with any remainder closed by a second exit fill —
    /// so the position ends flat however the venue rations the fills.
    fn dry_round_trip(
        entry_mode: FillPolicy,
        entry_price: Decimal,
        entry_partial: Decimal,
        exit_mode: FillPolicy,
        exit_price: Decimal,
        exit_partial: Decimal,
        size: Decimal,
    ) -> Core {
        let mut c = core();
        let (id, _) = c
            .place_pending(req(Side::Buy, entry_mode, entry_price, size, "entry"), 1)
            .unwrap();
        c.confirm_live(&id, 1).unwrap();
        c.ingest_fill(fill(&id, "e1", Side::Buy, entry_price, entry_partial), 2)
            .unwrap();

        let held = c.positions().open_positions()[0].shares;
        assert_eq!(
            held, entry_partial,
            "the venue's partial decides the size, not the request"
        );
        // The unfilled part of the entry will never fill; let it go so only cash
        // and the position are compared.
        cancel_open_buys(&mut c, 3);

        let (sid, _) = c
            .place_pending(req(Side::Sell, exit_mode, exit_price, held, "exit"), 4)
            .unwrap();
        c.confirm_live(&sid, 4).unwrap();
        c.ingest_fill(fill(&sid, "x1", Side::Sell, exit_price, exit_partial), 5)
            .unwrap();
        // Any shares the first exit did not cover are closed by a second fill.
        let left: Decimal = c
            .positions()
            .open_positions()
            .iter()
            .map(|p| p.shares)
            .sum();
        if left > Decimal::ZERO {
            let (rid, _) = c
                .place_pending(req(Side::Sell, exit_mode, exit_price, left, "exit-2"), 6)
                .unwrap();
            c.confirm_live(&rid, 6).unwrap();
            c.ingest_fill(fill(&rid, "x2", Side::Sell, exit_price, left), 7)
                .unwrap();
        }
        cancel_open_buys(&mut c, 8);
        c
    }

    /// The plan's named partials on both legs, across all four fill policies.
    #[test]
    fn every_maker_taker_partial_shape_reconciles() {
        let shapes: [(&str, Decimal); 4] = [
            ("1/10", dec!(1)),
            ("5/10", dec!(5)),
            ("9.99/10", dec!(9.99)),
            ("10/10", dec!(10)),
        ];
        let mut checked = 0usize;
        for (entry_label, entry_partial) in shapes {
            for (exit_label, exit_partial) in shapes {
                // An exit can never exceed what the entry actually delivered.
                if exit_partial > entry_partial {
                    continue;
                }
                for em in [FillPolicy::Maker, FillPolicy::Taker] {
                    for xm in [FillPolicy::Maker, FillPolicy::Taker] {
                        let c = dry_round_trip(
                            em,
                            dec!(0.43),
                            entry_partial,
                            xm,
                            dec!(0.95),
                            exit_partial,
                            dec!(10),
                        );
                        assert_reconciled(
                            &c,
                            &format!("entry {entry_label} {em:?} / exit {exit_label} {xm:?}"),
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 40, "the whole named matrix must run");
    }

    /// The VENUE's report outranks the order's fill policy (E17-b). A
    /// `MakerThenTaker` order can end up filling as a pure maker (it rested and
    /// was hit before the timer) or as a pure taker (it escalated); the policy
    /// says nothing about which happened, so the report must win and the fee must
    /// follow it — in both directions, so this cannot pass by always picking one.
    #[test]
    fn the_venue_role_report_outranks_the_orders_policy() {
        // A MakerThenTaker order that the venue says CROSSED → taker fee.
        let mut crossed = core();
        let (id, _) = crossed
            .place_pending(
                req(
                    Side::Buy,
                    FillPolicy::MakerThenTaker,
                    dec!(0.70),
                    dec!(10),
                    "c",
                ),
                1,
            )
            .unwrap();
        crossed.confirm_live(&id, 1).unwrap();
        crossed
            .ingest_fill(
                fill_as(&id, "c1", Side::Buy, dec!(0.70), dec!(10), false),
                2,
            )
            .unwrap();
        let pos = crossed.positions().open_positions()[0].clone();
        assert_eq!(pos.entry_role, OrderRole::Taker, "the report said taker");
        assert!(
            pos.flows.entry_fee_usd > Decimal::ZERO,
            "a crossing fill pays the taker fee even though the policy rested first"
        );

        // A plain Maker order the venue says CROSSED → also a taker fee. Under
        // policy-only resolution this silently paid nothing.
        let mut surprised = core();
        let (id2, _) = surprised
            .place_pending(
                req(Side::Buy, FillPolicy::Maker, dec!(0.70), dec!(10), "s"),
                1,
            )
            .unwrap();
        surprised.confirm_live(&id2, 1).unwrap();
        surprised
            .ingest_fill(
                fill_as(&id2, "s1", Side::Buy, dec!(0.70), dec!(10), false),
                2,
            )
            .unwrap();
        let pos2 = surprised.positions().open_positions()[0].clone();
        assert_eq!(pos2.entry_role, OrderRole::Taker);
        assert_eq!(
            pos2.flows.entry_fee_usd,
            (crate::exit_policy::taker_fee_pct(dec!(0.70)) / Decimal::ONE_HUNDRED) * dec!(7)
        );

        // The reverse: a TAKER order the venue says RESTED → no fee. (Rebates are
        // not modelled, but a maker fill is genuinely free.)
        let mut rested = core();
        let (id3, _) = rested
            .place_pending(
                req(Side::Buy, FillPolicy::Taker, dec!(0.70), dec!(10), "r"),
                1,
            )
            .unwrap();
        rested.confirm_live(&id3, 1).unwrap();
        rested
            .ingest_fill(
                fill_as(&id3, "r1", Side::Buy, dec!(0.70), dec!(10), true),
                2,
            )
            .unwrap();
        let pos3 = rested.positions().open_positions()[0].clone();
        assert_eq!(pos3.entry_role, OrderRole::Maker, "the report said maker");
        assert_eq!(
            pos3.flows.entry_fee_usd,
            Decimal::ZERO,
            "a resting fill is charged nothing"
        );
    }

    /// The exact fixture from the plan's background section: a `MakerThenTaker`
    /// entry that rests at the bid, is hit, and exits at a profit. This is the
    /// case that measured a 0.0817722 gap before E17.
    #[test]
    fn maker_then_taker_entry_and_profit_exit_reconcile() {
        let mut c = core();
        let (id, _) = c
            .place(
                req(
                    Side::Buy,
                    FillPolicy::MakerThenTaker,
                    dec!(0.43),
                    dec!(10),
                    "entry",
                ),
                0,
                1,
            )
            .unwrap();
        // The resting bid is hit in full → a MAKER entry fill (no fee).
        c.ingest_fill(fill(&id, "mtt-1", Side::Buy, dec!(0.43), dec!(10)), 2)
            .unwrap();
        let pos = c.positions().open_positions()[0].clone();
        assert_eq!(pos.entry_role, OrderRole::Maker, "rested, so maker");
        assert_eq!(
            pos.flows.entry_fee_usd,
            Decimal::ZERO,
            "a maker entry pays no fee"
        );

        // Profit target exits as a maker.
        let (sid, _) = c
            .place(
                req(Side::Sell, FillPolicy::Maker, dec!(0.95), dec!(10), "exit"),
                0,
                3,
            )
            .unwrap();
        c.ingest_fill(fill(&sid, "mtt-2", Side::Sell, dec!(0.95), dec!(10)), 4)
            .unwrap();

        assert_reconciled(&c, "MakerThenTaker hit → maker profit exit");
        let closed = &c.positions().closed_positions()[0];
        // 10 shares × (0.95 − 0.43) = 5.20, with no fees on either leg.
        assert_eq!(closed.pnl_usd, dec!(5.20));
        assert_eq!(closed.net_pnl_usd, dec!(5.20));
        assert_eq!(closed.entry_role, OrderRole::Maker);
        assert_eq!(closed.exit_role, OrderRole::Maker);
        assert_eq!(closed.dust_shares, Decimal::ZERO);
    }

    /// Mixed roles across legs of ONE position: a maker leg that partially fills,
    /// then the escalated taker leg that completes the entry. This drives the REAL
    /// escalation (maker timeout → cancel → cross the remainder) rather than
    /// hand-placing the second order, so it also pins that a partially filled
    /// maker leg still escalates instead of being rejected as a duplicate entry.
    /// The fee must follow each leg's own role, not the original request — the
    /// core of the E17 defect.
    #[test]
    fn a_mixed_role_position_charges_each_leg_by_what_it_did() {
        let mut c = core();
        let (id, _) = c
            .place(
                req(
                    Side::Buy,
                    FillPolicy::MakerThenTaker,
                    dec!(0.43),
                    dec!(10),
                    "entry",
                ),
                1_000,
                1,
            )
            .unwrap();
        assert!(c.ome().get(&id).unwrap().status.is_live(), "it must rest");

        // Maker leg: 4 of 10 shares are hit at the resting bid.
        c.ingest_fill(fill(&id, "leg-1", Side::Buy, dec!(0.43), dec!(4)), 2)
            .unwrap();
        let half = c.positions().open_positions()[0].clone();
        assert_eq!(half.shares, dec!(4));
        assert_eq!(half.entry_role, OrderRole::Maker);
        assert_eq!(
            half.flows.entry_fee_usd,
            Decimal::ZERO,
            "the maker leg pays no fee"
        );

        // The maker window elapses: the kernel cancels the rest of the resting
        // order and crosses the remaining 6 shares as a TAKER.
        c.tick(1_002).unwrap();
        let pos = c.positions().open_positions()[0].clone();
        assert_eq!(pos.shares, dec!(10), "both legs accrue onto one position");
        assert_eq!(
            pos.entry_role,
            OrderRole::MakerThenTaker,
            "the position records that both kinds of fill happened"
        );
        // Basis is the real money spent: 4×0.43 rested, 6×0.43 crossed.
        assert_eq!(pos.cost_usd, dec!(10) * dec!(0.43));
        // The fee covers the TAKER leg only — not all 10 shares at the taker rate.
        let expected_fee = (crate::exit_policy::taker_fee_pct(dec!(0.43)) / Decimal::ONE_HUNDRED)
            * dec!(0.43)
            * dec!(6);
        assert_eq!(pos.flows.entry_fee_usd, expected_fee);
        assert!(expected_fee > Decimal::ZERO);

        // Exit in full as a taker at a profit.
        let (sid, _) = c
            .place(
                req(Side::Sell, FillPolicy::Taker, dec!(0.95), dec!(10), "exit"),
                0,
                5,
            )
            .unwrap();
        assert_eq!(c.ome().get(&sid).unwrap().status, OrderStatus::Filled);
        assert_reconciled(&c, "mixed maker/taker entry, taker exit");

        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.entry_role, OrderRole::MakerThenTaker);
        assert_eq!(closed.exit_role, OrderRole::Taker);
        assert_eq!(closed.shares, dec!(10));
        // Gross 10×(0.95−0.43) = 5.20, minus the taker legs' fees.
        let exit_fee =
            (crate::exit_policy::taker_fee_pct(dec!(0.95)) / Decimal::ONE_HUNDRED) * dec!(9.5);
        assert_eq!(closed.net_pnl_usd, dec!(5.20) - expected_fee - exit_fee);
    }

    /// Dry and LIVE must produce a bit-identical ledger. In DRY the core
    /// synthesises the fills; in LIVE the venue adapter reports them. Both meet
    /// at `apply_delta_effects`, so identical inputs must yield identical money.
    /// This is the `account:parity` gate expressed as a unit test.
    #[test]
    fn dry_and_live_ledgers_agree_bit_for_bit() {
        for (em, xm) in [
            (FillPolicy::Maker, FillPolicy::Taker),
            (FillPolicy::Taker, FillPolicy::Maker),
            (FillPolicy::Maker, FillPolicy::Maker),
            (FillPolicy::MakerThenTaker, FillPolicy::Taker),
        ] {
            // DRY: submit at a crossing book so the core fills it itself.
            let mut dry = core();
            dry.book_snapshot(
                "tok",
                vec![(dec!(0.43), dec!(100))],
                vec![(dec!(0.43), dec!(100))],
                1,
            );
            let (did, _) = dry
                .place(req(Side::Buy, em, dec!(0.43), dec!(10), "entry"), 0, 1)
                .unwrap();
            if dry.ome().get(&did).unwrap().filled_size < dec!(10) {
                // It rested without crossing; drive the same fill the venue would.
                dry.ingest_fill(fill(&did, "d1", Side::Buy, dec!(0.43), dec!(10)), 2)
                    .unwrap();
            }
            let (dxid, _) = dry
                .place(req(Side::Sell, xm, dec!(0.95), dec!(10), "exit"), 0, 3)
                .unwrap();
            if dry.ome().get(&dxid).unwrap().filled_size < dec!(10) {
                dry.ingest_fill(fill(&dxid, "d2", Side::Sell, dec!(0.95), dec!(10)), 4)
                    .unwrap();
            }

            // LIVE: register pending, venue acks, then the same fills stream in.
            let mut live = core();
            let (lid, _) = live
                .place_pending(req(Side::Buy, em, dec!(0.43), dec!(10), "entry"), 1)
                .unwrap();
            live.confirm_live(&lid, 1).unwrap();
            live.ingest_fill(fill(&lid, "l1", Side::Buy, dec!(0.43), dec!(10)), 2)
                .unwrap();
            let (lxid, _) = live
                .place_pending(req(Side::Sell, xm, dec!(0.95), dec!(10), "exit"), 3)
                .unwrap();
            live.confirm_live(&lxid, 3).unwrap();
            live.ingest_fill(fill(&lxid, "l2", Side::Sell, dec!(0.95), dec!(10)), 4)
                .unwrap();

            assert_eq!(
                dry.ledger().balance(),
                live.ledger().balance(),
                "[{em:?}→{xm:?}] dry and live cash must be bit-identical"
            );
            assert_eq!(
                realized(&dry),
                realized(&live),
                "[{em:?}→{xm:?}] dry and live realized PnL must be bit-identical"
            );
            assert_reconciled(&dry, &format!("dry parity {em:?}→{xm:?}"));
            assert_reconciled(&live, &format!("live parity {em:?}→{xm:?}"));
        }
    }

    /// A rollback (`FAILED`) must not leave phantom cash, a phantom fee, or a
    /// phantom position behind.
    #[test]
    fn a_rolled_back_fill_leaves_the_ledger_clean() {
        let mut c = core();
        // Rest the order so the venue, not the matcher, reports the fills — the
        // rollback must target the SAME trade the provisional fill created.
        let (id, _) = c
            .place_pending(
                req(Side::Buy, FillPolicy::Maker, dec!(0.43), dec!(10), "entry"),
                1,
            )
            .unwrap();
        c.confirm_live(&id, 1).unwrap();
        c.ingest_fill(fill(&id, "r1", Side::Buy, dec!(0.43), dec!(10)), 2)
            .unwrap();
        let provisioned = c.ledger().balance();
        assert!(provisioned < SEED, "the provisional fill moved cash");

        // The venue reports that same trade as FAILED: it is fully rolled back.
        c.ingest_fill(
            Fill {
                status: FillStatus::Failed,
                ..fill(&id, "r1", Side::Buy, dec!(0.43), dec!(10))
            },
            3,
        )
        .unwrap();
        c.tick(4).unwrap();

        let net = realized(&c);
        assert_eq!(
            c.ledger().balance(),
            SEED + net,
            "a rolled-back trade must not move cash"
        );
        let held: Decimal = c
            .positions()
            .open_positions()
            .iter()
            .map(|p| p.shares)
            .sum();
        assert_eq!(held, Decimal::ZERO, "nothing may be held after a rollback");
        assert_eq!(net, Decimal::ZERO, "a rollback realizes no PnL");
    }

    /// Reconciliation-gap fills (the third producer of deltas) reconcile too:
    /// the venue reports a partial the core never saw, and the position is sized
    /// from what was actually confirmed, not from what was requested.
    #[test]
    fn reconciliation_gap_fills_reconcile() {
        let mut c = core();
        // The order goes out and the venue ack'd it; the fill arrives only later
        // and only in part — the classic reconciliation gap.
        let (id, _) = c
            .place_pending(
                req(Side::Buy, FillPolicy::Taker, dec!(0.62), dec!(10), "entry"),
                1,
            )
            .unwrap();
        c.confirm_live(&id, 1).unwrap();
        c.ingest_fill(fill(&id, "gap-1", Side::Buy, dec!(0.62), dec!(4)), 2)
            .unwrap();
        let held = c.positions().open_positions()[0].shares;
        assert_eq!(held, dec!(4), "only what the venue confirmed");
        cancel_open_buys(&mut c, 3);

        // Close the whole holding as a taker at a loss.
        let (sid, _) = c
            .place(
                req(Side::Sell, FillPolicy::Taker, dec!(0.40), held, "exit"),
                0,
                4,
            )
            .unwrap();
        assert_eq!(c.ome().get(&sid).unwrap().status, OrderStatus::Filled);
        assert_reconciled(&c, "reconciliation gap partial entry");
        assert!(realized(&c) < Decimal::ZERO, "bought 0.62, sold 0.40");
    }

    /// The E17 identity asserted MID-FLIGHT on a half-exited position — the case
    /// the flat-book form cannot see.
    ///
    /// The view exposes two different entry figures and only one of them is cash:
    /// `flows.entry_cost_usd` is the TOTAL notional paid in, while `cost_usd` is
    /// the basis of the shares STILL HELD. A partial exit moves basis out of
    /// `cost_usd` and returns it inside `proceeds_usd`, so an identity that sums
    /// `cost_usd` counts that release twice and reports a phantom drift exactly
    /// equal to the released basis. Both facts are pinned here, because the
    /// distinction is invisible until a position is partly exited — which is why
    /// an external monitor summing `cost_usd` (as the first draft of
    /// `account-drift-check.mjs` did) reports drift on a correct ledger.
    #[test]
    fn the_identity_holds_mid_flight_and_entry_cost_is_not_the_held_basis() {
        let mut c = core();
        let (id, _) = c
            .place(
                req(Side::Buy, FillPolicy::Taker, dec!(0.43), dec!(10), "in"),
                0,
                1,
            )
            .unwrap();
        c.ingest_fill(
            fill_as(&id, "mf-1", Side::Buy, dec!(0.43), dec!(10), false),
            2,
        )
        .unwrap();

        // Sell 4 of the 10 — the position stays open with both cash legs non-zero.
        let (sid, _) = c
            .place(
                req(Side::Sell, FillPolicy::Taker, dec!(0.60), dec!(4), "out"),
                0,
                3,
            )
            .unwrap();
        c.ingest_fill(
            fill_as(&sid, "mf-2", Side::Sell, dec!(0.60), dec!(4), false),
            4,
        )
        .unwrap();

        let p = c.positions().open_positions()[0].clone();
        assert_eq!(p.shares, dec!(6), "4 of 10 sold, so 6 remain");
        assert!(p.flows.proceeds_usd > Decimal::ZERO, "cash came back in");
        assert_eq!(
            p.flows.entry_cost_usd,
            dec!(4.30),
            "the TOTAL paid in never shrinks with a partial exit"
        );
        assert_eq!(
            p.cost_usd,
            dec!(2.58),
            "the held basis does shrink: 4.30 − (4.30/10 × 4)"
        );
        assert!(
            p.flows.entry_cost_usd > p.cost_usd,
            "this is exactly why the identity may not use the held basis"
        );

        let cash_in = p.flows.entry_cost_usd + p.flows.entry_fee_usd;
        let cash_back = p.flows.proceeds_usd - p.flows.exit_fee_usd;
        let realized: Decimal = c
            .positions()
            .closed_positions()
            .iter()
            .map(|p| p.net_pnl_usd)
            .sum();
        assert_eq!(
            c.ledger().balance(),
            SEED + realized - cash_in + cash_back,
            "the identity must hold mid-flight, not only once flat"
        );
        // ...and the held-basis form must NOT hold, or this test would be pinning
        // nothing. It overstates cash by the released basis, to the last digit.
        let wrong = SEED + realized - (p.cost_usd + p.flows.entry_fee_usd) + cash_back;
        assert_eq!(
            wrong - c.ledger().balance(),
            dec!(1.72),
            "summing the held basis double-counts the 4-share release"
        );
    }
}
