//! Core service — assembles the OME, ledger, risk gate and dry matcher and
//! implements every typed command. Synchronous except for the event sink, so it
//! is fully unit-testable; the async UDS layer and live venue sit on top.

use crate::ipc::schema::Event;
use crate::ipc::server::now_ms;
use crate::ledger::Ledger;
use crate::model::*;
use crate::ome::{AppliedFillRecord, FillDelta, FillOutcome, LateFill, Ome, SubmitParams};
use crate::position::{EquityBasis, OpenParams, PositionConfig, PositionManager};
use crate::reconcile::{AuditInput, AuditReport, CashIdentity};
use crate::risk::{LossBreakers, RiskConfig, RiskGate};
use crate::settlement::{SETTLEMENT_EXIT_REASON, SettlementBook, booking_for, settlement_key};
use crate::shadow_evolution::{
    EvolutionOutcome, EvolutionStatus, MutableParams, ShadowEvolution, ShadowEvolutionConfig,
};
use crate::sim::{Book, rests_on_book};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub mode: Mode,
    pub default_maker_timeout_ms: i64,
    /// Ticks a taker exit's limit is set below the executable bid so the FOK
    /// stays marketable in thin books (the venue still fills at the bid or
    /// better). 0 = price exactly at the executable bid.
    pub exit_taker_slip_ticks: i64,
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
    /// The kernel ships ZERO enabled strategies — what starts enabled is the
    /// operator's persisted intent (`strategy_state_path`) plus explicit
    /// `--enable-strategy` flags, so adding a strategy can never change what an
    /// existing session trades. Unknown names are reported and ignored, never
    /// fatal.
    pub enabled_strategies: Vec<String>,
    /// Strategies to switch OFF after the engine is installed, by name. Applied
    /// after `enabled_strategies`, so the explicit "off" wins if both name the
    /// same strategy.
    pub disabled_strategies: Vec<String>,
    /// Directory scanned at startup for user-layer strategy libraries
    /// (`*.dylib`/`*.so`): every library found is dlopen'd and REGISTERED —
    /// registration never enables; what trades is decided solely by
    /// `enabled_strategies` (persisted intent + CLI flags). `None` = no
    /// directory (auto-load off). Load failures are reported and skipped,
    /// never fatal — a broken file must not brick the kernel.
    pub strategy_dir: Option<String>,
    /// Where the effective enabled-set is recorded so runtime toggles and boot
    /// flags survive a restart (`data/strategy-state.json` in production; see
    /// [`crate::strategy_state`]). `None` disables persistence — the backtester
    /// and most in-process tests run with `None`, and a toggle then changes
    /// nothing on disk.
    pub strategy_state_path: Option<String>,
    /// The strategies the OPERATOR named on the command line
    /// (`--enable-strategy`), kept apart from `enabled_strategies` because the
    /// startup self-check is about an EXPLICIT request, not about what a
    /// persisted state file replays (#265).
    ///
    /// Empty for every in-process core and for the backtester, which is why the
    /// refusal below cannot surprise a caller that never asked for a strategy by
    /// name. The two sets are related by construction: `main` builds
    /// `enabled_strategies` as the persisted set plus these names.
    pub requested_strategies: Vec<String>,
    /// #265 — start even when none of the requested strategies resolved. The
    /// default is to REFUSE: an explicit request that loads nothing used to be
    /// one `WARN` line followed by a healthy-looking kernel that had no strategy
    /// at all (exit 0, socket listening, health probe green). `--allow-zero-strategies`
    /// is the acknowledged escape hatch for a harness that wants that boot.
    pub allow_zero_strategies: bool,
    pub min_round_age_sec: i64,
    pub size_usd: Decimal,
    pub min_shares: Decimal,
    pub max_shares: Decimal,
    /// P0 #202 — the per-entry budget as a percentage of the account's cash
    /// equity, replacing the absolute `size_usd` when > 0. 0 (the default)
    /// keeps every deployment's current order sizes exactly as they are.
    pub size_pct: Decimal,
    /// Engine trend confirmation window (sec) and floor (ms) — exposed so tests
    /// and ops can shorten the confirmation window without touching code.
    pub trend_confirm_sec: i64,
    pub trend_window_floor_ms: i64,
    /// spread_arb entry knobs (E15): `None` keeps the shipped default in
    /// [`crate::signal::SpreadArbConfig`]; a value overrides it in
    /// `engine_config` — the one mapping that drives both the live server and
    /// the backtester, so a sweep varies candidates purely through config.
    pub spread_arb_trend_entry_factor: Option<Decimal>,
    pub spread_arb_entry_min_obi: Option<Decimal>,
    pub spread_arb_entry_max_spread_pct: Option<Decimal>,
    pub spread_arb_entry_dip_max_pct: Option<Decimal>,
    pub spread_arb_entry_bounce_min_pct: Option<Decimal>,
    /// Turn-filter lookback window (sec); `None` = default 5.
    pub spread_arb_entry_bounce_window_sec: Option<i64>,
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
    /// How old an orderbook may be before the engine refuses to price off it,
    /// in milliseconds (issue #205). `0` = the check is OFF (any age accepted).
    ///
    /// This is the knob that decides how long after the feed goes quiet the bot
    /// stops trading, so it is an operator setting rather than a compiled
    /// constant: CLI `--max-orderbook-stale-ms` / `BK_MAX_ORDERBOOK_STALE_MS`,
    /// validated at startup and echoed in the boot log. The default is
    /// [`crate::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS`] — the value every
    /// deployment has always run with, so an unconfigured kernel is unchanged.
    pub max_orderbook_stale_ms: i64,
    /// How often the in-kernel accounting audit runs, in seconds (issue #189).
    /// 0 disables it. Default 30: the acceptance criterion is "drift is alerted,
    /// persisted and blocks new entries within 30s of appearing".
    pub audit_interval_sec: i64,
    /// Where to append one JSONL record per audit (issue #189). None = derive
    /// `reconcile.jsonl` beside the order log; `order_log_path: None` (the
    /// backtester and most tests) means no audit file at all.
    pub audit_log_path: Option<String>,
    /// Where to persist the applied-fill idempotency table (issue #178). None =
    /// derive `applied-fills.jsonl` beside the order log; `order_log_path: None`
    /// means no persistence, i.e. the table is per-session.
    pub applied_log_path: Option<String>,
    /// Whether a failing audit blocks NEW entries (default true; issue #189).
    /// Exits are never blocked — a drifted ledger must not be able to trap the
    /// bot in a position (the #174 failure mode). Turning this off keeps the
    /// alert and the persisted record while leaving the gates alone.
    pub audit_halt_entries: bool,
    /// Where to persist the SETTLEMENT journal (issue #175). None = derive
    /// `settlements.jsonl` beside the order log; `order_log_path: None` means no
    /// persistence, i.e. settlement idempotency is per-session.
    pub settlement_log_path: Option<String>,
    /// TEST HOOK, dry mode only: make the first N redemption attempts of every
    /// claim fail before one succeeds. `0` (the default) is the production
    /// behaviour — the simulated redemption always lands. A failure is a real
    /// `RedemptionFailure` through the real path (`note_failure` → backoff →
    /// retry), so the retry clock, the receivable and the manual-stop rule are
    /// exercised exactly as a venue failure would exercise them. The live path
    /// never reads this field: `simulate_redemptions` is the only consumer and it
    /// runs only where `settles_locally()` holds.
    pub dry_redeem_fail: u32,
    /// TEST HOOK, dry mode only: report those injected failures as `manual`, i.e.
    /// stop the automatic retries for good (`next_attempt_ms = i64::MAX`). Only
    /// meaningful together with [`CoreConfig::dry_redeem_fail`].
    pub dry_redeem_manual: bool,
    /// This core replays archived events, and may therefore charge a taker-fee
    /// schedule other than the shipped one (#234, item 4).
    ///
    /// `--fee-model` is the counterfactual knob for a replay, and the CLI refuses
    /// it without `--backtest`; this field is the same statement made where the
    /// charge happens. The two live charge sites
    /// ([`Core::live_taker_fee_pct`]) assert that a core which has NOT declared
    /// itself a replay is charging the shipped schedule — the isolation used to
    /// live only in the CLI, which meant a future caller of `set_fee_schedule`
    /// could have repriced live fills with no code objecting. Default `false`:
    /// only the backtester sets it.
    pub fee_schedule_replay: bool,
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
            spread_arb: {
                let mut sa = crate::signal::SpreadArbConfig {
                    trend_confirm_sec: self.trend_confirm_sec,
                    ..Default::default()
                };
                if let Some(v) = self.spread_arb_trend_entry_factor {
                    sa.trend_entry_factor = v;
                }
                if let Some(v) = self.spread_arb_entry_min_obi {
                    sa.entry_min_obi = v;
                }
                if let Some(v) = self.spread_arb_entry_max_spread_pct {
                    sa.entry_max_spread_pct = v;
                }
                if let Some(v) = self.spread_arb_entry_dip_max_pct {
                    sa.entry_dip_max_pct = v;
                }
                if let Some(v) = self.spread_arb_entry_bounce_min_pct {
                    sa.entry_bounce_min_pct = v;
                }
                if let Some(v) = self.spread_arb_entry_bounce_window_sec {
                    sa.entry_bounce_window_sec = v;
                }
                sa
            },
            size_usd: self.size_usd,
            min_shares: self.min_shares,
            max_shares: self.max_shares,
            size_pct: self.size_pct,
            // #205: the runtime freshness budget. It flows through this one
            // mapping, so a live server and a backtest replay see the same value.
            max_orderbook_stale_ms: self.max_orderbook_stale_ms,
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
    /// Evolution re-registration the toggle triggers). Afterwards the effective
    /// enabled-set is persisted (when `strategy_state_path` is configured) so
    /// the next boot replays it — the boot lists and the runtime toggles write
    /// the same file, and the last explicit intent wins.
    ///
    /// Returns `Err` when an EXPLICIT request resolved to zero live strategies
    /// (#265): the operator named strategies and not one of them exists, so the
    /// kernel would boot with a healthy-looking banner and trade nothing. The
    /// refusal belongs to the caller (the server ends the boot, the backtester
    /// fails the run) rather than to this method, so the facts stay inspectable
    /// and an embedded caller decides what a zero-strategy engine means to it.
    /// [`CoreConfig::allow_zero_strategies`] is the acknowledged opt-out.
    pub fn install_engine(&self, core: &mut Core) -> Result<(), String> {
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
        if let Some(path) = &self.strategy_state_path {
            crate::strategy_state::save(std::path::Path::new(path), &core.enabled_strategy_names());
        }
        self.strategy_startup_self_check(core)
    }

    /// The startup self-check (#265): state what was REQUESTED, what RESOLVED and
    /// what is enabled, and refuse a boot that resolves an explicit request to
    /// nothing.
    ///
    /// The line is printed unconditionally, refusals included, because it is the
    /// contract `scripts/walk-forward-sweep.mjs` reads back out of the kernel's
    /// log: a replay that could not resolve the strategies it asked for must be
    /// recognisable as such rather than arriving as a report with zero trades
    /// ("the strategy had no signal" and "there was no strategy" have to be
    /// different answers).
    ///
    /// A name the operator ALSO disabled explicitly is not part of the request —
    /// `--enable-strategy x --disable-strategy x` asked for nothing — so it can
    /// never be the reason this refuses.
    ///
    /// "Resolved" is asked of the live registry (`Core::strategy_names`), not of
    /// the enable loop: the question is whether the name exists as a strategy in
    /// this process, which is exactly what a missing library makes false. The
    /// persisted set is deliberately not consulted — a name that no longer
    /// resolves there was never requested by this invocation.
    fn strategy_startup_self_check(&self, core: &Core) -> Result<(), String> {
        let requested: Vec<&str> = self
            .requested_strategies
            .iter()
            .map(String::as_str)
            .filter(|n| !self.disabled_strategies.iter().any(|d| d.as_str() == *n))
            .collect();
        let registered = core.strategy_names();
        let resolved = requested
            .iter()
            .filter(|n| registered.iter().any(|r| r == *n))
            .count();
        let enabled = core.enabled_strategy_names();
        eprintln!(
            "blitzkrieg-core: strategy startup self-check: requested={} resolved={} enabled=[{}]",
            requested.len(),
            resolved,
            enabled.join(", ")
        );
        if requested.is_empty() || resolved > 0 || self.allow_zero_strategies {
            return Ok(());
        }
        Err(format!(
            "refusing to start: none of the {} explicitly requested strateg{} resolved to a live \
             strategy ({}), so this kernel would run with no strategy at all — while reporting \
             itself healthy (the engine's live set is [{}]). Take the libraries from the strategy \
             directory (`--strategy-dir`, `BK_STRATEGY_DIR`, or the default `user_layer/strategies` \
             of the checkout the binary lives in) — every library there is refused unless it sits \
             under an approved strategy root (`{}` under a repository root, or a directory named \
             in {}) — or acknowledge the empty set with --allow-zero-strategies. See the \
             `strategy auto-load` lines above for what was rejected and why",
            requested.len(),
            if requested.len() == 1 { "y" } else { "ies" },
            requested.join(", "),
            enabled.join(", "),
            crate::strategy_engine::loader::APPROVED_ROOTS.join("`, `"),
            crate::strategy_engine::loader::ENV_ALLOW_DIRS,
        ))
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
            libs.sort_by_key(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let rank = if name.contains("spread_arb") {
                    0
                } else if name.contains("trend_follow") {
                    1
                } else if name.contains("mean_reversion") {
                    2
                } else {
                    3
                };
                (rank, p.clone())
            });
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
            }
        }
        #[cfg(not(feature = "strategy-loading"))]
        {
            let _ = core;
        }
    }
}

/// True when `name` is the shipped `dog_strategy` crate's OWN artifact: the exact
/// stem (`dog_strategy`, `libdog_strategy.dylib`) or cargo's hashed form
/// (`dog_strategy-1a2b3c4d`). That example is loaded dynamically via IPC
/// `strategy.load` in tests and must not be auto-loaded at startup.
///
/// Deliberately not a substring test: any artifact whose stem merely ENDS in
/// `dog_strategy` would be silently dropped from startup with no error — the
/// exact failure mode that is hardest to notice, since the kernel boots fine and
/// simply never trades that strategy.
fn is_dog_strategy_artifact(name: &str) -> bool {
    let stem = name
        .strip_prefix("lib")
        .unwrap_or(name)
        .split('.')
        .next()
        .unwrap_or(name);
    stem == "dog_strategy" || stem.starts_with("dog_strategy-")
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
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.')
                || name == "deps"
                || name == "build"
                || name == "incremental"
                || name.contains("probe")
                || is_dog_strategy_artifact(name)
                || name.contains("devcheck")
            {
                continue;
            }
            collect_strategy_libs(&path, depth + 1, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("dylib") | Some("so")
        ) {
            // Test/probe strategies (e.g. dog_strategy, devcheck_probe) are loaded dynamically
            // via IPC `strategy.load` in tests (strategy-gate-check, strategy-evolution-check, strategy-devcheck)
            // and must not be auto-loaded at startup.
            if let Some(file_name) = path.file_name().and_then(|f| f.to_str())
                && (is_dog_strategy_artifact(file_name)
                    || file_name.contains("probe")
                    || file_name.contains("devcheck"))
            {
                continue;
            }
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
    /// Relative allocation weight (E16/#98): the three legs (spread_arb /
    /// trend_follow / mean_reversion) get weighted shares of the per-entry
    /// budget instead of identical ones. Scales the leg's own size override
    /// or the global `size_usd`; a weight can only shrink the budget, and a
    /// non-positive weight disables the leg's entries entirely.
    #[serde(with = "crate::decimal::opt", default)]
    pub size_weight: Option<Decimal>,
    /// This leg's own equity-relative budget (#202). `None` = the global
    /// `size_pct`; a value is clamped to it when the global is armed, and
    /// stands alone when the global is off.
    #[serde(with = "crate::decimal::opt", default)]
    pub size_pct: Option<Decimal>,
}

impl StrategyLimit {
    /// The sizing knobs this limit overrides (`None` across the board when it
    /// configures only caps — the engine then uses the globals).
    pub fn sizing(&self) -> crate::engine::StrategySize {
        crate::engine::StrategySize {
            size_usd: self.size_usd,
            min_shares: self.min_shares,
            max_shares: self.max_shares,
            size_weight: self.size_weight,
            size_pct: self.size_pct,
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
    /// Per-evolution step ceiling (Lock 2). `main` clamps this to the built-in
    /// ceiling before it gets here — a config file may tighten the gradient lock
    /// but nothing may widen it.
    pub max_gradient: Option<Decimal>,
    /// Directory for the per-strategy audit files (`<dir>/<strategy>.jsonl`).
    /// Defaults to `data/evolution`; a harness points it at a scratch directory
    /// so an experiment never writes into the operator's real audit history.
    pub audit_dir: Option<String>,
    /// E13 (#95): start in the unattended mode (`auto_evolve`). The switch is
    /// also runtime-toggleable and its LAST runtime state is persisted, so this
    /// only seeds the very first boot.
    pub auto_evolve: Option<bool>,
    /// Seconds between DEEP evolution rounds (the config file expresses this
    /// in minutes; `main` converts).
    pub evolution_cycle_secs: Option<i64>,
    /// Seconds an undecided proposal stays decidable (default 7 days).
    pub proposal_ttl_secs: Option<i64>,
    /// Knobs one DEEP-cycle variant moves simultaneously (>= 1).
    pub deep_dims: Option<usize>,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Dry,
            default_maker_timeout_ms: 5000,
            exit_taker_slip_ticks: 2,
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
            strategy_state_path: None,
            requested_strategies: Vec::new(),
            allow_zero_strategies: false,
            min_round_age_sec: 30,
            size_usd: Decimal::new(25, 1), // 2.5
            min_shares: Decimal::from(10),
            max_shares: Decimal::from(10),
            size_pct: Decimal::ZERO,
            trend_confirm_sec: 60,
            trend_window_floor_ms: 10_000,
            spread_arb_trend_entry_factor: None,
            spread_arb_entry_min_obi: None,
            spread_arb_entry_max_spread_pct: None,
            spread_arb_entry_dip_max_pct: None,
            spread_arb_entry_bounce_min_pct: None,
            spread_arb_entry_bounce_window_sec: None,
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
            // #205: the shipped freshness budget, unchanged.
            max_orderbook_stale_ms: crate::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS,
            audit_interval_sec: 30,
            audit_log_path: None,
            applied_log_path: None,
            audit_halt_entries: true,
            settlement_log_path: None,
            dry_redeem_fail: 0,
            dry_redeem_manual: false,
            // #234: nothing but the backtester may charge a counterfactual fee.
            fee_schedule_replay: false,
        }
    }
}

// ── Fee model (#182, #203) ───────────────────────────────────────────────────
//
// The taker fee was a formula copied into four places: the kernel's charge path
// and three gate/reconcile scripts. Copies cannot notice each other changing,
// and the failure is one-directional and quiet — the kernel switches its default
// schedule, the scripts keep asserting the old arithmetic, and a gate that
// passes stops meaning what it says. The scripts now take the per-share fee from
// `core.feeQuote` (below) and pin the DECLARED model, so a change here without a
// change there is a red gate instead of a wrong green one.
//
// The schedule itself lives in ONE place — `exit_policy::FeeSchedule`, whose
// `source` field names the publication each set of parameters comes from — and
// the quote below reports whatever that is, so a fee can never be charged under
// one name and declared under another. `official` (Polymarket's published
// `rate * p * (1 - p)`, exponent 1) is the succession path: switching the
// default means changing `exit_policy::legacy_quadratic_schedule` to it AND the
// pinned expectation in `scripts/lib/fee-model.mjs`, in the same change — which
// is the point. What that costs the strategies is measured, not argued:
// `scripts/fee-model-sensitivity-check.mjs` replays the frozen corpus under both
// schedules (#203).
//
// #234 finished the close-out. The curve's arithmetic has ONE spelling in the
// kernel — `FeeSchedule::fee_per_share` — so `fee_quote` no longer recomputes
// `rate * (p*(1-p))^exponent` beside it; `model_matches` now holds the charged
// fee against the PINNED parameters (`exit_policy::pinned_fee_parameters`), a
// source independent of the schedule it is checking, so a changed rate is
// reported instead of hiding behind two identical spellings. The authoritative
// cross-language self-check is `scripts/core-parity.mjs::assertPinnedFeeModel`
// against `scripts/lib/fee-model.mjs`; this one is the in-process half.

/// The fee actually charged per share at `price` under `schedule`:
/// `(schedule.fee_pct(p)/100) * p`, spelled exactly as the charge path spells it
/// (`apply_delta_effects`, `reconcile`), so a quote cannot drift from a fill.
/// Maker fills are exempt and are reported separately as zero.
fn charged_per_share(schedule: crate::exit_policy::FeeSchedule, price: Decimal) -> Decimal {
    (schedule.fee_pct(price) / Decimal::ONE_HUNDRED) * price
}

/// The fee actually charged per share at `price` under the schedule in force.
pub fn charged_fee_per_share(price: Decimal) -> Decimal {
    charged_per_share(crate::exit_policy::fee_schedule(), price)
}

/// Does the schedule in force still price what this repository PINS for its
/// declared name (#234, item 1)?
///
/// The pin is `exit_policy::pinned_fee_parameters` — deliberately NOT the
/// registry the schedule itself came from, because a check that reads the same
/// source as the thing it checks cannot fail: flipping the shipped rate from
/// 0.125 to 0.07 moved the declaration and the charge together and this reported
/// `true` while the fee moved 44%. Reading the pin makes that flip report
/// `false`, which is what the field was introduced to mean.
///
/// The pin is held against the schedule's PARAMETERS, not against the number one
/// sampled price happens to produce. `fee_quote` samples a single price (0.5
/// unless the caller asks otherwise) and two different `(rate, exponent)` pairs
/// can cross there: `0.03125*(p(1-p))^1` prices the pinned legacy curve's
/// `0.0078125` per share at p=0.5 and a different fee at every other price, so a
/// value-only comparison at the sampled point reports a wrong configuration as a
/// match (see `fee_quote_tests::model_matches_rejects_a_curve_that_only_coincides_at_the_sampled_price`).
/// Because both sides of that comparison run the same expression, it cannot see
/// a change to the expression itself either — what it can see is exactly the
/// parameters, which is what the pin describes. Drift in the arithmetic is
/// covered where the arithmetic lives (`exit_policy`'s own pricing tests) and by
/// the cross-language gate below, which recomputes the fee independently.
/// A schedule whose name this repository cannot describe is reported as NOT
/// matching, rather than as a green "nothing to check".
///
/// This is the in-process half. The authoritative, cross-language fee self-check
/// is `scripts/core-parity.mjs::assertPinnedFeeModel`, which holds this kernel's
/// `core.feeQuote` against the pinned table in `scripts/lib/fee-model.mjs` — the
/// two pins must be changed together, in the same change as the schedule.
fn schedule_reproduces_pin(schedule: crate::exit_policy::FeeSchedule) -> bool {
    let Some((rate, exponent)) = crate::exit_policy::pinned_fee_parameters(schedule.name) else {
        return false;
    };
    schedule.rate == rate && schedule.exponent == exponent
}

/// Read-only quote of the fee schedule (#182). `price` defaults to the widest
/// point of the schedule (0.5) so a caller that only wants the model metadata
/// does not have to invent a price.
pub fn fee_quote(price: Option<Decimal>) -> crate::ipc::schema::FeeQuoteResult {
    fee_quote_at(crate::exit_policy::fee_schedule(), price)
}

/// [`fee_quote`] with the schedule supplied, so a test can ask what a
/// DIFFERENT schedule would report — the check has to be able to say "no", and
/// the process-wide schedule can only be set once (#234).
fn fee_quote_at(
    schedule: crate::exit_policy::FeeSchedule,
    price: Option<Decimal>,
) -> crate::ipc::schema::FeeQuoteResult {
    let price = price.unwrap_or_else(|| dec!(0.5));
    let charged = charged_per_share(schedule, price);
    let model_matches = schedule_reproduces_pin(schedule);
    crate::ipc::schema::FeeQuoteResult {
        model: schedule.name.to_string(),
        rate: schedule.rate,
        exponent: schedule.exponent,
        price,
        fee_per_share: charged,
        fee_pct_of_price: schedule.fee_pct(price),
        maker_fee_per_share: Decimal::ZERO,
        model_matches,
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
/// destructuring loop consumes: id, side, remaining, token, condition,
/// strategy, asset, direction, round slot, maker timeout.
type EscalationTarget = (
    String,
    Side,
    Decimal,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
);

/// Append-only journal of the OME's applied-fill idempotency table (issue #178).
///
/// The table used to be memory-only, so a restart forgot which venue trade ids
/// had already been booked and the venue's own re-delivery (a WS replay or the
/// REST sweep's recent-trade window) booked the same fill a second time — the
/// position doubled and the cash moved twice. Rebuilding it from `trades.jsonl`
/// is not possible: that log stores ONE AGGREGATED record per CLOSED position and
/// carries no venue trade ids at all, so there is nothing to fold a fill key from.
///
/// Same shape as the order log: one line per MUTATION (a key's applied size, or
/// a tombstone when it is evicted), folded latest-wins on load. The file lives
/// beside the order log because it is the OME's state, and it is compacted to the
/// live table on restore so it cannot grow across restarts.
struct AppliedFillLog {
    path: std::path::PathBuf,
}

impl AppliedFillLog {
    fn new(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Append the mutations. Best effort, like every other log here: a write
    /// failure must never interrupt trading (the in-memory table stays
    /// authoritative for this run, and the OME warns when its journal is full).
    fn append(&self, records: &[AppliedFillRecord]) {
        if records.is_empty() {
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut buf = String::new();
        for r in records {
            match serde_json::to_string(r) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(e) => tracing::warn!(error = %e, "cannot serialize applied-fill record"),
            }
        }
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            if let Err(e) = f.write_all(buf.as_bytes()) {
                tracing::warn!(error = %e, path = %self.path.display(), "applied-fill log append failed");
            }
        } else {
            tracing::warn!(path = %self.path.display(), "cannot open applied-fill log");
        }
    }

    /// Load the mutations in file order (the OME folds them). Unparseable lines
    /// are skipped, never fatal: a corrupt tail must not stop the core from
    /// starting, and every record kept is a duplicate the table will refuse.
    fn load(&self) -> Vec<AppliedFillRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut skipped = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<AppliedFillRecord>(line) {
                Ok(r) => out.push(r),
                Err(_) => skipped += 1,
            }
        }
        if skipped > 0 {
            tracing::warn!(
                skipped,
                path = %self.path.display(),
                "applied-fill log: skipped unparseable lines"
            );
        }
        out
    }

    /// Rewrite the log as the current table (one line per live key). Called after
    /// a restore, so the file is the table rather than every mutation ever made.
    fn compact(&self, snapshot: &[AppliedFillRecord]) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut buf = String::new();
        for r in snapshot {
            match serde_json::to_string(r) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(e) => tracing::warn!(error = %e, "cannot serialize applied-fill record"),
            }
        }
        if let Err(e) = std::fs::write(&self.path, buf) {
            tracing::warn!(error = %e, path = %self.path.display(), "applied-fill log compact failed");
        }
    }
}

/// In-kernel accounting audit state (issue #189). Session-scoped: the anchor and
/// the strike counter describe THIS run's observations, and the durable record is
/// the audit JSONL.
#[derive(Debug, Clone, Default)]
struct AuditRuntime {
    /// Last audit instant (0 = never).
    last_at_ms: i64,
    /// Last audit that was written to the log even though it passed (heartbeat).
    last_heartbeat_ms: i64,
    /// The baseline the anchored cash identity is measured from. Taken at the
    /// first audit whose structural checks pass and NEVER moved while the books
    /// are wrong — re-anchoring would hide the drift it exists to expose.
    anchor: Option<CashIdentity>,
    report: Option<AuditReport>,
    /// Consecutive cross-source-only failures; two in a row halt (a single one
    /// can be a stale venue read).
    cross_source_failures: u32,
    /// New entries are currently blocked by a failing audit.
    halted: bool,
    halt_reason: String,
    runs: u64,
    failures: u64,
    /// Where each audit appends its verdict (None = no persistence).
    audit_log_path: Option<String>,
    /// The last time the ledger's cash was SET from the venue rather than moved
    /// by a fill: `(at_ms, delta)`. The plugin host does this deliberately when
    /// nothing rests, so the trade-leg check can tell the venue's own correction
    /// from a local inconsistency instead of halting entries over it.
    last_realign: Option<(i64, Decimal)>,
}

/// The kernel's last recorded error, as `engine.stats.lastError` exposes it.
///
/// One slot with exactly one writer ([`Core::note_error`]): whichever path went
/// wrong last — venue refusal, safety net, or the kernel's own refusal of a leg
/// — the panel banner and any outside observer read the same fact, with the code
/// that classifies it and the instant it happened. Before #180 the only such
/// slot was `last_venue_error`, which the taker rejection below never touched,
/// so `lastError` stayed empty (or stale) while orders were being refused.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastError {
    pub ts_ms: i64,
    pub code: CoreErrorCode,
    pub message: String,
}

impl LastError {
    /// How the LEGACY `engine.stats.lastVenueError` key renders this record.
    ///
    /// The shipped panel (`ui/webapp/webui/src/pages/Overview.vue`) renders this
    /// string as free text, and that is the compatibility this method owes: the
    /// shape `{ tsMs, message }` and the "last error, newest wins" instant are
    /// unchanged, so the banner keeps working without a frontend change.
    ///
    /// It is NOT byte-identical to the pre-#180 strings on every path, and the
    /// earlier claim that it was is wrong. The venue-reject path is identical
    /// (`{:?}` on the code, a colon, the message). The self-check and reconcile
    /// paths carried NO code prefix before #180 — they were bare prose
    /// (`"trading self-check failed: …"`, `"reconciliation sweep failed (n): …"`)
    /// — and now gain one, because one slot with one writer cannot render two
    /// shapes. Nothing shipped parses those two strings: the panel shows them as
    /// text, and the only test that reads the key asserts `contains`, not
    /// equality. `lastError` above is the structured form to migrate to.
    pub fn legacy_message(&self) -> String {
        format!("{:?}: {}", self.code, self.message)
    }
}

/// One placement's result: the order's id, the status the OME now holds for it,
/// and — when the kernel itself refused the leg after accepting it (a
/// dry/read-only taker whose book cannot fill it) — the structured reason.
///
/// `status == Rejected` with a `rejection` is the leg the venue would have
/// killed; `status == Rejected` without one is a leg an external actor (the
/// venue, a reconcile) retired. Before #180 the drier path returned neither, so
/// `orders.place` answered a bare `REJECTED` and the caller could not tell "no
/// crossing liquidity" from "not enough depth" from "risk said no".
#[derive(Debug, Clone)]
pub struct PlaceOutcome {
    pub order_id: OrderId,
    pub status: OrderStatus,
    pub rejection: Option<CoreError>,
}

/// Upper bound on the kernel's pending-close exit-reason table (issue #190).
///
/// The table holds one entry per token whose closing SELL has been submitted but
/// not yet filled — a *pending-close* ledger, not a history — so its live
/// working set is the engine's tradeable fan-out: the round's tokens across the
/// configured assets (8 tokens × 3 assets = 24 on the shipped Polymarket
/// deployment) plus whatever manual flattens add on top. 64 is ≈2.5× the widest
/// live set, so a legitimate session never reaches the bound; it exists so a leak
/// is bounded for the life of the process instead of growing forever. Eviction
/// prefers entries whose position is already gone (stale by construction) and
/// drops the oldest otherwise; both are counted in `engine.stats.exitReasons`.
const EXIT_REASON_TABLE_MAX: usize = 64;

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
    ///
    /// A PENDING-CLOSE ledger, not a history (issue #190): an entry exists only
    /// while the token still has an open position, and the table can never
    /// exceed [`EXIT_REASON_TABLE_MAX`]. A leaked entry would be inherited by
    /// the NEXT position on the same token and mis-attribute its exit.
    exit_reasons: HashMap<TokenId, ExitReason>,
    /// Insertion order of [`Self::exit_reasons`], oldest first. The map cannot
    /// name its own oldest entry and the bound has to evict one; kept in
    /// lockstep with the map's key set by the four helpers below.
    exit_reason_order: VecDeque<TokenId>,
    /// Entries dropped by the [`EXIT_REASON_TABLE_MAX`] bound, session-scoped
    /// and surfaced in `engine.stats`: an eviction loses real exit attribution,
    /// so it is counted rather than silent.
    exit_reasons_evicted: u64,
    /// Entries dropped because their token no longer has an open position — the
    /// expected path (a close, a round rollover).
    exit_reasons_swept: u64,
    /// Strategy close intents drained from the engine in `engine_evaluate` and
    /// consumed by the shared exit-submission path in `run_exit_checks`.
    strategy_exits: Vec<crate::strategies::StrategyExitIntent>,
    /// Optional self-driving engine (P3). When present, book/spot/round events
    /// flow in and the core evaluates strategies and places orders on its own.
    /// Crate-visible so the replay driver can host strategies without going
    /// through the loader path (the kernel ships none to load).
    pub(crate) engine: Option<crate::engine::Engine>,
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
    /// When this core's ACCOUNTING SESSION began, in epoch ms (issue #200).
    ///
    /// A seeded (`dry`/`readonly`) core re-seeds its cash ledger here
    /// (`Ledger::set_balance(dry_seed_balance)` in `new`) while its trade log
    /// and position log outlive the process. A cold ledger beside a trade book
    /// that still holds earlier sessions answers a different question than the
    /// cash identity asks, so an out-of-process audit of a seeded core has to
    /// scope `realized` to the trades closed at/after this instant. A live core
    /// ignores it: its opening cash is the venue's, and its identity telescopes
    /// across polls instead.
    started_at_ms: i64,
    /// Optional mirror of every market-data event into a JSONL archive (P-1.3).
    /// The backtester replays this file.
    event_archive: Option<crate::data_source::EventArchive>,
    /// Per-strategy order/trade accounting (P-1.1). Session-scoped: reset on
    /// restart, like `stats`; the durable per-trade record is the trade log.
    strategy_accounting: HashMap<String, StrategyAccounting>,
    /// Venue-rejection cooldowns keyed by placement intent (`internal_key`):
    /// after the venue refuses an order, retries of the SAME intent back off
    /// exponentially and stop after a cap — a stuck exit once burned 380+
    /// rejections hammering a venue that kept refusing at full tick rate.
    place_cooldowns: HashMap<String, RejectCooldown>,
    /// Orders whose escalation this process already reported as skipped because
    /// the cross the book offers is refused by the risk gate (#261). The sweep
    /// retries on every maker timeout, so without this the same unaffordable
    /// cross would reprint the same line every few seconds. An id leaves the
    /// set as soon as the order stops being live, or when a later escalation
    /// actually goes through.
    escalation_skips: HashSet<String>,
    /// Venue cancels the core has already committed to locally (order marked
    /// Cancelled by a regime pull, round cleanup, escalation or explicit
    /// cancel) while the venue may still hold the order resting. The live
    /// bridge drains this and issues the actual venue cancels — retiring an
    /// order locally without telling the venue leaves a resting orphan.
    pending_venue_cancels: Vec<String>,
    /// Consecutive LIVE venue rejections (a successful placement resets).
    /// Reaching the threshold trips the freeze (kill switch).
    consecutive_venue_rejects: u32,
    /// Consecutive failed reconciliation sweeps (a successful sweep resets).
    /// Reaching the threshold freezes trading — a blind safety net is the
    /// failure mode that lets ghosts and orphans accumulate (E31-b).
    consecutive_sweep_failures: u32,
    /// Last error of any kind recorded this session (venue refusal, failed
    /// self-check or reconcile sweep, kill switch, kernel-side order refusal),
    /// surfaced to the panel as `engine.stats.lastError`. Session-scoped: a
    /// restart starts with no error, and nothing clears it but a new one.
    last_error: Option<LastError>,
    /// Newest external (unknown-order) fill timestamp already folded into the
    /// position book; persisted next to the position log.
    recon_watermark_ms: i64,
    /// Newest trading-capability self-check report (panel visibility).
    last_self_check: Option<blitzkrieg_market_api::SelfCheckReport>,
    /// Durable journal of the applied-fill idempotency table (issue #178). The
    /// OME never touches the filesystem; the service drains its mutation journal
    /// after every applied fill and appends it here, and restores the table from
    /// it at startup.
    applied_log: Option<AppliedFillLog>,
    /// In-kernel accounting audit state (issue #189).
    audit: AuditRuntime,
    /// Settlement + redemption book (issue #175): which positions a market
    /// resolution has settled (durable, so a second round cannot re-book them),
    /// and the claims whose collateral is still on-chain. In dry mode the core
    /// resolves its own markets and simulates the redemption; in live mode the
    /// venue plugin answers and redeems.
    settlement: SettlementBook,
    /// Last free-cash figure the venue reported, as `(at_ms, free)` — the venue
    /// leg of the audit compares against it.
    venue_free: Option<(i64, Decimal)>,
    /// F4: close-accounting credentials for fill-driven full closes, keyed by
    /// the OME trade key. A MATCHED sell that emptied a position books TradeDb /
    /// strategy PnL / breaker immediately; if the venue later reports that same
    /// trade FAILED, the credential is what lets the service unwind each of
    /// those ledgers instead of leaving phantom profit behind. FIFO-capped:
    /// the FAILED normally arrives seconds after the fill that created it.
    close_credentials: std::collections::VecDeque<(String, CloseCredential)>,
    next_id: u64,
    tx: Option<mpsc::UnboundedSender<Event>>,
}

/// F4: what a fill-driven full close changed, so a later FAILED status for the
/// same trade can reverse every one of those ledgers exactly.
struct CloseCredential {
    /// The position-book row as it stood BEFORE the closing fill removed it —
    /// the exact inventory to restore.
    restored: crate::position::OpenPosition,
    /// The closed record `on_position_closed` booked (TradeDb / accounting /
    /// breaker / daily PnL), for unwinding.
    closed: crate::position::ClosedPosition,
    /// The strategy breaker's consecutive-loss count just before this close
    /// was recorded: a reversed WIN had reset the streak, and that is the only
    /// recoverable value (retract restores it).
    streak_before: u32,
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
        // Both side-log paths are derived from the order log's directory when not
        // set explicitly: they belong to the same durable state, and a deployment
        // that persists orders wants its idempotency table and audit trail on
        // disk too. `order_log_path: None` (the backtester, most tests) means no
        // persistence for any of them.
        let side_log_path = |explicit: &Option<String>, name: &str| -> Option<String> {
            if let Some(p) = explicit {
                return Some(p.clone());
            }
            let dir = std::path::Path::new(config.order_log_path.as_deref()?).parent()?;
            Some(dir.join(name).to_string_lossy().to_string())
        };
        let applied_log =
            side_log_path(&config.applied_log_path, "applied-fills.jsonl").map(AppliedFillLog::new);
        let audit_log = side_log_path(&config.audit_log_path, "reconcile.jsonl");
        let settlement_log = side_log_path(&config.settlement_log_path, "settlements.jsonl");
        let settlement = SettlementBook::new(settlement_log.as_deref().map(std::path::Path::new));
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
                if let Some(v) = t.auto_evolve {
                    c.auto_evolve = v;
                }
                if let Some(v) = t.evolution_cycle_secs {
                    c.evolution_cycle_secs = v;
                }
                if let Some(v) = t.proposal_ttl_secs {
                    c.proposal_ttl_secs = v;
                }
                if let Some(v) = t.deep_dims {
                    c.deep_dims = v.max(1);
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
            exit_reason_order: VecDeque::new(),
            exit_reasons_evicted: 0,
            exit_reasons_swept: 0,
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
            // The session clock for the accounting identity (issue #200). Read
            // once, at construction: the ledger below is seeded at the same
            // instant, so "cash since the seed" and "trades since startedAtMs"
            // describe the same window.
            started_at_ms: now_ms(),
            event_archive,
            strategy_accounting: HashMap::new(),
            place_cooldowns: HashMap::new(),
            escalation_skips: HashSet::new(),
            pending_venue_cancels: Vec::new(),
            consecutive_venue_rejects: 0,
            consecutive_sweep_failures: 0,
            last_error: None,
            recon_watermark_ms: 0,
            last_self_check: None,
            applied_log,
            audit: AuditRuntime {
                audit_log_path: audit_log,
                ..Default::default()
            },
            settlement,
            venue_free: None,
            close_credentials: std::collections::VecDeque::new(),
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
    ///
    /// Also restores the applied-fill idempotency table (issue #178) — without it,
    /// the venue's own re-delivery of an already-booked trade id books the fill a
    /// second time after every restart — and re-establishes the BUY reservations
    /// those restored orders still commit (issue #181), which a fresh Ledger would
    /// otherwise start without.
    pub fn restore_orders(&mut self) -> usize {
        let applied = self.restore_applied();
        if applied.0 > 0 {
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!(
                    "restored {} applied-fill key(s) from {} after restart (duplicate venue \
                     trades stay idempotent)",
                    applied.0, applied.1
                ),
            });
        }
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
            // A re-adopted order still commits its unfilled notional. The local
            // reservation table is empty on a fresh process, so without this the
            // reserve gate would believe the whole balance is free and the next
            // entries would over-commit against cash the venue has already
            // spoken for (issue #181).
            let mut restored_notional = Decimal::ZERO;
            for o in &live {
                if o.side != Side::Buy {
                    continue;
                }
                let remaining = (o.size - o.filled_size).max(Decimal::ZERO);
                let notional = o.price * remaining;
                self.ledger.sync_buy_reservation(&o.order_id, notional);
                restored_notional += notional;
            }
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message: format!(
                    "recovered {n} live order(s) from the order log after restart \
                     ({restored_notional} of BUY notional re-reserved)"
                ),
            });
        }
        n
    }

    /// Fold the persisted applied-fill journal into the OME. Returns
    /// `(keys restored, path)`.
    fn restore_applied(&mut self) -> (usize, String) {
        let Some(log) = self.applied_log.as_ref() else {
            return (0, String::new());
        };
        let records = log.load();
        if records.is_empty() {
            return (0, log.path().display().to_string());
        }
        self.ome.restore_applied(records);
        let n = self.ome.applied_len();
        // The file was every mutation ever made; it is now the live table.
        self.ome.take_applied_journal();
        log.compact(&self.ome.applied_snapshot());
        (n, log.path().display().to_string())
    }

    /// Append whatever the OME journalled since the last drain. Called after every
    /// applied fill (the only thing that changes the table), so the file is
    /// crash-consistent with the in-memory table at all times.
    fn persist_applied_journal(&mut self) {
        let records = self.ome.take_applied_journal();
        if records.is_empty() {
            return;
        }
        if let Some(log) = self.applied_log.as_ref() {
            log.append(&records);
        }
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
        // The watermark outlives the position set: a flat book must still
        // remember which external fills were already accounted for.
        self.recon_watermark_ms = db.load_watermark();
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
        // A settlement whose close never landed is a position that looks open but
        // is already paid: finish it before anything trades (issue #175).
        self.recover_settlements();
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
            message: format!("cancelled resting venue order {venue_order_id}"),
        });
    }
    pub fn mode(&self) -> Mode {
        self.config.mode
    }

    /// The instant this core's accounting session began (epoch ms) — see the
    /// field doc. Exposed read-only over `core.ready` so an external audit can
    /// scope the seeded cash identity to the trades this process can be
    /// responsible for (issue #200).
    pub fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }
    /// Read-only fee quote (#182) — the `core.feeQuote` entry point, so a gate or
    /// a reconcile script asks the kernel for the schedule it is charging instead
    /// of carrying its own copy of the formula. It sits on `Core` rather than
    /// being a bare function because the day the schedule becomes a per-core
    /// setting (the `--taker-fee-legacy` switch in flight) the lookup belongs
    /// here, and only here.
    pub fn fee_quote(&self, price: Option<Decimal>) -> crate::ipc::schema::FeeQuoteResult {
        fee_quote(price)
    }

    /// Set/realign the ledger's cash from the venue (startup seed and the periodic
    /// free-cash sync both land here).
    ///
    /// In LIVE the figure is remembered as the venue leg of the accounting audit
    /// (issue #189) — an audit compares it against what the ledger believes is
    /// free, and a mismatch that survives the venue's own retry is drift.
    pub fn set_balance(&mut self, b: Decimal) {
        if self.config.mode == Mode::Live {
            let ts = now_ms();
            let previous = self.ledger.balance();
            if previous != b {
                // The host realigns the ledger to the venue when there are no
                // resting commitments, so a change between two reports is either
                // a real fill or cash that moved without a fill event. The move is
                // recorded so the audit's anchored identity can separate the two
                // instead of reporting the venue's own correction as local drift.
                self.audit.last_realign = Some((ts, b - previous));
                tracing::info!(
                    previous = %previous,
                    reported = %b,
                    "ledger cash set from the venue (no resting commitment)"
                );
            }
            self.venue_free = Some((ts, b));
        }
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
            self.rewire_hot_params(now_ms());
            // Record the operator's intent: the next boot replays this set.
            // `None` (backtests, most tests) writes nothing.
            if let Some(path) = &self.config.strategy_state_path {
                crate::strategy_state::save(
                    std::path::Path::new(path),
                    &self.enabled_strategy_names(),
                );
            }
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
                    // D-31: if the declaration carries a `time_left_sec` floor,
                    // state it — the receipt is the operator's only pre-enable
                    // view of how far the opt-out actually reaches.
                    let declared = if loaded.gate_exemptions.any() {
                        let floor = match loaded.gate_exemptions.timing_min_time_left_sec {
                            Some(n) => format!(" (timing floor {n}s)"),
                            None => String::new(),
                        };
                        format!(
                            "; declares gate exemptions: {}{floor}",
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
                            self.rewire_hot_params(now_ms());
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
    fn rewire_hot_params(&mut self, now_ms: i64) {
        if let Some(e) = self.engine.as_mut() {
            if self.shadow_evolution.is_enabled() {
                // The whole host set, enabled or not: a declaration is read off the
                // live instance (E2-c), and `register_strategies` removes the cell of
                // anything ABSENT — so filtering by `enabled` here would delete a
                // switched-off strategy's evolved parameters and rollback anchor,
                // making a toggle lossy. A disabled strategy simply never emits the
                // candidates that would consume them.
                //
                // `now_ms` is the clock the CALLER acts at, forwarded rather than
                // re-read: this re-registration re-anchors every variant, and the
                // birth stamp it writes is what the observation floor is measured
                // against — one operation, one clock (#250).
                self.shadow_evolution
                    .register_strategies(&e.strategy_refs(), now_ms);
                e.set_hot_params(Some(self.shadow_evolution.registry()));
            } else {
                e.set_hot_params(None);
            }
        }
    }

    pub fn shadow_evolution_enable(&mut self, now_ms: i64) -> bool {
        self.shadow_evolution.enable(now_ms);
        self.rewire_hot_params(now_ms);
        true
    }

    pub fn shadow_evolution_disable(&mut self) -> bool {
        self.shadow_evolution.disable();
        self.rewire_hot_params(now_ms());
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

    /// E13: every known proposal's latest state, newest first (IPC surface).
    pub fn shadow_evolution_proposals(
        &self,
        limit: usize,
    ) -> Vec<crate::shadow_evolution::EvolutionProposal> {
        self.shadow_evolution.all_proposals(limit)
    }

    /// E13: the operator's verdict on one held proposal. String is the IPC-side
    /// verb ("accept"/"reject"/"defer"); anything else is refused loudly.
    pub fn shadow_evolution_decide(
        &mut self,
        id: &str,
        decision: &str,
        now_ms: i64,
    ) -> Result<crate::shadow_evolution::DecisionResult, String> {
        let decision = match decision {
            "accept" | "accepted" => crate::shadow_evolution::Decision::Accept,
            "reject" | "rejected" => crate::shadow_evolution::Decision::Reject,
            "defer" | "deferred" => crate::shadow_evolution::Decision::Defer,
            other => return Err(format!("unknown decision '{other}' (accept|reject|defer)")),
        };
        self.shadow_evolution.decide(id, decision, now_ms)
    }

    /// E13: the auto-evolve switch (the UIs' checkbox). Persisted across restarts.
    pub fn shadow_evolution_set_auto(&mut self, on: bool) -> bool {
        self.shadow_evolution.set_auto_evolve(on);
        self.shadow_evolution.auto_evolve()
    }

    /// E13: the 72h deep-evolution clock. Fires a compound-mutation round when
    /// the interval has elapsed; the returned event, if any, is emitted.
    pub fn shadow_evolution_maybe_cycle(&mut self, now_ms: i64) {
        if let Some(ev) = self.shadow_evolution.maybe_evolution_cycle(now_ms) {
            self.emit(Event::EvolutionCycle {
                cycle_seq: ev.cycle_seq,
                dims: ev.dims,
                strategies: ev.strategies,
                at_ms: ev.at_ms,
            });
        }
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
    ///
    /// Called every tick regardless of the engine switch, because the pass is not
    /// only "evaluate": it also expires undecided proposals past their TTL, which
    /// is bookkeeping the operator's "7 天未决自动过期" is a promise about. The
    /// switch itself is the manager's own gate (#249) — one place, so a disabled
    /// engine can neither evaluate nor adopt, and cannot silently stop ageing the
    /// queue it left behind either.
    pub fn shadow_evolution_evaluate(&mut self, now_ms: i64) {
        for outcome in self.shadow_evolution.evaluate(now_ms) {
            self.emit_shadow_outcome(outcome);
        }
    }

    fn emit_shadow_outcome(&self, outcome: EvolutionOutcome) {
        match outcome {
            EvolutionOutcome::Signal(sig) => self.emit(Event::EvolutionSignal { signal: sig }),
            EvolutionOutcome::Applied(sig) => self.emit(Event::EvolutionApplied { signal: sig }),
            EvolutionOutcome::Proposed(proposal) => {
                self.emit(Event::EvolutionProposed { proposal })
            }
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
        self.rewire_hot_params(now_ms());
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
            crate::engine::DataEvent::RoundMarkets { .. } => {
                self.stats.rounds += 1;
                // A round rollover replaces the whole round (`engine.markets`
                // carries the new list), so it is the moment any pending-close
                // reason whose position is gone can no longer belong to
                // anything live (issue #190).
                self.sweep_exit_reasons();
            }
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
        // E13: the 72h DEEP round rides the same cadence, so a long-running
        // process re-anchors its variant sets (compound mutants) without an
        // operator. The check itself is one comparison per cycle.
        self.shadow_evolution_maybe_cycle(now_ms);
        let engine = self.engine.as_mut().expect("engine present");
        // #202: the equity-relative sizing is a percentage of the account, and
        // the account is the ledger — pushed here, once per cycle, so a ticket
        // can never be sized against a balance the process no longer has.
        engine.set_equity_usd(self.ledger.balance());
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
            // E16/#98 portfolio-level exposure cap (0 = off): the account's
            // total open commitment across ALL strategies is bounded too —
            // per-strategy caps partition it, but their sum needs its own
            // ceiling. The check ADDS a constraint; the RiskGate hard bounds
            // (kill switch, price band, per-order cap) are untouched.
            if let Err(reason) = self.portfolio_limit_ok(&req) {
                self.stats.strategy_limit_rejected += 1;
                let acc = self.strategy_accounting.entry(name.clone()).or_default();
                acc.limit_rejected += 1;
                *acc.rejection_causes
                    .entry("limit.portfolioNotionalCap".into())
                    .or_default() += 1;
                tracing::info!(target: "strategy", "entry rejected: strategy={name} cause=limit.portfolioNotionalCap reason={reason}");
                continue;
            }
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

    /// Portfolio-level exposure cap (E16/#98): the sum of open notional across
    /// ALL strategies plus the incoming entry stays under
    /// `risk.max_open_notional_usd` (0 = disabled).
    fn portfolio_limit_ok(&self, req: &crate::model::OrderRequest) -> Result<(), String> {
        // Read the LIVE risk config: a runtime update (risk_config_mut /
        // IPC) lands in the gate, not in the static CoreConfig.
        let cap = self.risk.config().max_open_notional_usd;
        if cap <= Decimal::ZERO {
            return Ok(());
        }
        let used: Decimal = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.cost_usd)
            .sum();
        let incoming = req.price * req.size;
        if used + incoming > cap {
            return Err(format!(
                "portfolio notional cap ({used}+{incoming} > {cap})"
            ));
        }
        Ok(())
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
                        size_pct: self.config.size_pct,
                        strategy_scoped: false,
                        size_weight: None,
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
                    "sizeWeight": effective.size_weight.map(dec_json),
                    // #202: the equity-relative budget in force for this leg and
                    // what it is worth on the CURRENT balance, so "how much will
                    // this account commit per entry?" is a number on the panel
                    // rather than a reading of the flags.
                    "effectiveSizePct": dec_json(effective.size_pct),
                    "entryBudgetUsd": dec_json(
                        self.engine
                            .as_ref()
                            .map(|e| e.entry_budget_usd(&name))
                            .unwrap_or(effective.size_usd),
                    ),
                    // ── E2-b declared gate exemptions + per-strategy gate counts ──
                    "gateExemptions": declared.gates(),
                    // D-31: the declared `time_left_sec` floor, or null when the
                    // strategy left it to the kernel. Null is meaningful here —
                    // it says "this opt-out still stops at the global window".
                    "gateExemptionTimingFloorSec": declared.timing_min_time_left_sec,
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
                    .map(|(name, x)| {
                        // D-31: name the remaining-time floor beside the gates, so
                        // "which opt-outs reach into the closing window" is
                        // answerable from the stats snapshot alone.
                        let mut v = serde_json::json!({ "strategy": name, "gates": x.gates() });
                        if let Some(n) = x.timing_min_time_left_sec {
                            v["timingFloorSec"] = serde_json::json!(n);
                        }
                        v
                    })
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
            "venueRejected": self.stats.venue_rejected,
            // #180: the ONE panel-visible error slot — structured (code + when +
            // what), written by every internal refusal path, and read by the
            // panel's error banner. `code` is the same `CoreErrorCode`
            // vocabulary a rejected RPC carries in `data.coreCode`, so a client
            // that knows how to branch on one knows how to branch on the other.
            "lastError": match &self.last_error {
                Some(e) => serde_json::json!({
                    "tsMs": e.ts_ms,
                    "code": e.code,
                    "message": e.message,
                }),
                None => serde_json::Value::Null,
            },
            // Legacy spelling of the same record, kept because the shipped panel
            // (ui/webapp/webui/src/pages/Overview.vue) reads this key for its
            // "last trading error" banner. Same record, one writer, one
            // timestamp — never a second, staler copy. The message is rendered
            // exactly as this key always rendered it (`<code>: <message>`), so a
            // consumer that parsed the old string keeps working; `lastError`
            // above is the structured form to migrate to.
            "lastVenueError": match &self.last_error {
                Some(e) => serde_json::json!({
                    "tsMs": e.ts_ms,
                    "message": e.legacy_message(),
                }),
                None => serde_json::Value::Null,
            },
            // E31-b: how close the reconcile sweep is to freezing trading. The
            // counter is the missing half of the last error — the error says a
            // sweep failed, the counter says how many in a row, and the threshold
            // says when the freeze lands. Read-only: nothing here sets either
            // value, and the sweep's own success still resets the streak
            // (`Core::reconcile`). Exposed because the transition itself is only
            // reachable from a market plugin, so this is the only way an outside
            // observer (panel, gate) can see the kernel's own threshold.
            "reconcile": {
                "consecutiveSweepFailures": self.consecutive_sweep_failures,
                "freezeThreshold": SWEEP_FAILURE_FREEZE,
            },
            "selfCheck": match &self.last_self_check {
                Some(r) => serde_json::json!({
                    "ok": r.ok,
                    "tsMs": r.ts_ms,
                    "items": r.items.iter().map(|i| serde_json::json!({
                        "name": i.name, "ok": i.ok, "detail": i.detail,
                    })).collect::<Vec<_>>(),
                }),
                None => serde_json::Value::Null,
            },
            "tradingFrozen": if self.risk.is_killed() {
                serde_json::json!({
                    "active": true,
                    "reason": self.risk.kill_reason().unwrap_or("kill switch active"),
                })
            } else {
                serde_json::json!({ "active": false })
            },
            // In-kernel accounting audit (issue #189): the latest verdict, so the
            // panel shows what the kernel itself concluded about the books.
            "accountingAudit": self.accounting_audit_view(),
            // #173: the day's realized-loss budget as the panel reads it —
            // visible even when healthy, so "is the breaker armed, and against
            // what cap?" is answerable without grepping the startup log.
            "dailyLoss": {
                "dayIndex": self.positions.daily_state().day_index,
                "realizedPnlUsd": self.positions.daily_pnl(),
                "limitUsd": self.positions.effective_daily_loss_limit(),
                "openingEquityUsd": self.positions.daily_state().opening_equity_usd,
                // P1 #235: WHICH book that base belongs to ("dry"/"live", null
                // when the day predates the field). The cap is only as
                // meaningful as the account it is a share of, and the panel
                // used to show a self-consistent pair of numbers from two
                // different books.
                "equityBasis": self.positions.daily_state().equity_basis,
                "tripped": self.positions.daily_loss_tripped(),
                "trippedAtMs": self.positions.daily_state().tripped_at_ms,
                // #177: protective stops the wick guard withheld — "should have
                // triggered" is a number on the panel, not a silent no-op.
                "suppressedStops": self.positions.suppressed_stop_count(),
            },
            // #205: how stale a book may be before the engine stops pricing off
            // it — the value that actually gates entries, read off the engine
            // that applies it (a backtest can configure its own). "How long
            // after the feed goes quiet does the bot stop trading?" is a runtime
            // question and must be answerable from the panel.
            "orderbookFreshness": self.orderbook_freshness_view(),
            // P0 #202: what ONE order can commit on THIS account. The ceiling is
            // read off the kernel's own knobs — no ticket can hold more than
            // `max_shares` (every leg is clamped to it) or pay over `max_price`
            // a share, and when the equity-relative cap is armed the
            // `balance × k` bound applies on top. `scripts/risk-sizing-check.mjs`
            // asserts exactly the last line of this block on a live dry core.
            "sizing": self.sizing_view(),
            // #190: the pending-close exit-reason table — how many reasons are
            // held against the bound, and what the two clean-up paths removed.
            // `orphaned` must read 0: an entry whose position is gone is the
            // ghost reason the table used to accumulate.
            "exitReasons": self.exit_reasons_view(),
            // Settlement & redemption (issue #175): settled-but-unredeemed claims
            // are money the chain still owes, and this is where the panel sees
            // them — the same outlet as everything else, no new event type.
            "settlement": self.settlement_view(as_of_ms),
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

    /// P0 #202 — "how much can any ONE order commit on this account, right
    /// now?", answered from the kernel's own knobs so the panel (and the sizing
    /// gate) never re-derive it from the source:
    ///
    /// * `shareBandUsd` = `max_shares × max_price`: the widest ticket the
    ///   engine's own clamp allows, valued at the risk gate's own price ceiling.
    ///   This is the bound that existed before #202, and on the live 4.8 USDC
    ///   account it is 10.00 USD — 208% of the book.
    /// * `sizeBudgetUsd` (#202, when `size_pct > 0`) = `equity × pct%`.
    /// * `equityCapUsd` (#202, when the relative cap is armed) = `equity × k%`,
    ///   the bound the risk gate REJECTS over.
    /// * `worstCaseOrderUsd` = the tightest of the three. Closing intents are
    ///   exempt from the equity cap by design (`closes_exposure`), and a close
    ///   is bounded by the position it reduces — never by these knobs — so the
    ///   number describes NEW exposure, which is what "max possible loss on one
    ///   order" means for a binary-market BUY (the whole notional can go to 0).
    fn sizing_view(&self) -> serde_json::Value {
        let equity = self.ledger.balance();
        let globals = self.engine.as_ref().map(|e| e.global_sizing()).unwrap_or(
            crate::engine::EffectiveSizing {
                size_usd: self.config.size_usd,
                min_shares: self.config.min_shares,
                max_shares: self.config.max_shares,
                size_pct: self.config.size_pct,
                strategy_scoped: false,
                size_weight: None,
            },
        );
        let risk = self.risk.config();
        let share_band = globals.max_shares.max(Decimal::ZERO) * risk.max_price.max(Decimal::ZERO);
        let able = equity > Decimal::ZERO;
        let size_budget = (globals.size_pct > Decimal::ZERO && able)
            .then(|| equity * globals.size_pct / Decimal::ONE_HUNDRED);
        let equity_cap = (risk.max_order_notional_pct > Decimal::ZERO && able)
            .then(|| equity * risk.max_order_notional_pct / Decimal::ONE_HUNDRED);
        let mut worst = share_band;
        if let Some(b) = size_budget {
            worst = worst.min(b);
        }
        if let Some(c) = equity_cap {
            worst = worst.min(c);
        }
        serde_json::json!({
            "equityUsd": dec_json(equity),
            "minShares": dec_json(globals.min_shares),
            "maxShares": dec_json(globals.max_shares),
            "maxPrice": dec_json(risk.max_price),
            "shareBandUsd": dec_json(share_band),
            "sizePct": dec_json(globals.size_pct),
            "sizeBudgetUsd": size_budget.map(dec_json),
            "maxOrderNotionalUsd": dec_json(risk.max_order_notional),
            "maxOrderNotionalPct": dec_json(risk.max_order_notional_pct),
            "equityCapUsd": equity_cap.map(dec_json),
            "worstCaseOrderUsd": dec_json(worst),
            "worstCasePctOfEquity": if able {
                dec_json(worst / equity * Decimal::ONE_HUNDRED)
            } else {
                serde_json::Value::Null
            },
            // The escape hatch, stated where the bounds are: a close/reduce is
            // never judged by the equity cap, so a bound that would trap a
            // position cannot be read off this block either (#174/#202).
            "closesExemptFromEquityCap": true,
            "sizePctSkippedSignals": self
                .engine
                .as_ref()
                .map(|e| e.size_pct_skip_count())
                .unwrap_or(0),
        })
    }

    /// The latest accounting audit as the panel reads it. `null` until the first
    /// audit runs (the audit interval has not elapsed, or it is disabled).
    pub fn accounting_audit_view(&self) -> serde_json::Value {
        let mut v = match self.audit.report.as_ref() {
            Some(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            None => serde_json::json!({ "ok": true, "note": "audit not run yet" }),
        };
        v["halted"] = serde_json::json!(self.audit.halted);
        v["haltReason"] = serde_json::json!(self.audit.halt_reason);
        v["runs"] = serde_json::json!(self.audit.runs);
        v["failures"] = serde_json::json!(self.audit.failures);
        v["anchored"] = serde_json::json!(self.audit.anchor.is_some());
        // Settled-but-unredeemed payout: money owed to us, part of the identity's
        // `total` but not of `balance` until the redeem transaction confirms
        // (issue #175).
        v["receivableUsd"] = dec_json(self.settlement.receivable_usd());
        v["summary"] = serde_json::json!(
            self.audit
                .report
                .as_ref()
                .map(|r| r.summary())
                .unwrap_or_default()
        );
        v
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

    /// Read-only depth view for the UI (`engine.books`, E8-c 盘口深度).
    ///
    /// Reads `Core.books` — the mirror that every ingest path updates (Node
    /// `books.snapshot` bridge and the Rust-native feed alike), and the same
    /// store the exit path values open positions from — so the view can never
    /// contradict what trading actually saw. Levels and metrics come from
    /// `OrderbookSnapshot::from_levels`, the same builder the strategy layer
    /// consumes. Purely observational: no locks beyond the core's own, no side
    /// effects. An absent round or an unknown token simply yields nothing.
    pub fn books_view(&self, max_levels: usize) -> Vec<AssetBooksView> {
        let Some(engine) = self.engine.as_ref() else {
            return Vec::new();
        };
        let now = now_ms();
        engine
            .scanner()
            .round_state(now)
            .markets
            .iter()
            .map(|m| AssetBooksView {
                asset: m.asset.clone(),
                up: Self::book_side_view(&self.books, &m.up_token_id, max_levels),
                down: Self::book_side_view(&self.books, &m.down_token_id, max_levels),
            })
            .collect()
    }

    fn book_side_view(
        books: &HashMap<TokenId, Book>,
        token_id: &str,
        max_levels: usize,
    ) -> BookSideView {
        let Some(b) = books.get(token_id) else {
            return BookSideView::empty();
        };
        if b.bids.is_empty() && b.asks.is_empty() {
            return BookSideView::empty();
        }
        let s = OrderbookSnapshot::from_levels(
            token_id.to_string(),
            b.bids.clone(),
            b.asks.clone(),
            now_ms(),
        );
        let level = |(p, q): &(Decimal, Decimal)| BookLevelView {
            price: *p,
            size: *q,
        };
        BookSideView {
            bids: s.bids.iter().take(max_levels).map(level).collect(),
            asks: s.asks.iter().take(max_levels).map(level).collect(),
            best_bid: Some(s.best_bid).filter(|p| *p > Decimal::ZERO),
            best_ask: Some(s.best_ask).filter(|p| *p < Decimal::ONE),
            mid_price: Some(s.mid_price).filter(|p| *p > Decimal::ZERO),
            obi: Some(s.obi),
            spread: Some(s.spread),
            spread_pct: Some(s.spread_pct),
        }
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
        if req.side == Side::Buy {
            // Same entry gate as `place`: a pending opening order is an opening
            // order, however it is submitted.
            self.audit_entry_gate()?;
        }
        // The #202 equity-relative cap is a percentage of the account as it is
        // AT SUBMISSION, so the gate is handed the live balance here — the same
        // number the daily-loss breaker opens its day with.
        self.risk.check_with_equity(&req, self.ledger.balance())?;
        let id = self.new_order_id();
        if req.side == Side::Buy {
            self.ledger.reserve(&id, req.price * req.size)?;
        }
        self.ome.submit(SubmitParams {
            order_id: id.clone(),
            request: req,
            submitted_at_ms: now_ms,
            // The pre-submit path cannot know the entry's escalation window;
            // `confirm_live` falls back to the default clock when it arms.
            maker_timeout_ms: 0,
        })?;
        self.emit_order(&id);
        Ok((id, OrderStatus::Pending))
    }

    /// Venue acknowledged the order (resting live). Optionally map to its venue id.
    pub fn confirm_live(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        if let Some(o) = self.ome.get(id).cloned() {
            self.ome.mark_live(id, now_ms)?;
            // Release reservation only on fill/cancel; keep it while resting.
            self.emit_order(id);
            // The venue just accepted an order from us: capability proven for
            // this instant, so the failure streak and this intent's cooldown
            // both end here.
            self.consecutive_venue_rejects = 0;
            self.place_cooldowns.remove(&o.internal_key);
            // A `maker_then_taker` order rests as GTC; the escalation clock
            // starts on the venue's acceptance (the submit path cannot arm it
            // in LIVE — the ack arrives asynchronously). Without this the
            // maker leg rests forever and the taker fallback never fires.
            if o.mode == FillPolicy::MakerThenTaker && o.escalate_at_ms.is_none() {
                let timeout = if o.maker_timeout_ms > 0 {
                    o.maker_timeout_ms
                } else {
                    self.config.default_maker_timeout_ms
                };
                self.ome.set_escalation(id, now_ms + timeout)?;
            }
        }
        Ok(())
    }

    /// Venue rejected/killed an order before acceptance: release any reservation.
    pub fn reject_live(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        self.reject_live_result(id, None, now_ms)
    }

    /// Same as [`Core::reject_live`], carrying the venue's own failure text so
    /// cooldowns, per-strategy accounting and the freeze decision all classify
    /// against the REAL reason instead of a bare "rejected".
    pub fn reject_live_result(
        &mut self,
        id: &str,
        err: Option<crate::model::CoreError>,
        now_ms: i64,
    ) -> CoreResult<()> {
        if let Some(o) = self.ome.get(id).cloned() {
            if o.side == Side::Buy {
                self.ledger.release(id);
            }
            self.ome.mark_terminal(id, OrderStatus::Rejected, now_ms)?;
            self.emit_order(id);
            let e = err.unwrap_or_else(|| {
                crate::model::CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    "venue rejected order",
                )
            });
            // A `maker_then_taker` entry whose maker leg cannot even REST
            // (post-only would cross the book) never gets a turn to escalate
            // by timer — the venue refused the resting order itself. The
            // mode's contract is to cross instead: place the taker leg now.
            // Other failure classes (balance, auth, venue down) would fail as
            // takers too, so they keep the normal backoff/retry path.
            if o.mode == FillPolicy::MakerThenTaker
                && e.code == blitzkrieg_market_api::CoreErrorCode::WouldCross
            {
                // Same contract as the timer escalation: cross at the price
                // the CURRENT book actually offers, not the rejected passive
                // limit. If no crossing depth exists even at the grid's
                // extreme, there is nothing to escalate to — the intent goes
                // to the normal rejection accounting (backoff/freeze) and a
                // later tick re-enters if the signal persists.
                let cap = if o.side == Side::Buy {
                    Decimal::new(99, 2)
                } else {
                    Decimal::new(1, 2)
                };
                if let Some((_, worst)) = self.marketable_walk(o.side, &o.token_id, o.size, cap) {
                    let req = OrderRequest {
                        token_id: o.token_id.clone(),
                        condition_id: o.condition_id.clone(),
                        side: o.side,
                        mode: FillPolicy::Taker,
                        price: worst,
                        size: o.size,
                        // Only the engine's ENTRY order is `maker_then_taker`
                        // (exits are Maker or Taker), so this escalated leg is
                        // always new exposure and rightly stays subject to the
                        // kill switch — it is not a close intent (P0 #174).
                        internal_key: format!("{id}:escalated"),
                        strategy: o.strategy.clone(),
                        asset: o.asset.clone(),
                        direction: o.direction.clone(),
                        round_slot: o.round_slot,
                    };
                    // The risk gate gets the first word here too (#261): a
                    // cross the book offers but this core may not afford is
                    // not an escalation, it is a second refusal — and taking
                    // it would spend the intent's place in the placement
                    // history on a leg that never had a chance. Falling
                    // through books the original venue rejection instead,
                    // which is the same handling as "no crossing depth".
                    match self.risk.check(&req) {
                        Ok(()) => match self.place_escalated(req, now_ms) {
                            Ok(_) => {
                                return Ok(());
                            }
                            Err(e2) => self.emit_error(e2),
                        },
                        Err(e2) => tracing::info!(
                            order = %id,
                            strategy = %req.strategy,
                            price = %worst,
                            size = %req.size,
                            error = %e2,
                            "escalation skipped after WouldCross: the book's cross is refused by the risk gate"
                        ),
                    }
                }
            }
            self.note_venue_rejection(&o.internal_key, o.strategy.clone(), &e, now_ms);
        }
        Ok(())
    }

    /// Book a venue rejection: per-strategy accounting (`ordersRejected` /
    /// `rejectionCauses`), the intent's retry cooldown with exponential
    /// backoff, the panel-visible last error, and — once the consecutive
    /// streak hits [`FREEZE_ON_REJECTS`] — the trading freeze itself.
    fn note_venue_rejection(
        &mut self,
        internal_key: &str,
        strategy: String,
        err: &crate::model::CoreError,
        now_ms: i64,
    ) {
        self.stats.venue_rejected += 1;
        self.consecutive_venue_rejects = self.consecutive_venue_rejects.saturating_add(1);
        self.note_error(err, now_ms);
        {
            let acc = self.strategy_accounting.entry(strategy).or_default();
            acc.rejected += 1;
            *acc.rejection_causes
                .entry(format!("venue.{:?}", err.code))
                .or_default() += 1;
        }
        let (attempts, _next_ok) = {
            let cd = self
                .place_cooldowns
                .entry(internal_key.to_string())
                .or_default();
            cd.attempts = cd.attempts.saturating_add(1);
            cd.last_error = err.message.clone();
            cd.next_ok_ms = now_ms + reject_backoff_ms(cd.attempts);
            (cd.attempts, cd.next_ok_ms)
        };
        tracing::info!(
            "venue rejected order: key={internal_key} attempts={} next_retry_in={}ms reason={}",
            attempts,
            reject_backoff_ms(attempts),
            err.message
        );
        if self.consecutive_venue_rejects >= FREEZE_ON_REJECTS {
            self.freeze_trading(
                format!(
                    "{} consecutive venue rejections; last: {}",
                    self.consecutive_venue_rejects, err.message
                ),
                now_ms,
            );
        } else if attempts >= MAX_PLACE_ATTEMPTS {
            // One intent keeps bouncing. Entries stop here; closing intents
            // keep retrying past the cap (unhedged exposure may not be
            // abandoned), but both deserve ONE loud panel alert.
            let (first_time, close) = {
                let cd = self
                    .place_cooldowns
                    .get_mut(internal_key)
                    .expect("cooldown entry just written");
                let first = !cd.alerted;
                cd.alerted = true;
                (first, is_close_intent(internal_key))
            };
            if first_time {
                let status = if close {
                    format!(
                        "still retrying (close intent, backoff only) after {attempts} venue rejections: {}",
                        err.message
                    )
                } else {
                    format!(
                        "abandoned after {attempts} venue rejections: {}",
                        err.message
                    )
                };
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message: format!("placement intent {internal_key} {status}"),
                });
            }
        }
    }

    /// Whether an intent is currently blocked from (re)placement: `Some` while
    /// its cooldown is running or its attempt cap is spent. Callers skip
    /// silently — the rejection was already accounted for when it happened.
    fn placement_blocked(&self, internal_key: &str, now_ms: i64) -> bool {
        self.place_cooldowns.get(internal_key).is_some_and(|c| {
            now_ms < c.next_ok_ms
                || (c.attempts >= MAX_PLACE_ATTEMPTS && !is_close_intent(internal_key))
        })
    }

    /// Freeze trading (kill switch) with a reason the panel can show. A
    /// restart or an explicit `risk.resume` clears it; the startup self-check
    /// re-probes either way. The freeze is logged at error level (#184) — a kill
    /// switch is the loudest thing this kernel does, and every path that can
    /// reach it (venue-refusal streak, failed self-check, failed sweep, audit
    /// halt) must be countable in a run log.
    ///
    /// It deliberately does NOT write the error slot: the slot keeps the ROOT
    /// CAUSE (the raw venue error that pushed a streak over the line), and
    /// `engine.stats.tradingFrozen.reason` already carries this message for the
    /// panel's freeze banner. Overwriting the slot here is what would make it
    /// show a paraphrase instead of the error an operator has to act on.
    fn freeze_trading(&mut self, reason: String, now_ms: i64) {
        if self.risk.is_killed() {
            return; // already frozen — do not re-alarm on every rejection
        }
        let _ = now_ms;
        self.risk.kill(reason.clone());
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!("trading frozen: {reason}"),
        });
        tracing::error!(reason = %reason, "trading frozen (kill switch)");
        self.emit_error_event(crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            reason,
        ));
    }

    // ── In-kernel accounting audit (issue #189) ─────────────────────────────
    // The kernel's own three-way money check, on a timer: the ledger's books
    // against themselves, against the trade records, and against the venue's
    // reported free cash. A failure alerts, persists a record and blocks NEW
    // entries — deliberately not the kill switch, which (see #174) blocks closes
    // too and would trap the position the drift is about.

    /// Assemble the audit's input from the core's own state. Cheap enough to run
    /// on the maintenance tick: a handful of sums over live orders, open
    /// positions and the reservation table.
    fn build_audit_input(&self, now_ms: i64) -> AuditInput {
        let mut open_buy_notional = Decimal::ZERO;
        let mut open_buys = 0usize;
        for o in self.ome.live_orders() {
            if o.side != Side::Buy {
                continue;
            }
            open_buys += 1;
            open_buy_notional += o.price * (o.size - o.filled_size).max(Decimal::ZERO);
        }
        // A reservation is "stray" when the order behind it is not a tracked live
        // BUY: nobody can ever release it, so it is a permanent phantom hold.
        let stray_reservations: Vec<(OrderId, Decimal)> = self
            .ledger
            .reservations_snapshot()
            .into_iter()
            .filter(|(id, _)| {
                !self
                    .ome
                    .get(id)
                    .map(|o| o.side == Side::Buy && o.status.is_live())
                    .unwrap_or(false)
            })
            .collect();
        let (realized, seen_trades, has_trade_log) = match self.trade_db.as_ref() {
            Some(db) => (
                f64_to_decimal(db.summary().total_net_pnl),
                db.summary().total_trades,
                true,
            ),
            None => (Decimal::ZERO, 0, false),
        };
        let mut spent = Decimal::ZERO;
        let mut received = Decimal::ZERO;
        for p in self.positions.open_positions() {
            spent += p.flows.entry_cost_usd + p.flows.entry_fee_usd;
            received += p.flows.proceeds_usd - p.flows.exit_fee_usd;
        }
        let open_positions = self.positions.open_positions().len();
        AuditInput {
            now_ms,
            live: self.config.mode == Mode::Live,
            identity: CashIdentity {
                balance: self.ledger.balance(),
                // Settled but not yet redeemed: owed to us, not in the wallet.
                receivable: self.settlement.receivable_usd(),
                realized,
                spent,
                received,
            },
            reserved: self.ledger.reserved(),
            ledger_balanced: self.ledger.is_balanced(),
            unfunded_reserved: self.ledger.unfunded_reserved(),
            open_buy_notional,
            open_buys,
            stray_reservations,
            venue_free: self.venue_free.map(|(_, b)| b),
            venue_free_at_ms: self.venue_free.map(|(t, _)| t),
            ledger_realigned: self.audit.last_realign,
            open_positions,
            seen_trades,
            has_trade_log,
        }
    }

    /// Run the accounting audit now and act on its verdict: alert, persist, and
    /// block/halt new entries. Returns the report (also kept for the panel).
    ///
    /// Called from `tick` on `audit_interval_sec`, and directly by tests.
    pub fn run_accounting_audit(&mut self, now_ms: i64) -> AuditReport {
        let input = self.build_audit_input(now_ms);
        let report = crate::reconcile::audit_accounting(&input, self.audit.anchor.as_ref());

        // Anchor: taken at the first audit the STRUCTURAL checks pass, then never
        // moved while anything is failing — re-anchoring on a failure would hide
        // exactly the movement the identity exists to expose.
        if self.audit.anchor.is_none() && !report.structural_failure {
            self.audit.anchor = Some(input.identity);
        }
        self.audit.runs += 1;
        if report.ok {
            self.audit.cross_source_failures = 0;
        } else {
            self.audit.failures += 1;
            if report.structural_failure {
                // Local inconsistency: no fetch race to forgive.
                self.audit.cross_source_failures = 0;
            } else {
                self.audit.cross_source_failures += 1;
            }
        }
        let cross_strike = self.audit.cross_source_failures >= 2;
        // A halt is warranted by a structural failure immediately, by a venue
        // mismatch only after it repeats (a single stale read must not stop
        // entries), and only when the operator left the brake enabled.
        let should_halt = !report.ok
            && (report.structural_failure || cross_strike)
            && self.config.audit_halt_entries;
        let was_halted = self.audit.halted;
        let mut resumed = false;

        if should_halt && !was_halted {
            self.audit.halted = true;
            self.audit.halt_reason = report.note.clone();
            let message = format!(
                "accounting audit FAILED — new entries blocked until it passes: {}",
                report.note
            );
            eprintln!("core: {message}");
            tracing::error!(drift = %report.drift_usd, "{}", message);
            // The halt is an error of the highest order (entries are blocked), so
            // it also lands in the one panel-visible slot (#180).
            self.note_error(
                &CoreError::new(CoreErrorCode::Internal, message.clone()),
                now_ms,
            );
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        } else if should_halt {
            self.audit.halt_reason = report.note.clone();
        } else if was_halted {
            self.audit.halted = false;
            self.audit.halt_reason.clear();
            resumed = true;
            let message = format!(
                "accounting audit recovered — new entries resumed ({})",
                report.note
            );
            eprintln!("core: {message}");
            tracing::info!("{message}");
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        } else if !report.ok {
            // Failing but not (yet) halting: still loud, still persisted.
            let message = format!("accounting audit drift: {}", report.note);
            eprintln!("core: {message}");
            tracing::warn!(drift = %report.drift_usd, "{}", message);
            self.emit(Event::RiskAlert {
                code: CoreErrorCode::Internal,
                message,
            });
        }

        // Persist on every failure, every halt/resume, and otherwise as a
        // heartbeat — a healthy audit is the evidence that the check RAN. The
        // very first audit is written too: a fresh process must be able to prove
        // it checked before anything went wrong.
        let heartbeat = self.audit.last_heartbeat_ms == 0
            || now_ms - self.audit.last_heartbeat_ms >= AUDIT_HEARTBEAT_MS;
        if !report.ok || should_halt || was_halted || resumed || heartbeat {
            self.audit.last_heartbeat_ms = now_ms;
            self.persist_audit_report(&report);
        }
        self.audit.report = Some(report.clone());
        self.audit.last_at_ms = now_ms;
        report
    }

    /// The latest audit verdict, for the panel (and for a caller that wants the
    /// report without running one).
    pub fn accounting_audit(&self) -> Option<&AuditReport> {
        self.audit.report.as_ref()
    }

    /// New entries are blocked by a failing accounting audit (issue #189).
    pub fn audit_blocks_entries(&self) -> bool {
        self.audit.halted
    }

    /// Why new entries are blocked (empty when they are not).
    pub fn audit_halt_reason(&self) -> &str {
        &self.audit.halt_reason
    }

    /// The gate itself: refuses a NEW entry while the audit is failing. Exits
    /// (SELL) never consult this — a drifted ledger must not be able to trap the
    /// bot in a position, which is what routing this through the kill switch did
    /// (#174).
    fn audit_entry_gate(&self) -> CoreResult<()> {
        if self.audit.halted {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                format!(
                    "accounting audit halted new entries: {}",
                    self.audit.halt_reason
                ),
            ));
        }
        Ok(())
    }

    /// Mutable ledger access, for tests that must put the books into the exact
    /// state a bug leaves behind (a stranded reservation, cash that moved with no
    /// trade) and assert the audit NOTICES it. Gated behind the `test-support`
    /// feature the crate enables for its own test targets, so no production build
    /// can reach it.
    #[cfg(feature = "test-support")]
    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }

    /// Append one audit verdict (or a late-fill record) to the audit log.
    fn persist_audit_value(&self, value: serde_json::Value) {
        let Some(path) = self.audit_log_path() else {
            return;
        };
        if let Some(dir) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{value}");
        }
    }

    fn persist_audit_report(&self, report: &AuditReport) {
        match serde_json::to_value(report) {
            Ok(v) => self.persist_audit_value(v),
            Err(e) => tracing::warn!(error = %e, "cannot serialize audit report"),
        }
    }

    fn audit_log_path(&self) -> Option<&str> {
        self.audit.audit_log_path.as_deref()
    }

    /// Trading-capability self-check result from the live venue bridge. A
    /// failed report freezes trading — the probe exercised the venue paths a
    /// live order shares (authenticated balance, reconciliation sweep), so
    /// failing them means order egress cannot work either.
    pub fn on_self_check(&mut self, report: blitzkrieg_market_api::SelfCheckReport) {
        let ok = report.ok;
        let ts = report.ts_ms;
        for item in &report.items {
            eprintln!(
                "core: self-check {}: {} ({})",
                item.name,
                if item.ok { "ok" } else { "FAIL" },
                item.detail
            );
        }
        self.last_self_check = Some(report);
        if !ok {
            // #184: name the failing probe(s) in the message, not just "failed".
            // The items are already in `engine.stats.selfCheck`, but an operator
            // reading the log needs the reason on the line itself.
            let failed = self
                .last_self_check
                .as_ref()
                .map(|r| {
                    r.items
                        .iter()
                        .filter(|i| !i.ok)
                        .map(|i| format!("{}: {}", i.name, i.detail))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let message = if failed.is_empty() {
                "trading self-check failed (venue unreachable or credentials rejected)".to_string()
            } else {
                format!("trading self-check failed: {}", failed.join("; "))
            };
            tracing::error!(detail = %message, "trading self-check failed");
            // Root cause into the one slot (code VENUE_ERROR, not the freeze's
            // own wording) — this path has no other carrier of its own.
            self.note_error(
                &crate::model::CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message.clone(),
                ),
                ts,
            );
            self.freeze_trading(message, ts);
        }
    }

    /// E31-b: the executor's reconciliation sweep failed three times in a row.
    /// The sweep is the safety net that repairs missed fills, retires ghosts
    /// and cancels orphans; a blind sweep lets the book drift silently, so
    /// trading freezes until the operator or a restart re-probes the channel.
    pub fn on_reconcile_failed(&mut self, err: crate::model::CoreError, now_ms: i64) {
        self.consecutive_sweep_failures = self.consecutive_sweep_failures.saturating_add(1);
        let streak = self.consecutive_sweep_failures;
        let message = format!("reconciliation sweep failed ({streak}): {}", err.message);
        // #184: the safety net reporting on itself is an ops-critical line, and it
        // belongs at error level from the FIRST failure — not only when the third
        // one finally freezes trading. The streak and the threshold ride along so
        // one line answers "how bad is this and what happens next".
        tracing::error!(
            streak,
            threshold = SWEEP_FAILURE_FREEZE,
            code = ?err.code,
            "{}",
            message
        );
        self.note_error(
            &crate::model::CoreError::new(err.code, message.clone()),
            now_ms,
        );
        if streak >= SWEEP_FAILURE_FREEZE {
            self.freeze_trading(
                format!(
                    "reconciliation sweep failed {streak} times in a row: {}",
                    err.message
                ),
                now_ms,
            );
        }
    }

    /// Apply an authoritative venue/user-WS fill (cumulative size) through the
    /// same idempotent ledger + OME path the dry matcher uses, then project the
    /// resulting delta onto the position book.
    pub fn ingest_fill(&mut self, fill: Fill, now_ms: i64) -> CoreResult<()> {
        let outcome = self.ome.apply_fill_report(fill, now_ms)?;
        self.report_fill_outcome(outcome, now_ms);
        Ok(())
    }

    /// Ledger/position projection plus the late-fill signal for one applied fill
    /// (issue #178). A late fill still moves size and cash; what it must NOT do is
    /// rewrite a terminal order status, and it must reach the operator.
    fn report_fill_outcome(&mut self, outcome: FillOutcome, now_ms: i64) {
        if let Some(d) = outcome.delta {
            self.apply_delta_effects(d, now_ms);
        }
        if let Some(late) = outcome.late_fill {
            self.emit_late_fill(&late, now_ms);
        }
    }

    /// Surface a fill that landed on an order which had already ended (or that
    /// reports more size than the order could hold). Returns the alert text.
    ///
    /// `ipc/schema.rs` is outside this change's territory, so this rides the
    /// existing `RiskAlert` event rather than adding a dedicated variant; the
    /// message is a greppable `late_fill …` line so an operator, the audit log
    /// and the panel all show the same fact without a new wire type.
    pub fn emit_late_fill(&mut self, late: &LateFill, now_ms: i64) -> String {
        let status = late
            .absorbed_by
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|| "live".to_string());
        let message = format!(
            "late_fill order={} trade={} status={} size={} cumulative={}/{} reported={} overfill={}",
            late.order_id,
            if late.trade_id.is_empty() {
                "-"
            } else {
                &late.trade_id
            },
            status,
            late.size,
            late.cumulative,
            late.order_size,
            late.reported_cumulative,
            late.overfill,
        );
        tracing::warn!(
            order = %late.order_id,
            trade = %late.trade_id,
            status = %status,
            size = %late.size,
            cumulative = %late.cumulative,
            order_size = %late.order_size,
            reported_cumulative = %late.reported_cumulative,
            overfill = late.overfill,
            "fill arrived for an order that had already ended (status kept, size booked)"
        );
        eprintln!("core: {message}");
        self.emit(Event::RiskAlert {
            code: if late.overfill {
                // The venue thinks more shares exist than the order asked for:
                // over-delivery, the one case that can exceed our own books.
                CoreErrorCode::RiskRejected
            } else {
                CoreErrorCode::Internal
            },
            message: message.clone(),
        });
        self.persist_audit_value(serde_json::json!({
            "atMs": now_ms,
            "event": "late_fill",
            "orderId": late.order_id,
            "tradeId": late.trade_id,
            "absorbedBy": status,
            "size": late.size.to_string(),
            "cumulative": late.cumulative.to_string(),
            "orderSize": late.order_size.to_string(),
            "reportedCumulative": late.reported_cumulative.to_string(),
            "overfill": late.overfill,
        }));
        message
    }

    /// The taker fee a LIVE charge site may use, at `price` (#234, item 4).
    ///
    /// The schedule is process-wide and set-once, and only a replay may install
    /// one other than the shipped default — `--fee-model` is refused without
    /// `--backtest`, and the backtester is the only caller of `set_fee_schedule`.
    /// That isolation used to live ENTIRELY in the CLI: the two live charge sites
    /// read whatever was installed and could not tell a replay from a live
    /// session. This makes the invariant explicit where the money is: a core that
    /// has not declared itself a replay charges the shipped schedule or the
    /// process stops.
    ///
    /// It fails CLOSED on purpose. A counterfactual fee on a real fill is a wrong
    /// number in the ledger, in every PnL derived from it and in the loss
    /// breakers that read them — silent and unrepairable after the fact — while a
    /// refused charge is loud and stops before it can book. If a live run ever
    /// legitimately needs another schedule, that is a design change to the charge
    /// path, not a flag flip.
    fn live_taker_fee_pct(&self, price: Decimal) -> Decimal {
        let schedule = crate::exit_policy::fee_schedule();
        assert!(
            self.may_charge(schedule),
            "non-replay core would charge the '{}' taker-fee schedule: --fee-model is \
             replay-only (set CoreConfig::fee_schedule_replay only in a backtest) (#234)",
            schedule.name
        );
        schedule.fee_pct(price)
    }

    /// Whether a charge under `schedule` is legal for THIS core (#234, item 4).
    /// Split out from [`Core::live_taker_fee_pct`] so the predicate can be tested
    /// without installing a schedule: the process-wide schedule is set-once, so a
    /// test cannot put a second one in place.
    fn may_charge(&self, schedule: crate::exit_policy::FeeSchedule) -> bool {
        self.config.fee_schedule_replay
            || schedule == crate::exit_policy::legacy_quadratic_schedule()
    }

    /// Ledger + position projection for a canonical fill delta. Single choke
    /// point shared by dry fills, live user-WS fills and reconciliation gaps.
    ///
    /// E17: the fee follows the fill's RESOLVED role (`d.role`), and the cash
    /// movement and the position accrual are computed from ONE fee value, so the
    /// ledger and the trade record cannot disagree about what was paid.
    ///
    /// #181: this is also where the BUY reservation is retired. Two numbers move:
    /// the CASH (the execution price that actually left) and the RESERVATION (the
    /// limit-price notional those shares had reserved up front). They differ on a
    /// price-improved fill, and then the reservation is re-synced to the order's
    /// real outstanding commitment — so a full fill, a partial fill and a
    /// terminal transition all leave it exactly right rather than leaving residue
    /// behind (which no cancel path would ever release: a Filled order never gets
    /// cancelled).
    fn apply_delta_effects(&mut self, d: FillDelta, now_ms: i64) {
        let px = d.price;
        // A maker fill pays no fee; a taker fill pays the schedule in force on the
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
                self.live_taker_fee_pct(px)
            };
            fee_usd = (fee_pct / Decimal::ONE_HUNDRED) * notional;
            match d.side {
                Side::Buy => {
                    // Release the notional reserved at the LIMIT price for these
                    // shares, and pay the execution price in cash.
                    self.ledger
                        .settle_buy_fill(&d.order_id, notional, d.limit_price * d.delta);
                    self.ledger.charge_fee(fee_usd);
                }
                Side::Sell => self.ledger.settle_sell_fill(notional, fee_usd),
            }
        } else {
            // Rollback/correction: revert the cash with no fee.
            match d.side {
                Side::Buy => self.ledger.settle_sell_fill(-px * d.delta, Decimal::ZERO),
                // A reversed SELL gives the cash back to the venue; no reservation
                // is involved (SELLs never reserve), so the release is zero.
                Side::Sell => {
                    self.ledger
                        .settle_buy_fill(&d.order_id, -px * d.delta, Decimal::ZERO);
                }
            }
        }
        // Re-sync the reservation to the order's true outstanding commitment: the
        // filled shares are no longer committed (zero when the order ended); a
        // BUY rollback puts them back.
        if d.side == Side::Buy {
            let target = match self.ome.get(&d.order_id) {
                Some(o) if o.status.is_live() => o.price * self.ome.remaining(&d.order_id),
                _ => Decimal::ZERO,
            };
            let before = self.ledger.sync_buy_reservation(&d.order_id, target);
            if before > Decimal::ZERO && target == Decimal::ZERO {
                tracing::debug!(
                    order = %d.order_id,
                    released = %before,
                    "buy reservation retired (order no longer has an outstanding commitment)"
                );
            }
        }
        self.project_fill_delta(&d, fee_usd, now_ms);
        self.emit_fill(d);
        // The idempotency table changed with this fill; get it on disk before the
        // next event can arrive (issue #178).
        self.persist_applied_journal();
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
                        // F4: the row BEFORE this exit fill accrues is the exact
                        // inventory a later FAILED status for this trade must
                        // restore (after apply_exit_fill the row is a zeroed
                        // shell — useless for a restore).
                        let pre_exit = self
                            .positions
                            .open_positions()
                            .iter()
                            .find(|p| p.id == id)
                            .cloned();
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
                                let reason =
                                    self.take_exit_reason(token).unwrap_or(ExitReason::Manual);
                                let was_maker = d.role.is_maker();
                                let streak_before = self.breaker.consecutive_losses(&d.strategy);
                                if let Some(closed) =
                                    self.positions.close(&id, px, reason, was_maker, now_ms)
                                {
                                    if let Some(restored) = pre_exit {
                                        self.push_close_credential(
                                            d.trade_id.clone(),
                                            CloseCredential {
                                                restored,
                                                closed: closed.clone(),
                                                streak_before,
                                            },
                                        );
                                    }
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
                        // F4: the fill this rollback reverses may have already
                        // fully closed the position and booked the close
                        // (TradeDb / strategy PnL / breaker / daily PnL). Unwind
                        // all of it; only a rollback with NO credential on file
                        // is unexplainable.
                        let consumed = self
                            .close_credentials
                            .iter()
                            .position(|(tid, _)| *tid == d.trade_id)
                            .map(|i| self.close_credentials.remove(i).unwrap().1);
                        match consumed {
                            Some(cred) => self.unwind_closed_trade(cred, d, now_ms),
                            // No position at all for the token: the reversal cannot
                            // be applied. Never swallow it.
                            None => self.emit(Event::Error {
                                error: CoreError::new(
                                    CoreErrorCode::Internal,
                                    format!(
                                        "exit rollback {} for token {token} has no open position to reverse",
                                        d.order_id
                                    ),
                                ),
                            }),
                        }
                    }
                    None => {}
                }
            }
        }
    }

    /// Breaker + event emission when a position closes.
    fn on_position_closed(&mut self, closed: &crate::position::ClosedPosition, now_ms: i64) {
        // The pending-close reason for this token died with the position it
        // belonged to (issue #190). Every close path funnels through here, and
        // an entry left behind would be inherited by the NEXT position on the
        // same token — the ghost reason that mis-attributed exits.
        self.clear_exit_reason(&closed.token_id);
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
            strategy: closed.strategy.clone(),
            token_id: closed.token_id.clone(),
            condition_id: closed.condition_id.clone(),
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
        // #173: the daily budget freezes entries the moment its cap is reached —
        // reported here (not only on the next tick) so the freeze and the loss
        // land in the same audit window.
        self.emit_daily_trip();
    }

    // ── Pending-close exit reasons (issue #190) ─────────────────────────────
    //
    // The table remembers which reason an in-flight closing SELL was submitted
    // with, so the fill that completes the close records the right attribution.
    // An entry therefore lives exactly as long as the close it belongs to, and
    // these four helpers are the only way in or out — `exit_reasons` is never
    // touched directly. Before #190 the table was only ever inserted into: an
    // exit that never filled (cancelled, replaced by a settlement, a venue pull)
    // left its reason behind for the NEXT position on that token to inherit.

    /// Record the reason an in-flight close of `token` was submitted with.
    fn note_exit_reason(&mut self, token: &str, reason: ExitReason) {
        if self
            .exit_reasons
            .insert(token.to_string(), reason)
            .is_none()
        {
            self.exit_reason_order.push_back(token.to_string());
        }
        self.enforce_exit_reason_bound();
    }

    /// Consume the pending reason for `token`: the close that fills uses it, and
    /// it must not outlive that close.
    fn take_exit_reason(&mut self, token: &str) -> Option<ExitReason> {
        let reason = self.exit_reasons.remove(token);
        if reason.is_some() {
            self.exit_reason_order.retain(|t| t != token);
        }
        reason
    }

    /// Forget the pending reason for `token` without consuming it — the close
    /// that landed carried its own reason (a settlement, a reconciled venue
    /// close), so the in-flight one is stale either way.
    fn clear_exit_reason(&mut self, token: &str) {
        if self.exit_reasons.remove(token).is_some() {
            self.exit_reason_order.retain(|t| t != token);
        }
    }

    /// Drop every entry whose token no longer has an open position.
    ///
    /// Run when the round rolls over (`engine.markets` replaces the whole round)
    /// and after a close: that is the point at which a leftover entry can no
    /// longer belong to anything live, and an entry whose position is gone would
    /// otherwise be inherited by the next position on that token.
    fn sweep_exit_reasons(&mut self) {
        if self.exit_reasons.is_empty() {
            return;
        }
        let open: HashSet<String> = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.token_id.clone())
            .collect();
        let stale: Vec<String> = self
            .exit_reasons
            .keys()
            .filter(|t| !open.contains(*t))
            .cloned()
            .collect();
        for token in stale {
            self.exit_reasons.remove(&token);
            self.exit_reasons_swept += 1;
        }
        self.repair_exit_reason_order();
    }

    /// Keep the table within [`EXIT_REASON_TABLE_MAX`], evicting what is least
    /// likely to still be needed: first the entries whose token has no open
    /// position (stale by construction), then the oldest.
    fn enforce_exit_reason_bound(&mut self) {
        if self.exit_reasons.len() <= EXIT_REASON_TABLE_MAX {
            return;
        }
        let open: HashSet<String> = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.token_id.clone())
            .collect();
        let ghosts: Vec<String> = self
            .exit_reason_order
            .iter()
            .filter(|t| !open.contains(*t))
            .cloned()
            .collect();
        for token in ghosts {
            if self.exit_reasons.len() <= EXIT_REASON_TABLE_MAX {
                break;
            }
            self.exit_reasons.remove(&token);
            self.exit_reasons_evicted += 1;
        }
        while self.exit_reasons.len() > EXIT_REASON_TABLE_MAX {
            let Some(oldest) = self.exit_reason_order.pop_front() else {
                break;
            };
            if self.exit_reasons.remove(&oldest).is_some() {
                self.exit_reasons_evicted += 1;
            }
        }
        self.repair_exit_reason_order();
    }

    /// Drop order entries whose reason is no longer in the map, so the deque
    /// always describes exactly the map's key set (the invariant the eviction
    /// path relies on to name the oldest entry).
    fn repair_exit_reason_order(&mut self) {
        self.exit_reason_order
            .retain(|t| self.exit_reasons.contains_key(t));
        debug_assert_eq!(
            self.exit_reason_order.len(),
            self.exit_reasons.len(),
            "exit_reason_order must mirror exit_reasons"
        );
    }

    /// The pending-close table as the panel reads it (issue #190): how many
    /// reasons are held, the bound, and what the two clean-up paths removed.
    /// `orphaned` counts entries whose position is gone — the ghost this table
    /// used to accumulate; it is 0 at rest.
    fn exit_reasons_view(&self) -> serde_json::Value {
        let open: HashSet<&str> = self
            .positions
            .open_positions()
            .iter()
            .map(|p| p.token_id.as_str())
            .collect();
        let orphaned = self
            .exit_reasons
            .keys()
            .filter(|t| !open.contains(t.as_str()))
            .count();
        serde_json::json!({
            "tracked": self.exit_reasons.len(),
            "capacity": EXIT_REASON_TABLE_MAX,
            "orphaned": orphaned,
            "evictedTotal": self.exit_reasons_evicted,
            "sweptTotal": self.exit_reasons_swept,
        })
    }

    /// The orderbook freshness budget as the panel reads it (issue #205). The
    /// engine's own value wins when an engine is installed (a backtest or a
    /// replay configures its own); without one the configured value is reported,
    /// which is what the next `install_engine` will apply.
    fn orderbook_freshness_view(&self) -> serde_json::Value {
        let max_stale_ms = self
            .engine
            .as_ref()
            .map(|e| e.max_orderbook_stale_ms())
            .unwrap_or(self.config.max_orderbook_stale_ms);
        serde_json::json!({
            "maxStaleMs": max_stale_ms,
            // `maxStaleMs: 0` is not "a zero budget" (which would refuse every
            // book the instant it is stamped): it is the explicit "freshness
            // check OFF". Named here so the panel never renders the two alike.
            "checkEnabled": max_stale_ms > 0,
        })
    }

    /// F4: file a close-accounting credential under a trade key. FIFO-capped —
    /// the FAILED that consumes a credential normally arrives seconds after the
    /// fill that created it, so a small window is all the pairing needs.
    fn push_close_credential(&mut self, trade_id: String, cred: CloseCredential) {
        if trade_id.is_empty() {
            return;
        }
        while self.close_credentials.len() >= 512 {
            self.close_credentials.pop_front();
        }
        self.close_credentials.push_back((trade_id, cred));
    }

    /// F4: a FAILED status arrived for a trade whose sell fill had already
    /// fully closed a position and booked the close. Reverse every ledger that
    /// close touched, in the order the close applied them:
    ///   1. position inventory — restore the exact open row (shares, basis,
    ///      flows) and hand back the daily PnL the close added;
    ///   2. TradeDb — retract the record (JSONL line dropped, summary rebuilt);
    ///   3. per-strategy accounting — counts, fees and net PnL back out;
    ///   4. consecutive-loss breaker — remove the loss / restore the streak
    ///      the win had reset, lifting a halt this close caused.
    ///
    /// Cash needs nothing here: `apply_delta_effects` already refunded it
    /// before projecting this rollback.
    fn unwind_closed_trade(&mut self, cred: CloseCredential, d: &FillDelta, now_ms: i64) {
        let closed = &cred.closed;
        let restored = self.positions.restore_closed(cred.restored, closed);
        if restored {
            self.persist_positions();
        }
        if let Some(db) = self.trade_db.as_mut() {
            let rec = crate::trade_db::TradeRecord::from_closed(closed);
            db.retract(&rec, now_ms);
        }
        {
            let entry_fee =
                (closed.entry_fee_pct / Decimal::ONE_HUNDRED) * closed.entry_price * closed.shares;
            let exit_fee =
                (closed.exit_fee_pct / Decimal::ONE_HUNDRED) * closed.exit_price * closed.shares;
            let acc = self
                .strategy_accounting
                .entry(closed.strategy.clone())
                .or_default();
            acc.closed_trades = acc.closed_trades.saturating_sub(1);
            if closed.net_pnl_usd >= Decimal::ZERO {
                acc.wins = acc.wins.saturating_sub(1);
            } else {
                acc.losses = acc.losses.saturating_sub(1);
            }
            acc.fees_usd -= entry_fee + exit_fee;
            acc.net_pnl_usd -= closed.net_pnl_usd;
        }
        self.breaker
            .retract(&closed.strategy, closed.net_pnl_usd, cred.streak_before);
        tracing::warn!(
            order_id = %d.order_id,
            token = %d.token_id,
            trade = %d.trade_id,
            position = %closed.id,
            inventory_restored = restored,
            net_pnl_reversed = %closed.net_pnl_usd,
            "FAILED trade reversed a full close: position inventory restored, close ledgers unwound"
        );
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
        // The sweep channel just proved itself alive — a failed-sweep streak
        // only counts failures in a row (E31-b).
        self.consecutive_sweep_failures = 0;
        // Ledger + position effects for any gap fills the OME just applied.
        for gap in report.actions.iter().filter_map(|a| match a {
            crate::reconcile::ReconcileAction::FilledGap { delta, .. } => Some(delta.clone()),
            _ => None,
        }) {
            self.apply_delta_effects(gap, snap.now_ms);
        }
        // Fills the sweep absorbed into an order that had already ended (#178):
        // the size is booked, the terminal status kept, and the operator told.
        for late in report.late_fills.clone() {
            self.emit_late_fill(&late, snap.now_ms);
        }
        // Trades on orders the OME never issued (manual closes made directly
        // on the venue): fold them into the position book so the position
        // cannot haunt the exit rules forever.
        self.apply_external_fills(&report.unknown_fills, snap.now_ms);
        if !report.actions.is_empty()
            || !report.suspect_ghost_ids.is_empty()
            || !report.unknown_fills.is_empty()
        {
            let filled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::FilledGap { .. }))
                .count();
            let marked_filled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedFilled { .. }))
                .count();
            let marked_cancelled = report
                .actions
                .iter()
                .filter(|a| matches!(a, crate::reconcile::ReconcileAction::MarkedCancelled { .. }))
                .count();
            // #184: a sweep that had to REPAIR something is a drift report — the
            // local book disagreed with the venue and the safety net caught it.
            // The net working is good news; the divergence is not, and an
            // operator has to be able to see it in the run log without reading
            // the panel. Only repairs and ghost suspicions are errors: external
            // fills alone are a manual close made on the venue (informational).
            if !report.actions.is_empty() || !report.suspect_ghost_ids.is_empty() {
                tracing::error!(
                    filled_gaps = filled,
                    marked_filled,
                    marked_cancelled,
                    ghosts = ?report.suspect_ghost_ids,
                    "reconciliation drift repaired: the venue disagreed with the core's book"
                );
            } else {
                tracing::info!(
                    external_fills = report.unknown_fills.len(),
                    "reconciliation: external (manual) fills folded into the position book"
                );
            }
            self.emit(Event::ReconcileReport {
                filled,
                marked_filled,
                marked_cancelled,
                ghost_ids: report.suspect_ghost_ids.clone(),
            });
        }
        Ok(report)
    }

    /// Fold external fills (venue trades on orders the OME does not track) into
    /// the position book. Only SELLs are applied: a manual close reduces the
    /// matching position by token id, settling the ledger for the cash that
    /// actually moved, and closes the position outright when it empties.
    /// Manual BUYS are skipped — cash truth comes from the periodic
    /// `venue_free_balance` realign, and inventing an unmanaged position here
    /// would fight the engine. Dedup is a persisted watermark: only fills
    /// strictly newer than the last applied one count, so a restart cannot
    /// re-apply what the sweep still reports.
    ///
    /// F2: the venue adapter only forwards legs it proved belong to our
    /// funder, and this loop stays conservative anyway — an unknown SELL is
    /// never booked beyond the inventory the token's position can actually
    /// attribute: size AND proceeds are truncated to what is held, so a
    /// mis-attributed over-sell cannot mint cash.
    fn apply_external_fills(&mut self, fills: &[crate::reconcile::VenueTrade], now_ms: i64) {
        if fills.is_empty() {
            return;
        }
        let fresh: Vec<&crate::reconcile::VenueTrade> = fills
            .iter()
            .filter(|t| t.ts_ms > self.recon_watermark_ms)
            .collect();
        if fresh.is_empty() {
            return;
        }
        let mut applied = 0usize;
        for t in &fresh {
            if t.side != Side::Sell {
                continue;
            }
            let Some((position_id, held)) = self
                .positions
                .open_positions()
                .iter()
                .find(|p| p.token_id == t.token_id)
                .map(|p| (p.id.clone(), p.shares))
            else {
                continue;
            };
            // F2: clamp the size to attributable inventory and price the cash
            // on the CLAMPED size — truncating the size while keeping the full
            // notional would still book the counterparty's money.
            let size = t.size.min(held);
            if size <= Decimal::ZERO {
                continue;
            }
            let notional = t.price * size;
            let fee_usd = if t.maker == Some(true) {
                Decimal::ZERO
            } else {
                // The fee follows the schedule in force, under the same
                // replay-only guard the fill choke point uses (#234, item 4).
                (self.live_taker_fee_pct(t.price) / Decimal::ONE_HUNDRED) * notional
            };
            self.ledger.settle_sell_fill(notional, fee_usd);
            let role = if t.maker == Some(true) {
                crate::model::OrderRole::Maker
            } else {
                crate::model::OrderRole::Taker
            };
            let remaining = self
                .positions
                .apply_exit_fill(&position_id, size, t.price, fee_usd, role)
                .unwrap_or(Decimal::ZERO);
            if remaining == Decimal::ZERO {
                // F10: a manual full close must hit the SAME close ledger the
                // engine's own fills do — TradeDb, strategy PnL, win/loss and
                // the consecutive-loss breaker. `close()` removes the position
                // from the book, so re-reporting the same trade cannot close
                // it twice: that removal IS the idempotence guard.
                if let Some(closed) = self.positions.close(
                    &position_id,
                    t.price,
                    ExitReason::Manual,
                    t.maker == Some(true),
                    now_ms,
                ) {
                    self.persist_positions();
                    self.on_position_closed(&closed, now_ms);
                }
                // Belt and braces: this path can close without the OME ever
                // issuing an order, and the pending-close reason must not
                // outlive the position it belonged to (issue #190) — the
                // reason is retired here even if no closed record came back.
                self.clear_exit_reason(&t.token_id);
            }
            self.persist_positions();
            applied += 1;
            tracing::info!(
                token = %t.token_id,
                requested_size = %t.size,
                applied_size = %size,
                price = %t.price,
                remaining = %remaining,
                "external close reconciled (manual venue sell folded into the position book)"
            );
        }
        // Buy-side external fills and sells with no matching position are
        // expected noise; one line per sweep keeps the log honest.
        let unmatched = fresh.iter().filter(|t| t.side == Side::Sell).count() - applied;
        let buys = fresh.iter().filter(|t| t.side != Side::Sell).count();
        if unmatched > 0 || buys > 0 {
            eprintln!(
                "core: external fills not matched to a position: sells={unmatched} buys={buys} (buys are reconciled via venue cash realign)"
            );
        }
        let new_mark = fresh
            .iter()
            .map(|t| t.ts_ms)
            .max()
            .unwrap_or(self.recon_watermark_ms);
        self.recon_watermark_ms = self.recon_watermark_ms.max(new_mark);
        if let Some(db) = self.position_db.as_ref() {
            db.save_watermark(self.recon_watermark_ms);
        }
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

    /// Apply a hot update of the ENTRY limits (#191) — the whole safe subset
    /// ([`crate::risk::HOT_RELOADABLE`]), in memory, with an audit record.
    ///
    /// What makes this safe enough to offer at runtime is what the whitelist
    /// selects for: the order path reads every one of these LIVE (the risk gate
    /// on each check, the engine's own config on each ticket), they bound NEW
    /// exposure only — a close is exempt by construction (#174), so tightening a
    /// limit can never trap an open position — and none of them carries
    /// accumulated state, so a change cannot retroactively decide something the
    /// day's book of events already decided.
    ///
    /// Atomic: the patch is validated in full (non-negative, coherent band)
    /// before anything is written, so a refused patch leaves NO trace. Nothing is
    /// persisted — a restart re-applies the startup flags (#191: "hot" must never
    /// be read as "permanent"), and the audit says so in the same breath.
    ///
    /// The audit is one `tracing::info!` line per changed field (`target: "risk"`,
    /// old → new + actor + instant) in the run log every deployment already
    /// writes; the same facts are returned to the caller, so the reply and the log
    /// cannot tell two different stories.
    pub fn apply_risk_limits(
        &mut self,
        patch: &crate::ipc::schema::SetRiskLimitsParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::RiskLimitUpdate> {
        use crate::ipc::schema::{RISK_LIMIT_UPDATE_NOTE, RiskLimitChange, RiskLimitUpdate};
        use crate::risk::HOT_RELOADABLE;

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "risk.setLimits needs at least one field; hot-reloadable: {}",
                    HOT_RELOADABLE.join(", ")
                ),
            ));
        }

        // Plan first, write later: a bad field must not leave a half-applied
        // patch behind.
        let risk = self.risk.config();
        let (notional, notional_pct, open_notional) = (
            risk.max_order_notional,
            risk.max_order_notional_pct,
            risk.max_open_notional_usd,
        );
        let (min_shares, max_shares) = (self.config.min_shares, self.config.max_shares);
        let mut changes: Vec<RiskLimitChange> = Vec::new();
        let mut plan =
            |field: &'static str, from: Decimal, to: Option<Decimal>| -> CoreResult<()> {
                let Some(to) = to else {
                    return Ok(());
                };
                if to < Decimal::ZERO {
                    return Err(CoreError::new(
                        CoreErrorCode::InvalidParams,
                        format!("{field} must be >= 0, got {to}; nothing was applied"),
                    ));
                }
                changes.push(RiskLimitChange { field, from, to });
                Ok(())
            };
        plan("maxOrderNotional", notional, patch.max_order_notional)?;
        plan(
            "maxOrderNotionalPct",
            notional_pct,
            patch.max_order_notional_pct,
        )?;
        plan(
            "maxOpenNotionalUsd",
            open_notional,
            patch.max_open_notional_usd,
        )?;
        plan("minShares", min_shares, patch.min_shares)?;
        plan("maxShares", max_shares, patch.max_shares)?;

        // The band is a band: `effective_sizing` caps the floor by the ceiling, so
        // an incoherent pair would silently read as something the operator did not
        // ask for. Refuse it instead.
        let (new_min, new_max) = (
            patch.min_shares.unwrap_or(min_shares),
            patch.max_shares.unwrap_or(max_shares),
        );
        if new_min > new_max {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "minShares {new_min} would exceed maxShares {new_max}; \
                     the share band must stay coherent — nothing was applied"
                ),
            ));
        }

        let band_moved = changes
            .iter()
            .any(|c| c.field == "minShares" || c.field == "maxShares");
        for c in &changes {
            match c.field {
                "maxOrderNotional" => self.risk.config_mut().max_order_notional = c.to,
                "maxOrderNotionalPct" => self.risk.config_mut().max_order_notional_pct = c.to,
                "maxOpenNotionalUsd" => self.risk.config_mut().max_open_notional_usd = c.to,
                "minShares" => self.config.min_shares = c.to,
                "maxShares" => self.config.max_shares = c.to,
                // Unreachable today (both lists live ten lines apart), and loud
                // rather than silent if that ever stops being true: an arm that
                // matched nothing would leave the reply and the audit claiming a
                // change this function never made.
                other => {
                    debug_assert!(false, "unmapped hot-reloadable field {other}");
                    tracing::error!(
                        target: "risk",
                        field = other,
                        "hot-reloadable field has no write arm; the audit record would be a lie"
                    );
                }
            }
        }
        if band_moved {
            // The engine holds the band the tickets are cut from; the CoreConfig
            // copy above is what a reader with no engine sees.
            if let Some(engine) = self.engine.as_mut() {
                engine.set_share_band(self.config.min_shares, self.config.max_shares);
            }
        }

        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                reason = patch.reason.as_deref().unwrap_or(""),
                "risk limit hot-update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                RISK_LIMIT_UPDATE_NOTE,
            );
        }
        Ok(RiskLimitUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            persisted: false,
            note: RISK_LIMIT_UPDATE_NOTE,
        })
    }
    /// The ONE writer of the panel-visible last-error slot (#180). Every
    /// internal refusal path funnels through here — directly, or via
    /// [`Core::emit_error`] — so the slot cannot be written with a message no
    /// code classifies, and cannot be skipped because one path forgot. It is
    /// also what makes the slot a *fresh* fact: each write carries the instant
    /// the failure was observed, so a reader can tell a new failure from an old
    /// banner (`tsMs` moves, `message` is the newest reason).
    fn note_error(&mut self, err: &CoreError, now_ms: i64) {
        self.last_error = Some(LastError {
            ts_ms: now_ms,
            code: err.code,
            message: err.message.clone(),
        });
    }
    /// Broadcast a structured error to Node (never swallowed) AND record it in
    /// the one panel-visible error slot.
    pub fn emit_error(&mut self, e: CoreError) {
        self.emit_error_at(e, now_ms());
    }
    /// [`Core::emit_error`] with the caller's own instant, for error paths that
    /// carry the event time (a sweep at its `ts`) rather than the time of this
    /// call.
    pub fn emit_error_at(&mut self, e: CoreError, now_ms: i64) {
        self.note_error(&e, now_ms);
        self.emit_error_event(e);
    }
    /// Emit an error event WITHOUT touching the slot, for a consequence whose
    /// cause is already recorded: the slot must keep showing the error an
    /// operator has to act on (the raw venue refusal), not the kernel's own
    /// reaction to it (the freeze). Everything else goes through
    /// [`Core::emit_error`].
    fn emit_error_event(&self, e: CoreError) {
        self.emit(Event::Error { error: e });
    }
    /// Broadcast a risk alert.
    pub fn emit_risk_alert(&self, code: CoreErrorCode, message: String) {
        self.emit(Event::RiskAlert { code, message });
    }

    /// Roll the daily-loss budget when the UTC day changes (P0 #173).
    ///
    /// The budget's base is the account's CASH EQUITY — the ledger's
    /// venue-reconciled balance (host `seed_balance`/`venue_free_balance` keep
    /// it honest in live, `dry_seed_balance` in dry). Only the kernel can read
    /// it: the percentage cap is relative to the day's opening equity, so a
    /// fixed dollar default stops being meaningless on a small book. Called
    /// once per tick, before the exit checks, so a position closed right after
    /// the boundary is already counted in the new day.
    fn roll_daily_budget(&mut self, now_ms: i64) {
        // `balance` is already gross of local reservations — adding `reserved`
        // here would double-count resting BUY commitments.
        let equity = self.ledger.balance();
        if let Some(roll) = self
            .positions
            .roll_daily(now_ms, equity, self.equity_basis())
        {
            let message = match roll.previous_day_index {
                Some(prev) => format!(
                    "daily loss budget: UTC day {prev} closed at ${} realized{} — day {} opens with equity ${}, cap ${}",
                    roll.previous_realized_pnl_usd,
                    if roll.previous_tripped {
                        " (TRIPPED: entries were frozen)"
                    } else {
                        ""
                    },
                    roll.day_index,
                    roll.opening_equity_usd,
                    roll.limit_usd
                ),
                None => format!(
                    "daily loss budget: UTC day {} opens with equity ${}, cap ${} (realized loss resets at the UTC day boundary)",
                    roll.day_index, roll.opening_equity_usd, roll.limit_usd
                ),
            };
            if roll.previous_tripped {
                tracing::warn!(day = roll.day_index, "{}", message);
            } else {
                tracing::info!(day = roll.day_index, "{}", message);
            }
            self.emit_risk_alert(CoreErrorCode::RiskRejected, message);
        }
    }

    /// Which book the equity handed to [`Self::roll_daily_budget`] came from
    /// (P1 #235): the day's percentage cap is a share of an account, and the
    /// ledger balance means a different thing in each of them.
    ///
    /// Derived from the SAME predicate `Core::new` uses to decide which book the
    /// ledger is seeded from — a locally-settling mode gets `dry_seed_balance`,
    /// live gets the venue's money — so the label cannot drift from the number
    /// it describes. A mode that settles locally is trading the simulation even
    /// when it is called read-only, and it must not be told its base came from
    /// the venue.
    fn equity_basis(&self) -> EquityBasis {
        if self.config.mode.settles_locally() {
            EquityBasis::Dry
        } else {
            EquityBasis::Live
        }
    }

    /// Announce a tripped daily-loss breaker exactly once per process (see
    /// `DailyLossState::tripped_reported`). Consumed here AND at the losing
    /// close, so the freeze is visible at the moment it happens and again after
    /// a restart restored an already-breached budget.
    fn emit_daily_trip(&mut self) {
        let Some(t) = self.positions.take_daily_trip() else {
            return;
        };
        let message = format!(
            "DAILY LOSS LIMIT REACHED: realized ${} against a ${} cap ({}% of the day's opening equity ${}) — new entries are frozen until the next UTC day (positions still exit normally)",
            t.realized_pnl_usd,
            t.limit_usd,
            self.config.positions.max_daily_loss_equity_pct,
            t.opening_equity_usd
        );
        tracing::warn!(
            day = t.day_index,
            realized = %t.realized_pnl_usd,
            limit = %t.limit_usd,
            "{}",
            message
        );
        self.emit_risk_alert(CoreErrorCode::RiskRejected, message);
    }

    /// Report every exit withheld from becoming an order (P0 #177, F6): the
    /// audit trail must show "should have triggered" instead of a silent hold.
    ///
    /// The log line names the cause instead of calling all of them a suppressed
    /// protective stop (#267): a held TRAILING stop is not a held protective
    /// stop, and a reviewer reading "protective stop suppressed" about a profit
    /// rule is being told the wrong story. The wording lives on the event
    /// (`SuppressedStopEvent::message`), so the panel and the log cannot drift.
    fn emit_suppressed_stops(&mut self) {
        for ev in self.positions.drain_suppressed_stops() {
            let message = ev.message();
            // #268 item 2: past `N x max_book_age_sec` unpriceable, this is no
            // longer a routine hold — it is a position that cannot act, and it
            // is raised to error level so it survives a warn filter. Two call
            // sites rather than a dynamic level: tracing's level is a compile-
            // time property of the callsite.
            if ev.escalated {
                tracing::error!(
                    position = %ev.position_id,
                    token = %ev.token_id,
                    cause = ?ev.cause,
                    bid = %ev.bid,
                    mid = %ev.mid,
                    stop_pct = %ev.stop_pct,
                    book_age_ms = ?ev.book_age_ms,
                    unpriceable_for_ms = ev.unpriceable_for_ms,
                    "{}",
                    message
                );
            } else {
                tracing::warn!(
                    position = %ev.position_id,
                    token = %ev.token_id,
                    cause = ?ev.cause,
                    bid = %ev.bid,
                    mid = %ev.mid,
                    stop_pct = %ev.stop_pct,
                    book_age_ms = ?ev.book_age_ms,
                    unpriceable_for_ms = ev.unpriceable_for_ms,
                    "{}",
                    message
                );
            }
            self.emit_risk_alert(CoreErrorCode::RiskRejected, message);
        }
    }

    // ── Orders ─────────────────────────────────────────────────────────────
    pub fn place(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<(OrderId, OrderStatus)> {
        let outcome = self.place_inner(req, maker_timeout_ms, now_ms, true)?;
        Ok((outcome.order_id, outcome.status))
    }

    /// [`Core::place`] with the full outcome: the id, the status AND — when the
    /// kernel itself refused the leg after accepting it (a dry/read-only taker
    /// the book cannot fill) — the structured reason for that refusal.
    ///
    /// `orders.place` answers from this (#180) so a caller is never handed a
    /// bare `REJECTED`; everything in-tree keeps the tuple shape, which is what
    /// the strategy/engine paths use.
    pub fn place_outcome(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<PlaceOutcome> {
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
        let outcome = self.place_inner(req, 0, now_ms, false)?;
        Ok((outcome.order_id, outcome.status))
    }

    /// Every refusal this kernel makes about an order funnels through here, so
    /// the one error slot (#180) is updated whichever gate said no — and so the
    /// slot can never be skipped because one path forgot to record.
    fn place_inner(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
        entry_gates: bool,
    ) -> CoreResult<PlaceOutcome> {
        let outcome = self.place_gated(req, maker_timeout_ms, now_ms, entry_gates);
        if let Err(e) = &outcome {
            self.note_error(e, now_ms);
        }
        outcome
    }

    fn place_gated(
        &mut self,
        req: OrderRequest,
        maker_timeout_ms: i64,
        now_ms: i64,
        entry_gates: bool,
    ) -> CoreResult<PlaceOutcome> {
        // An intent the venue just refused waits out its backoff locally —
        // re-POSTing at full tick rate is exactly the rejection storm this
        // guards against. Manual attempts are only paused while the backoff
        // window is running (≤ 30s), never permanently abandoned.
        if let Some(cd) = self.place_cooldowns.get(&req.internal_key)
            && now_ms < cd.next_ok_ms
        {
            return Err(crate::model::CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::RiskRejected,
                format!(
                    "intent {} in rejection cooldown ({}ms left): {}",
                    req.internal_key,
                    cd.next_ok_ms - now_ms,
                    cd.last_error
                ),
            ));
        }
        // #202: the equity-relative per-order cap is a percentage of the account
        // AT SUBMISSION, so the gate is handed the live balance rather than a
        // remembered copy of it.
        self.risk.check_with_equity(&req, self.ledger.balance())?;

        // Entry gates apply to opening BUY orders only; exits (SELL) are never
        // blocked by capacity, breaker or cooldowns.
        if entry_gates && req.side == Side::Buy {
            // A failing accounting audit blocks NEW entries (issue #189). It is
            // deliberately not the kill switch: that also blocks closes (#174), so
            // a drifted ledger would trap the bot in whatever it holds. Only the
            // opening side is refused, and only until an audit passes again.
            self.audit_entry_gate()?;
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
                // PAIR-COMPLETION exemption (hold-to-settlement strategies
                // only): the one-position-per-asset gate would reject the
                // second leg of a complete set, but UP + DOWN of the SAME
                // condition is market-neutral — the pair pays $1 at settlement
                // regardless of the outcome, so it is not a doubling of
                // exposure. The override is deliberately narrow: only the
                // specific "Already in {asset}" rejection, only for a strategy
                // that declared hold-to-settlement, only as the OPPOSITE
                // direction of a position it already holds on the SAME
                // condition. Same-direction stacking and multi-condition
                // exposure remain rejected, and every other can_open reason
                // (capacity, daily loss, cooldowns) still applies.
                let pair_completion = reason.starts_with("Already in")
                    && self
                        .engine
                        .as_ref()
                        .is_some_and(|e| e.strategy_holds_to_settlement(&req.strategy))
                    && self.positions.open_positions().iter().any(|p| {
                        p.asset == req.asset
                            && p.condition_id == req.condition_id
                            && p.strategy == req.strategy
                            && p.direction != direction
                    });
                if !pair_completion {
                    return Err(CoreError::new(CoreErrorCode::RiskRejected, reason));
                }
                tracing::info!(
                    strategy = %req.strategy,
                    asset = %req.asset,
                    direction = req.direction,
                    "pair-completion entry allowed past the one-per-asset gate (hold-to-settlement)"
                );
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
            maker_timeout_ms,
        })?;
        let rejection = self.place_after_submit(&id, maker_timeout_ms, now_ms)?;

        let status = self
            .ome
            .get(&id)
            .map(|o| o.status)
            .unwrap_or(OrderStatus::Pending);
        self.emit_order(&id);
        Ok(PlaceOutcome {
            order_id: id,
            status,
            rejection,
        })
    }

    /// The price a taker leg must state to guarantee a full fill of `size` on
    /// `token`, walked off the mirrored book (best levels first, no worse
    /// than `limit`), plus the volume-weighted price the walk would fill at.
    /// `None` when the book cannot cover the size within `limit` — exactly
    /// what a live FOK would be killed for.
    fn marketable_walk(
        &self,
        side: Side,
        token: &str,
        size: Decimal,
        limit: Decimal,
    ) -> Option<(Decimal, Decimal)> {
        self.books.get(token)?.walk_marketable(side, limit, size)
    }

    /// Why a dry/read-only taker leg could not be filled, as the structured
    /// error the submitter gets back (#180).
    ///
    /// Two different causes reach this point — nothing crosses the limit, or not
    /// enough size rests at or inside it — and they are different operational
    /// facts (a quote that moved vs a book too thin for our size), so each gets a
    /// stable leading token a log search, the panel and a gate can match on. Both
    /// carry `WOULD_CROSS`: that is exactly the code a live FOK is killed under
    /// (the venue adapter maps its "no match" / "cross" / "post-only" errors to
    /// it), and a refusal must not invent a second error model.
    fn taker_refusal(&self, order: &TrackedOrder, book: &Book) -> CoreError {
        let (crosses, depth) = book.crossing_depth(order.side, order.price);
        let best = match order.side {
            Side::Buy => book.best_ask().map(|p| format!("best ask {p}")),
            Side::Sell => book.best_bid().map(|p| format!("best bid {p}")),
        }
        .unwrap_or_else(|| {
            format!(
                "no {} rests",
                match order.side {
                    Side::Buy => "ask",
                    Side::Sell => "bid",
                }
            )
        });
        let message = match (order.side, crosses) {
            (Side::Buy, false) => format!(
                "taker_no_crossing_liquidity: no ask at or inside the {} buy limit ({best}; token {}, size {})",
                order.price, order.token_id, order.size
            ),
            (Side::Sell, false) => format!(
                "taker_no_crossing_liquidity: no bid at or inside the {} sell limit ({best}; token {}, size {})",
                order.price, order.token_id, order.size
            ),
            (Side::Buy, true) => format!(
                "taker_insufficient_depth: only {depth} of {} shares rest at or inside the {} buy limit ({best}; token {})",
                order.size, order.price, order.token_id
            ),
            (Side::Sell, true) => format!(
                "taker_insufficient_depth: only {depth} of {} shares rest at or inside the {} sell limit ({best}; token {})",
                order.size, order.price, order.token_id
            ),
        };
        CoreError::new(CoreErrorCode::WouldCross, message)
    }

    /// The dry/read-only half of submission: settle the leg locally, since there
    /// is no venue to report a fill. Returns the kernel-side refusal when the leg
    /// was rejected here, so the submitter learns WHY (#180).
    fn place_after_submit(
        &mut self,
        id: &str,
        maker_timeout_ms: i64,
        now_ms: i64,
    ) -> CoreResult<Option<CoreError>> {
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
                let mut rejection = None;
                match order.mode {
                    FillPolicy::Taker => {
                        // Fill like a live FOK: walk the opposing side's resting
                        // levels, best first, no worse than the order's own
                        // limit, and fill at the volume-weighted price. A book
                        // that does not cross or cannot cover the size is what
                        // the venue would kill the order for — reject with the
                        // same economics instead of inventing a fill at a price
                        // nobody was offering. The walk prices the VISIBLE
                        // book; the fill model's slippage dial prices the
                        // impact of our own size beyond it (identity by
                        // default → unchanged economics).
                        let book = self.books.get(&order.token_id).cloned().unwrap_or_default();
                        match book.walk_marketable(order.side, order.price, order.size) {
                            Some((vwap, _)) => {
                                let fill_price =
                                    self.config.fill_model.apply_slippage(order.side, vwap);
                                // A FOK is all-or-nothing by construction, so it
                                // is one execution of the whole size — the trade
                                // id stays the one this path has always used.
                                let trade_id = format!("{id}:{now_ms}");
                                self.authoritative_fill(
                                    id, &trade_id, order.size, fill_price, false, now_ms,
                                )?
                            }
                            None => {
                                if order.side == Side::Buy {
                                    self.ledger.release(id);
                                }
                                self.ome.mark_terminal(id, OrderStatus::Rejected, now_ms)?;
                                // #180: a refusal is a first-class fact — it goes
                                // into the one error slot, reaches Node as an
                                // Event::Error, lands in the log at error level and
                                // travels back to the submitter on the
                                // `orders.place` result. Before this, the reject
                                // branch emitted an order update and nothing else,
                                // so `orders.place` answered a bare REJECTED and
                                // `engine.stats.lastError` stayed empty.
                                let err = self.taker_refusal(&order, &book);
                                tracing::error!(
                                    order = id,
                                    token = %order.token_id,
                                    side = ?order.side,
                                    size = %order.size,
                                    limit = %order.price,
                                    reason = %err.message,
                                    "order rejected: taker leg has no fillable liquidity"
                                );
                                self.emit_error_at(err.clone(), now_ms);
                                self.emit_order(id);
                                rejection = Some(err);
                            }
                        }
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
                // `emit_order` after the rejection is recorded, so an event
                // consumer that reads stats on the update sees the reason.
                Ok(rejection)
            }
            Mode::Live => {
                // Pending until the venue adapter confirms; the async layer calls
                // mark_live / feeds user-WS fills. Implemented with the CLOB adapter.
                Ok(None)
            }
        }
    }

    /// Apply one authoritative (cumulative) fill and its ledger effect.
    ///
    /// `maker` is the role this execution actually had, which the dry matcher
    /// knows exactly: it either crossed a resting bid (`false`) or rested and was
    /// hit (`true`). Stating it keeps DRY and LIVE on the same authority — the
    /// execution itself — instead of DRY inferring from policy and LIVE reading
    /// the venue.
    ///
    /// `trade_id` is the execution's own identity, and `cumulative` is the size
    /// this identity has now reported — NOT an increment. Two executions of one
    /// order must therefore carry two different ids (a partial fill followed by
    /// a further partial fill is two trades), which is exactly how a venue
    /// reports them; re-reporting the same id is a revision of what was already
    /// booked, not a second trade (issue #178).
    fn authoritative_fill(
        &mut self,
        id: &str,
        trade_id: &str,
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
            trade_id: Some(trade_id.to_string()),
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
    ///
    /// The fill is bounded by the crossing side's DEPTH, not by the order's own
    /// size (issue #183): a maker trades only against the volume that actually
    /// reaches its limit, so an order bigger than the book leaves the rest
    /// resting — partially filled — instead of pretending the whole size
    /// traded. The depth reading is the taker walk's own
    /// ([`crate::sim::Book::marketable_depth`]), so dry cannot hold two
    /// different ideas of how much is available.
    fn try_maker_fill(&mut self, id: &str, now_ms: i64) {
        let Some(order) = self.ome.get(id).cloned() else {
            return;
        };
        if !order.status.is_live() || order.filled_size >= order.size {
            return;
        }
        let Some(book) = self.books.get(&order.token_id) else {
            return;
        };
        if !book.crosses(&order) {
            return;
        }
        let depth = book.marketable_depth(order.side, order.price);
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
        let remaining = (order.size - order.filled_size).max(Decimal::ZERO);
        let chunk = model.maker_fill_size(&order.order_id, remaining, depth);
        if chunk <= Decimal::ZERO {
            // The crossing side offers nothing to take: the order keeps resting.
            return;
        }
        // A maker still fills at its own resting limit (Node parity); only the
        // SIZE is bounded by the book. Each chunk is its own trade identity, so
        // a later chunk books on top of this one instead of revising it.
        let trade_id = format!("{id}:{now_ms}:{}", order.filled_size + chunk);
        if let Err(e) = self.authoritative_fill(id, &trade_id, chunk, order.price, true, now_ms) {
            // Through the one error slot like every other internal failure: a fill
            // the OME refuses is exactly the kind of thing an operator must see
            // (#180/#184 rather than a bare Event::Error).
            tracing::error!(order = id, error = %e, "maker fill could not be applied");
            self.emit_error_at(e, now_ms);
        }
    }

    pub fn cancel(&mut self, id: &str, now_ms: i64) -> CoreResult<()> {
        let order = self
            .ome
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, id))?;
        let was_live = order.status.is_live();
        if order.side == Side::Buy {
            self.ledger.release(id);
        }
        self.ome.mark_terminal(id, OrderStatus::Cancelled, now_ms)?;
        // The venue may still hold the order resting: hand its id to the live
        // bridge for a real cancel. Nothing to cancel when the venue never
        // acknowledged the order (no id — the venue never heard of it).
        if was_live && let Some(vid) = order.venue_order_id {
            self.queue_venue_cancel(vid);
        }
        self.emit_order(id);
        Ok(())
    }

    /// Enqueue one venue cancel (deduped, bounded so a pathological loop
    /// cannot grow it without bound). The live bridge drains it every tick.
    fn queue_venue_cancel(&mut self, venue_order_id: String) {
        if !self.pending_venue_cancels.contains(&venue_order_id) {
            self.pending_venue_cancels.push(venue_order_id);
            if self.pending_venue_cancels.len() > 256 {
                self.pending_venue_cancels.remove(0);
            }
        }
    }

    /// Drain the venue-cancel queue: once. The bridge must forward each id as
    /// a real `DELETE /order` — see [`Self::cancel`].
    pub fn take_pending_venue_cancels(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_venue_cancels)
    }

    /// E31-c: retire the RESTING remainder of a partially filled order while
    /// keeping the already-filled part in the position book. The strategy
    /// decides WHEN; the kernel guarantees the venue-side cancel rides along.
    pub fn cancel_remaining(&mut self, order_id: &str, now_ms: i64) -> CoreResult<()> {
        self.cancel(order_id, now_ms)
    }

    /// E31-c: flatten the position behind an order — sell every share the
    /// fills have accrued so far, at that position's latest valuation.
    pub fn close_filled(&mut self, order_id: &str, now_ms: i64) -> CoreResult<usize> {
        let token = self
            .ome
            .get(order_id)
            .map(|o| o.token_id.clone())
            .ok_or_else(|| CoreError::new(CoreErrorCode::UnknownOrder, order_id.to_string()))?;
        let position = self
            .positions
            .open_positions()
            .iter()
            .find(|p| p.token_id == token)
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                CoreError::new(
                    CoreErrorCode::UnknownOrder,
                    format!("no open position behind order {order_id}"),
                )
            })?;
        self.flatten(Some(&position), now_ms)
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
        for (id, token, condition, shares, strategy, asset, direction, _current) in targets {
            // Already have a live sell for this token? skip.
            if self
                .ome
                .live_for(&token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live())
            {
                continue;
            }
            // Price the exit off the CURRENT book, not the stale valuation:
            // `current_price` is a mid ≈ what the shares are worth, not what
            // a resting bid is offering, so an FOK there never crosses live.
            // The walk names the worst bid level that covers the size. With
            // no bids at all nothing can fill — the request falls back to the
            // grid floor and the settle/venue rejects it honestly.
            let price = self
                .marketable_walk(Side::Sell, &token, shares, Decimal::new(1, 2))
                .map(|(_, worst)| worst)
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(Decimal::new(1, 2));
            self.note_exit_reason(&token, ExitReason::Manual);
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

    // ── Settlement & redemption (issue #175) ────────────────────────────────
    //
    // A position that survives to its market's resolution used to sit in the
    // book forever: never closed, never paid, with its cash permanently short.
    // This section closes it at the resolution's payout price and books that
    // payout as a RECEIVABLE — never as cash. The distinction is the whole
    // design: the wallet does not hold the money until the on-chain redemption
    // confirms, and crediting `balance` early would make the ledger claim cash
    // the venue does not have, which is exactly what the live leg of the
    // accounting audit reads as divergence (and halts entries over). The
    // receivable is part of `CashIdentity::total`, so the anchored identity
    // stays true at every instant: settlement moves the payout into `total`
    // against the realized PnL of the close, redemption moves it from the
    // receivable into `balance` without changing `total`.
    //
    // Booking is idempotent on two levels, because a settlement is real money:
    // an in-memory applied set (this process) and an append-only journal whose
    // replayed lines rebuild that set (every later process).

    /// Markets the core is waiting on a verdict for, as queries for the venue.
    /// The plugin drains this in live mode; dry mode answers itself in
    /// [`Core::tick`], so the same code path is exercised without a chain.
    pub fn take_settlement_queries(
        &mut self,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::SettlementQuery> {
        self.sync_settlement_watch(now_ms);
        self.settlement.take_queries(now_ms)
    }

    /// Keep the settlement watch list in step with the open book: a market is
    /// watched once a position of ours is past its expiry, and forgotten as soon
    /// as its last position leaves (settled, or closed by an ordinary exit).
    fn sync_settlement_watch(&mut self, now_ms: i64) {
        let positions: Vec<crate::position::OpenPosition> =
            self.positions.open_positions().to_vec();
        self.settlement.track_markets(&positions, now_ms);
        let live: HashSet<&str> = positions.iter().map(|p| p.condition_id.as_str()).collect();
        for condition_id in self.settlement.tracked_condition_ids() {
            if !live.contains(condition_id.as_str()) {
                self.settlement.forget_market(&condition_id);
            }
        }
    }

    /// A market's resolution arrived from the venue (live) or was synthesized by
    /// the core (dry). Closes every position held on that market exactly once and
    /// books the payout as a receivable.
    ///
    /// Loud on anything it cannot settle: a position whose token the resolution
    /// does not price stays OPEN (inventing a payout would be worse than the
    /// drift it fixes) and the market stays watched.
    pub fn on_market_resolution(
        &mut self,
        resolution: blitzkrieg_market_api::MarketResolution,
        now_ms: i64,
    ) {
        self.settlement
            .note_answer(&resolution.condition_id, resolution.resolved, now_ms);
        if !resolution.resolved {
            return; // not resolved yet — the query re-arms on its own cadence
        }
        let mut market_positions: Vec<crate::position::OpenPosition> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.condition_id == resolution.condition_id)
            .cloned()
            .collect();
        // Paying legs first: the market's claim must exist before a worthless leg
        // joins it, because a NegRisk redemption takes the whole outcome-share
        // vector in one call (a leg booked first could not be added afterwards).
        market_positions.sort_by_key(|p| {
            std::cmp::Reverse(
                resolution
                    .payout_per_share(&p.token_id)
                    .unwrap_or(Decimal::ZERO),
            )
        });
        if market_positions.is_empty() {
            self.settlement.forget_market(&resolution.condition_id);
            return;
        }
        let mut settled = 0usize;
        let mut payout_usd = Decimal::ZERO;
        let mut unpriced: Vec<String> = Vec::new();
        for pos in &market_positions {
            let Some(booking) = booking_for(pos, &resolution) else {
                unpriced.push(format!("{} (position {})", pos.token_id, pos.id));
                continue;
            };
            // Commit BEFORE closing. The journal line is what makes the close
            // replay-safe (and `book` returns None for a key it already holds, so
            // a re-delivered resolution cannot book the same position twice).
            let Some(committed) = self.settlement.book(&booking, &resolution, now_ms) else {
                continue;
            };
            payout_usd += committed.payout_usd;
            // A redemption through the CTF pays no fee, so the settlement exit is
            // a maker close: `close` charges no exit fee and the realized net is
            // `payout − entry cost − entry fee` — exactly the movement the
            // identity's receivable records.
            let closed = self.positions.close(
                &committed.position_id,
                committed.payout_per_share,
                SETTLEMENT_EXIT_REASON,
                true,
                now_ms,
            );
            match closed {
                Some(closed) => {
                    // Trade record first, then the position snapshot: recovery
                    // needs the realized record to already exist, because the
                    // audit's expected side is built from it (see
                    // `SettlementBook::recover`).
                    self.on_position_closed(&closed, now_ms);
                    self.settlement.note_closed(&committed.key, now_ms);
                    self.persist_positions();
                    settled += 1;
                    tracing::info!(
                        position = %committed.position_id,
                        condition = %committed.condition_id,
                        shares = %committed.shares,
                        payout_per_share = %committed.payout_per_share,
                        payout_usd = %committed.payout_usd,
                        winning = committed.winning,
                        source = %resolution.source,
                        "position settled at market resolution"
                    );
                }
                None => {
                    // The booking is committed but the position was not in the
                    // book (an external close raced it). Nothing to close, and
                    // the claim stands: the tokens are ours on-chain whatever the
                    // local book says.
                    tracing::warn!(
                        position = %committed.position_id,
                        "settlement booked but the position was already closed locally"
                    );
                }
            }
        }
        if settled > 0 {
            self.persist_positions();
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::Internal,
                message: format!(
                    "settled {settled} position(s) on {} — {payout_usd} USDC receivable pending redemption",
                    resolution.condition_id
                ),
            });
        }
        if !unpriced.is_empty() {
            let message = format!(
                "settlement: resolution of {} (source {}) prices no outcome for {} — left open",
                resolution.condition_id,
                resolution.source,
                unpriced.join(", ")
            );
            tracing::error!("{message}");
            // Through the one error slot as well: a market that cannot be priced
            // is why positions sit unresolved, and that must be visible in
            // `engine.stats` without reading logs (#180).
            self.emit_error(CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::Internal,
                message,
            ));
            return; // keep the market watched: a later resolution may price it
        }
        self.settlement.forget_market(&resolution.condition_id);
    }

    /// Claims due for a redemption attempt. The plugin drains this in live mode
    /// and reports each one back through `on_redemption_result`; dispatching
    /// stamps the claim's backoff, so a result that never arrives is retried
    /// rather than forgotten.
    pub fn take_pending_redemptions(
        &mut self,
        now_ms: i64,
    ) -> Vec<blitzkrieg_market_api::RedemptionRequest> {
        self.settlement.take_redemptions(now_ms)
    }

    /// A redemption attempt came back.
    ///
    /// Success is the exact instant the payout becomes cash: the claim leaves the
    /// receivable and the same amount enters the ledger balance, so the audit's
    /// `total` is unchanged by it. Failure is loud, is retried on the book's
    /// backoff, and never blocks a close or a new entry — this path touches
    /// neither the risk gate nor the order manager.
    pub fn on_redemption_result(
        &mut self,
        result: blitzkrieg_market_api::RedemptionResult,
        now_ms: i64,
    ) {
        match result.failure {
            None => {
                let Some(payout) = self.settlement.note_confirmed(
                    &result.id,
                    result.tx_hash.clone(),
                    result.block_number,
                    now_ms,
                ) else {
                    // Unknown or already-redeemed claim: crediting here is how a
                    // phantom payout would enter the books, so it does not.
                    tracing::warn!(
                        claim = %result.id,
                        "redemption success for an unknown or already-redeemed claim — ignored"
                    );
                    return;
                };
                self.ledger.credit_redemption(payout);
                tracing::info!(
                    claim = %result.id,
                    condition = %result.condition_id,
                    payout_usd = %payout,
                    tx = result.tx_hash.as_deref().unwrap_or(""),
                    block = result.block_number.unwrap_or_default(),
                    "settled claim redeemed on-chain — payout credited to cash"
                );
                self.emit(Event::RiskAlert {
                    code: blitzkrieg_market_api::CoreErrorCode::Internal,
                    message: format!(
                        "redeemed settled position: +{payout} USDC on-chain (claim {}, tx {})",
                        result.id,
                        result.tx_hash.as_deref().unwrap_or("n/a")
                    ),
                });
            }
            Some(failure) => {
                let what = if failure.manual {
                    "manual redemption required"
                } else {
                    "will retry"
                };
                let message = format!(
                    "redemption of claim {} failed ({what}): {}",
                    result.id, failure.message
                );
                if !self.settlement.note_failure(&result.id, &failure, now_ms) {
                    tracing::error!(
                        claim = %result.id,
                        error = %failure.message,
                        "redemption failed for an unknown or already-redeemed claim"
                    );
                } else {
                    tracing::error!(
                        claim = %result.id,
                        manual = failure.manual,
                        error = %failure.message,
                        "redemption failed"
                    );
                }
                eprintln!("core: {message}");
                // Through the one slot as well: a claim whose collateral is stuck
                // is money the operator is waiting on (#180).
                self.emit_error(CoreError::new(
                    blitzkrieg_market_api::CoreErrorCode::VenueError,
                    message,
                ));
            }
        }
    }

    /// Dry mode: simulate the redemption of every due claim. The claim is
    /// confirmed with a `dry-simulated` tx hash, so the ledger move is the same
    /// one a mined transaction produces and the whole path (settle → receivable →
    /// cash) is testable with no chain at all.
    ///
    /// `config.dry_redeem_fail` (test hook, off by default) makes the first N
    /// attempts of each claim fail instead. The failure travels the production
    /// path — `on_redemption_result` → `note_failure` → backoff, or a permanent
    /// stop when it is `manual` — so a gate can drive the retry clock and the
    /// receivable with no venue to fail for it. The count is per claim (its
    /// `attempts` counter, which the dispatch stamps), so the retry that follows
    /// a backoff is attempt 2 and lands.
    fn simulate_redemptions(&mut self, now_ms: i64) {
        for request in self.settlement.take_redemptions(now_ms) {
            let attempts = self
                .settlement
                .claim(&request.id)
                .map(|c| c.attempts)
                .unwrap_or(0);
            let failure = (attempts <= self.config.dry_redeem_fail).then(|| {
                blitzkrieg_market_api::RedemptionFailure {
                    message: format!(
                        "dry-simulated redemption failure (attempt {attempts} of {} injected)",
                        self.config.dry_redeem_fail
                    ),
                    manual: self.config.dry_redeem_manual,
                }
            });
            let result = blitzkrieg_market_api::RedemptionResult {
                id: request.id.clone(),
                condition_id: request.condition_id.clone(),
                tx_hash: failure.is_none().then(|| "dry-simulated".to_string()),
                block_number: None,
                failure,
                at_ms: now_ms,
            };
            self.on_redemption_result(result, now_ms);
        }
    }

    /// Answer a settlement query locally (dry/read-only modes). The convention is
    /// the market's own last mid: the highest-valued token held wins and pays 1,
    /// the others pay 0 — and when no token is above 0.5 the market pays nobody,
    /// which understates rather than invents a payout. Documented here because it
    /// is a simulation rule, not a fact about the market: `source` says
    /// `core-dry`, so a simulated settlement can never be read as a real one.
    ///
    /// The mid comes from the mirrored BOOK when we have one, not from the
    /// position's last valuation: settlement runs before the tick's exit pass, so
    /// `current_price` can be a tick stale, and a stale mid is a wrong payout.
    fn dry_resolution(
        &self,
        query: &blitzkrieg_market_api::SettlementQuery,
        now_ms: i64,
    ) -> Option<blitzkrieg_market_api::MarketResolution> {
        let positions = self.positions.open_positions();
        let priced: Vec<(&str, Decimal)> = query
            .token_ids
            .iter()
            .map(|t| {
                let book_mid = self.books.get(t).map(|b| {
                    crate::model::OrderbookSnapshot::from_levels(
                        t.to_string(),
                        b.bids.clone(),
                        b.asks.clone(),
                        now_ms,
                    )
                    .mid_price
                });
                let mid = book_mid
                    .filter(|m| *m > Decimal::ZERO)
                    .or_else(|| {
                        positions
                            .iter()
                            .find(|p| &p.token_id == t)
                            .map(|p| p.current_price)
                    })
                    .unwrap_or(Decimal::ZERO);
                (t.as_str(), mid)
            })
            .collect();
        if priced.is_empty() {
            return None;
        }
        let (winner, best) =
            priced.iter().fold(
                ("", Decimal::ZERO),
                |acc, (t, p)| {
                    if *p > acc.1 { (*t, *p) } else { acc }
                },
            );
        let pays = best > Decimal::new(5, 1);
        let payouts = priced
            .iter()
            .map(|(t, _)| {
                let payout = if pays && *t == winner {
                    Decimal::ONE
                } else {
                    Decimal::ZERO
                };
                ((*t).to_string(), payout)
            })
            .collect();
        Some(blitzkrieg_market_api::MarketResolution {
            condition_id: query.condition_id.clone(),
            resolved: true,
            payouts,
            // A simulated market has no venue metadata to carry.
            neg_risk: false,
            source: "core-dry".to_string(),
        })
    }

    /// One settlement round, dry modes only: the core asks itself (through the
    /// same query path the venue plugin uses) and answers from the last books it
    /// saw. Live resolutions arrive from the venue instead.
    ///
    /// The simulated redemption runs at the end of the round, so dry mode
    /// exercises the whole path (settle → receivable → cash) with no chain —
    /// `on_market_resolution` itself only ever books, whatever the mode.
    fn drive_settlement(&mut self, now_ms: i64) {
        if !self.config.mode.settles_locally() {
            return;
        }
        for query in self.take_settlement_queries(now_ms) {
            if let Some(resolution) = self.dry_resolution(&query, now_ms) {
                self.on_market_resolution(resolution, now_ms);
            }
        }
        self.simulate_redemptions(now_ms);
    }

    /// Alert once when a watched market stops being answered (see
    /// [`SettlementBook::note_blind_state`]). Loud but not repeated: the panel's
    /// `blindSinceMs` is the continuous signal.
    fn alert_if_settlement_blind(&mut self, now_ms: i64) {
        let blind = self.settlement.blind_since_ms(now_ms);
        if self.settlement.note_blind_state(blind.is_some()) {
            let since = blind.unwrap_or(now_ms);
            let message = format!(
                "settlement: no market verdict since {since} — {} market(s) past expiry cannot settle until the venue answers",
                self.settlement.tracked_markets()
            );
            tracing::error!("{message}");
            // Also the one slot (#180): "settlement has been blind since X" is the
            // single most important thing an operator can know about a live book.
            self.emit_error(CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::Internal,
                message,
            ));
        }
    }

    /// Finish settlements whose close never became durable (a crash between the
    /// journal commit and the close). The booking stands — the tokens are ours
    /// on-chain — so the position is closed here exactly once, at the recorded
    /// payout price.
    ///
    /// Called from [`Core::restore_positions`], before trading resumes: the
    /// trade record has to exist for the audit's expected side to balance (see
    /// `SettlementBook::recover`), and this is the only moment the books can
    /// still be repaired.
    fn recover_settlements(&mut self) -> usize {
        let stale: Vec<crate::position::OpenPosition> = self
            .settlement
            .recover(self.positions.open_positions())
            .into_iter()
            .cloned()
            .collect();
        let mut recovered = 0usize;
        for pos in stale {
            let key = settlement_key(&pos);
            let Some(record) = self.settlement.record(&key) else {
                continue;
            };
            let (payout_per_share, at_ms) = (record.payout_per_share, record.applied_at_ms);
            let closed = self.positions.close(
                &pos.id,
                payout_per_share,
                SETTLEMENT_EXIT_REASON,
                true,
                at_ms,
            );
            if let Some(closed) = closed {
                // The trade record was lost with the crash: write it now, once,
                // and mark the close durable so no later start repeats it.
                self.on_position_closed(&closed, at_ms);
                self.settlement.note_closed(&key, at_ms);
                recovered += 1;
                tracing::warn!(
                    position = %closed.id,
                    condition = %closed.condition_id,
                    payout_per_share = %payout_per_share,
                    "recovered a settlement whose close never landed — closed at the booked payout"
                );
            }
        }
        if recovered > 0 {
            self.persist_positions();
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::Internal,
                message: format!(
                    "recovered {recovered} settled position(s) whose close was interrupted — redeemed claims unaffected"
                ),
            });
        }
        recovered
    }

    /// The settlement block of the engine stats: what is settled, what the chain
    /// still owes us, and anything stuck. Read by the panel through the existing
    /// `engine_stats` outlet (no new event type).
    fn settlement_view(&self, as_of_ms: i64) -> serde_json::Value {
        let claims: Vec<serde_json::Value> = self
            .settlement
            .claims()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "conditionId": c.condition_id,
                    "payoutUsd": dec_json(c.payout_usd),
                    "negRisk": c.neg_risk,
                    "shares": c.outcome_shares.iter().map(|s| dec_json(*s)).collect::<Vec<_>>(),
                    "positions": c.position_ids.len(),
                    "attempts": c.attempts,
                    "nextAttemptMs": c.next_attempt_ms,
                    "manual": c.manual,
                    "txHash": c.tx_hash,
                    "blockNumber": c.block_number,
                    "lastError": c.last_error,
                })
            })
            .collect();
        serde_json::json!({
            "settledPositions": self.settlement.applied_count(),
            "bookedThisSession": self.settlement.booked_this_session(),
            "receivableUsd": dec_json(self.settlement.receivable_usd()),
            "pendingRedemptions": self.settlement.pending_claim_count(),
            "retryableRedemptions": self.settlement.retryable_claim_count(),
            "manualRedemptions": self.settlement.manual_claim_count(),
            "redeemedClaims": self.settlement.redeemed_count(),
            "trackedMarkets": self.settlement.tracked_markets(),
            "blindSinceMs": self.settlement.blind_since_ms(as_of_ms),
            "lastError": match self.settlement.last_error() {
                Some((claim, message, manual)) => serde_json::json!({
                    "claim": claim, "message": message, "manual": manual,
                }),
                None => serde_json::Value::Null,
            },
            "claims": claims,
        })
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
        for outcome in pending {
            self.report_fill_outcome(outcome, now_ms);
        }

        // Settlement (issue #175): a position past its market's expiry has to
        // become cash, and only the news of the market's resolution can do it.
        // Runs before the audit so the money a settlement moves is accounted in
        // the same round it happens (the ordering is free: `drive_settlement` is
        // a no-op in live mode, where resolutions arrive from the venue).
        self.drive_settlement(now_ms);
        // A market we are watching whose verdict never arrives strands the
        // position: say so once per episode (the panel carries `blindSinceMs`
        // continuously, this is the alert).
        self.alert_if_settlement_blind(now_ms);

        // Accounting audit (issue #189): the kernel's own periodic three-way money
        // check, so drift is caught (and blocks new entries) even when nothing
        // external is polling. Cheap: sums over live orders and open positions.
        if self.config.audit_interval_sec > 0 {
            let due = self.audit.last_at_ms == 0
                || now_ms - self.audit.last_at_ms >= self.config.audit_interval_sec * 1000;
            if due {
                self.run_accounting_audit(now_ms);
            }
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

        // #173: roll the daily-loss budget at the UTC day boundary BEFORE the
        // exit checks, so a close that lands on the new day is counted in it
        // — and report the state that gates entries: the roll itself (audit
        // trail), a restored/just-tripped breaker, and #177's withheld stops.
        self.roll_daily_budget(now_ms);
        self.emit_daily_trip();

        // Evaluate exits for every open position and place a closing SELL.
        self.run_exit_checks(now_ms)?;

        // #177: a protective stop the wick guard withheld is an event, not a
        // silent no-op — review must be able to see "should have triggered".
        self.emit_suppressed_stops();

        // Escalate due maker_then_taker orders: cancel maker, cross as taker.
        let due: Vec<EscalationTarget> = self
            .ome
            .live_orders()
            .into_iter()
            .filter(|o| o.escalate_at_ms.map(|t| t <= now_ms).unwrap_or(false))
            .map(|o| {
                (
                    o.order_id.clone(),
                    o.side,
                    o.size - o.filled_size,
                    o.token_id.clone(),
                    o.condition_id.clone(),
                    o.strategy.clone(),
                    o.asset.clone(),
                    o.direction.clone(),
                    o.round_slot,
                    // The maker's own timeout, rebuilt from when it was
                    // armed: a re-armed clock waits the same interval again.
                    o.escalate_at_ms.unwrap_or(now_ms) - o.submitted_at_ms,
                )
            })
            .collect();
        for (id, side, remaining, token, condition, strategy, asset, direction, slot, timeout) in
            due
        {
            if remaining <= Decimal::ZERO {
                continue;
            }
            // Reprice the taker leg from the CURRENT book before touching the
            // maker: the resting limit is a passive price — a live FOK stated
            // there would be killed, so escalating to it just moves the
            // failure. If the book cannot cover the remaining size even at
            // the extreme of the grid, leave the maker resting and re-arm the
            // clock: no venue traffic, the entry intent stays alive.
            let cap = if side == Side::Buy {
                Decimal::new(99, 2)
            } else {
                Decimal::new(1, 2)
            };
            let Some((_, worst)) = self.marketable_walk(side, &token, remaining, cap) else {
                self.ome.set_escalation(&id, now_ms + timeout)?;
                continue;
            };
            let req = OrderRequest {
                token_id: token,
                condition_id: condition,
                side,
                mode: FillPolicy::Taker,
                price: worst,
                size: remaining,
                internal_key: format!("{id}:escalated"),
                strategy,
                asset,
                direction,
                round_slot: slot,
            };
            // The maker is about to be cancelled to make room for this leg, so
            // the risk gate gets the first word — the same gate `place` will
            // apply a moment later. `marketable_walk` only asks whether the
            // book HAS depth at the grid's extreme; it does not ask whether
            // this core may afford it. With a per-order notional cap, the
            // affordable price is `cap / size`, and a book that only offers
            // depth above it (a one-sided 0.99 ask is the everyday case) makes
            // a naive escalation cancel the maker and THEN get refused: the
            // entry is destroyed rather than deferred, and the propagated
            // error aborts the whole tick. Skipping instead keeps the resting
            // maker alive and re-arms the clock — the same semantics as the
            // no-depth branch above — while saying so once per order.
            if let Err(e) = self.risk.check(&req) {
                if self.escalation_skips.insert(id.clone()) {
                    tracing::info!(
                        order = %id,
                        strategy = %req.strategy,
                        price = %worst,
                        size = %remaining,
                        error = %e,
                        "escalation skipped: the book's cross is refused by the risk gate; maker leg kept resting"
                    );
                }
                self.ome.set_escalation(&id, now_ms + timeout)?;
                continue;
            }
            self.escalation_skips.remove(&id);
            self.cancel(&id, now_ms)?;
            // Past the gate, a placement can still fail (a reservation, a
            // venue refusal). The maker is already gone, so that entry is
            // lost — but ONE dead escalation must not abort the maintenance
            // pass: the remaining due orders still have their turn.
            if let Err(e) = self.place_escalated(req, now_ms) {
                tracing::warn!(
                    order = %id,
                    error = %e,
                    "the escalated leg was refused after the maker was cancelled"
                );
            }
        }
        // Forget the skip marks of orders that are no longer live: the set is
        // keyed by order id, and a fresh id is minted for every order.
        let ome = &self.ome;
        self.escalation_skips
            .retain(|id| ome.get(id).is_some_and(|o| o.status.is_live()));
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
        // Read the live books through a shared immutable borrow instead of
        // cloning the whole map per tick: the valuate/check_exits closure only
        // needs the few tokens behind open positions, not every mirrored book.
        let book_fn = |token: &str| {
            self.books.get(token).map(|b| {
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

        // Hold-to-settlement strategies keep their positions to expiry: selling
        // a complete-set leg before redemption destroys the pair's riskless
        // payoff ($1 per share-pair regardless of which side wins). The policy
        // ladder therefore skips them — but only where the kernel settles
        // locally (dry/read-only), where `drive_settlement` does the redemption.
        // On a LIVE kernel the ladder keeps running until venue resolutions are
        // a proven path: a position sitting unmanaged past expiry would be
        // strictly worse than one sold early. Strategy-signal intents still
        // close such positions everywhere — an explicit "get out" is honoured.
        let settlement_holders: HashSet<String> = if self.config.mode.settles_locally() {
            self.positions
                .open_positions()
                .iter()
                .map(|p| p.strategy.clone())
                .filter(|s| {
                    self.engine
                        .as_ref()
                        .is_some_and(|e| e.strategy_holds_to_settlement(s))
                })
                .collect()
        } else {
            HashSet::new()
        };
        let holds_settlement =
            |pos: &crate::position::OpenPosition| settlement_holders.contains(&pos.strategy);

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
                if holds_settlement(&pos) {
                    continue;
                }
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

        // E31-b residual backstop: a working exit can be outgrown by its
        // position (a late entry fill lands while the sell is resting), and
        // once the sell completes the residual has NO working sell and the
        // exit policy may stay silent (e.g. the profit target already passed).
        // Any position with a sell that filled after the position opened and
        // no live sell now gets an explicit re-close of the remainder.
        if self.config.auto_exits_enabled {
            for pos in self.positions.open_positions().to_vec() {
                if !has_job.insert(pos.id.clone()) {
                    continue;
                }
                let has_live_sell = self
                    .ome
                    .live_for(&pos.token_id, Side::Sell)
                    .into_iter()
                    .any(|o| o.status.is_live());
                if has_live_sell {
                    continue;
                }
                let closed_here = self.ome.all().iter().any(|o| {
                    o.side == Side::Sell
                        && o.token_id == pos.token_id
                        && o.status == OrderStatus::Filled
                        && o.updated_at_ms >= pos.entered_at_ms
                });
                if !closed_here {
                    continue;
                }
                let price = if pos.current_price > Decimal::ZERO {
                    pos.current_price
                } else {
                    Decimal::new(1, 2)
                };
                jobs.push(ExitJob {
                    position_id: pos.id.clone(),
                    token: pos.token_id.clone(),
                    condition_id: pos.condition_id.clone(),
                    price,
                    reason: ExitReason::ForceExit,
                    use_maker: false,
                    internal_key: format!("exit-residual:{}", pos.token_id),
                    strategy: pos.strategy.clone(),
                    asset: pos.asset.clone(),
                    direction: pos.direction.as_str().to_string(),
                });
            }
        }

        for job in jobs {
            let already_live = self
                .ome
                .live_for(&job.token, Side::Sell)
                .into_iter()
                .any(|o| o.status.is_live());
            if already_live {
                continue;
            }
            // The venue already refused this exit a moment ago: let the
            // backoff run instead of re-POSTing every tick (the 380+-rejection
            // storm this guards against). Silent skip — the rejection was
            // already accounted for when it happened.
            if self.placement_blocked(&job.internal_key, now_ms) {
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
            // A FOK sell must fill in FULL at or above its limit; priced at
            // the touch it dies on the first thin tick (thin books race the
            // quote). Selling LOWER only widens the marketable range — the
            // venue still fills at the bid or better, so the slip is a floor
            // for where we agree to sell, not a price we pay.
            let price = if mode == FillPolicy::Taker {
                marketable_exit_price(job.price, self.config.exit_taker_slip_ticks)
            } else {
                job.price
            };
            let order = OrderRequest {
                token_id: job.token.clone(),
                condition_id: job.condition_id,
                side: Side::Sell,
                mode,
                price,
                size,
                internal_key: job.internal_key,
                strategy: job.strategy,
                asset: job.asset,
                direction: job.direction,
                round_slot: 0,
            };
            // Record the intended exit reason so a full sell fill closes with it
            // (bounded and cleaned up by the close: issue #190).
            self.note_exit_reason(&job.token, job.reason);
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

/// Price a taker EXIT to actually fill: the limit nudged a few ticks below
/// the executable bid, floored at the venue minimum. Polymarket prices run in
/// 0.01 ticks, so a tick is 0.01.
fn marketable_exit_price(executable_bid: Decimal, slip_ticks: i64) -> Decimal {
    if executable_bid <= Decimal::ZERO {
        return executable_bid;
    }
    let slipped = executable_bid - Decimal::new(slip_ticks.max(0), 2);
    slipped.max(Decimal::new(1, 2))
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
    /// Orders the LIVE venue refused (place path), session-scoped.
    venue_rejected: u64,
}

/// Backoff bookkeeping for one placement intent. Backoff is exponential from
/// [`REJECT_BACKOFF_BASE_MS`], capped at [`REJECT_BACKOFF_MAX_MS`]; after
/// [`MAX_PLACE_ATTEMPTS`] consecutive venue refusals the intent is no longer
/// retried at all, and if the refusals are consecutive engine-wide the trading
/// freeze trips (see `note_venue_rejection`).
#[derive(Debug, Default, Clone)]
struct RejectCooldown {
    attempts: u32,
    next_ok_ms: i64,
    last_error: String,
    /// The "abandoned after N rejections" alert for this intent was emitted.
    alerted: bool,
}

/// Base spacing between retries of a venue-rejected intent.
const REJECT_BACKOFF_BASE_MS: i64 = 2_000;
/// Retry spacing ceiling.
const REJECT_BACKOFF_MAX_MS: i64 = 30_000;
/// Attempts of ONE intent after which it stops being retried until a
/// successful placement or a restart clears the cooldown. A closing intent is
/// exempt from the cap (its backoff still applies): the position it closes is
/// unhedged exposure, and walking away leaves naked residual on the venue —
/// the "position never closes" failure class.
const MAX_PLACE_ATTEMPTS: u32 = 8;
/// Consecutive venue refusals that freeze trading (kill switch).
const FREEZE_ON_REJECTS: u32 = 5;
/// Consecutive failed reconciliation sweeps that freeze trading (E31-b): a
/// blind sweep is the failure mode that lets ghosts and orphans accumulate.
const SWEEP_FAILURE_FREEZE: u32 = 3;
/// A passing accounting audit is written to the audit log this often, so the log
/// proves the check RAN and not merely that it never failed (issue #189).
const AUDIT_HEARTBEAT_MS: i64 = 600_000;

/// The trade summary keeps realized PnL as `f64` (Node-compatible on disk); the
/// audit compares it against `Decimal` cash, so the conversion is explicit here
/// rather than an `as` cast in the comparison.
fn f64_to_decimal(v: f64) -> Decimal {
    use std::str::FromStr;
    if !v.is_finite() {
        return Decimal::ZERO;
    }
    Decimal::from_str(&format!("{v:.9}")).unwrap_or(Decimal::ZERO)
}

/// A placement intent that CLOSES exposure (an automated exit, a strategy
/// close, a flatten or a residual backstop) rather than opening one.
///
/// The predicate itself lives in [`crate::risk`]: the risk gate has to grant a
/// closing intent the same exemption the retry policy does, and two copies of
/// "what counts as closing" is how the escape hatch gets locked again (P0 #174).
fn is_close_intent(internal_key: &str) -> bool {
    crate::risk::is_close_intent(internal_key)
}
fn reject_backoff_ms(attempts: u32) -> i64 {
    // 2s, 4s, 8s, 16s, 30s, 30s… — quick escape for a transient refusal,
    // bounded patience for a persistent one.
    let ms = REJECT_BACKOFF_BASE_MS.saturating_mul(1 << (attempts.saturating_sub(1)).min(4));
    ms.min(REJECT_BACKOFF_MAX_MS)
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
    } else if msg.contains("in rejection cooldown") {
        // A local backoff pause after a venue refusal — the placement was
        // NOT re-attempted at all, which is a different bucket from a
        // genuine refusal (and must not inflate it).
        "venue.cooldown".to_string()
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

/// 盘口深度 view (E8-c): one round asset with both token books, as the UI depth
/// chart renders them. Levels are best-first and capped per side by the caller.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBooksView {
    pub asset: String,
    pub up: BookSideView,
    pub down: BookSideView,
}

/// One token's live book. Every metric is `null` when no book has arrived for
/// the token — never a zero stand-in, which the panel must not read as a real
/// quote (from_levels' empty-book sentinels stay internal to the strategy
/// layer, they are not observables).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSideView {
    pub bids: Vec<BookLevelView>,
    pub asks: Vec<BookLevelView>,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub mid_price: Option<Decimal>,
    /// Order-book imbalance (bid_depth − ask_depth) / (bid_depth + ask_depth).
    pub obi: Option<Decimal>,
    pub spread: Option<Decimal>,
    pub spread_pct: Option<Decimal>,
}

impl BookSideView {
    fn empty() -> Self {
        Self {
            bids: Vec::new(),
            asks: Vec::new(),
            best_bid: None,
            best_ask: None,
            mid_price: None,
            obi: None,
            spread: None,
            spread_pct: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookLevelView {
    pub price: Decimal,
    pub size: Decimal,
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
        // A FOK taker fills against the resting ASK (0.45), so the limit must
        // reach the touch.
        let req = crate::model::OrderRequest {
            token_id: "tok".into(),
            condition_id: "c".into(),
            side: crate::model::Side::Buy,
            mode: crate::model::FillPolicy::Taker,
            price: dec!(0.45),
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
        // Ask 0.40 crosses the buy limit with enough depth: an honest FOK
        // fills at the walked price, opening the position under test.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(500))], now_ms);
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

    /// A private evolution audit dir for one test, returned as the tuning block
    /// that points the manager at it.
    ///
    /// The runtime switches live in `<audit_dir>/state.json` and WIN over the
    /// config (#249), so a test that left them on the default `data/evolution`
    /// would hand its switches to the next test to run — and inherit whatever
    /// that one wrote. Each caller gets its own directory and its own switches,
    /// and removes it at the end.
    pub(super) fn scratch(tag: &str) -> (Option<ShadowEvolutionTuning>, String) {
        let dir = std::env::temp_dir()
            .join(format!("bkse-{}-{tag}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        (
            Some(ShadowEvolutionTuning {
                audit_dir: Some(dir.clone()),
                ..Default::default()
            }),
            dir,
        )
    }

    /// One strategy's override bag, built the way the IPC handler builds it.
    fn bag(strategy: &str, knob: &str, value: Decimal) -> MutableParams {
        let mut p = StrategyParams::new();
        p.set(knob, value);
        let mut m = MutableParams::new();
        m.set_strategy(strategy, p);
        m
    }

    /// The `spread_arb` cap currently in force, read through the strategy's own
    /// config view (the same surface an external cdylib reports through
    /// `bk_strategy_config_view`) — the kernel no longer knows the field name.
    fn cap(c: &Core) -> Decimal {
        let views = c.engine.as_ref().unwrap().strategy_config_views();
        let (_, json) = views
            .iter()
            .find(|(n, _)| n == "spread_arb")
            .expect("test adapter reports a config view");
        serde_json::from_str::<serde_json::Value>(json)
            .unwrap()
            .get("trendMaxEntryPrice")
            .and_then(|v| v.as_str())
            .and_then(|s| Decimal::from_str_exact(s).ok())
            .expect("trendMaxEntryPrice in the view")
    }

    /// A fresh engine with the three test adapters hosted (the kernel ships no
    /// strategies, PR-B) — the shape the evolution tests need: one evolvable
    /// strategy per declared knob set, registered before `shadow_evolution_enable`.
    fn se_engine() -> crate::engine::Engine {
        let mut e = crate::engine::Engine::new(crate::engine::EngineConfig::default());
        crate::strategies::test_support::host(&mut e, Default::default(), Default::default());
        e
    }

    /// Acceptance: enabling Shadow Evolution attaches the per-strategy hot-swap
    /// registry to the engine, and a parameter change is visible to the engine on
    /// the next read (no restart). Also: enabling is opt-in, apply is per-strategy
    /// and rollback restores the prior set for that strategy only.
    #[test]
    fn hot_swap_reaches_the_engine_and_rolls_back() {
        let (tuning, dir) = scratch("hotswap");
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig::default(),
            dry_seed_balance: dec!(1000),
            shadow_evolution_enabled: false,
            shadow_evolution_tuning: tuning,
            ..Default::default()
        });
        c.enable_engine(se_engine());
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #250: every variant born by the enable path carries the clock the caller
    /// passed. `age_sec` is what the evaluator's observation floor
    /// (`min_observation_secs`) measures against, so a birth stamp of 0 does not
    /// merely mislabel a timestamp — against a wall-clock `now` the variant
    /// reports an age of decades, and the floor stops gating anything: a twin
    /// seconds old would qualify the moment its trade count allowed it.
    ///
    /// The placeholder lived in the re-registration the enable path triggers (the
    /// hot-param rewire), which is why this asserts on the whole path rather than
    /// on `scaffold` alone.
    #[test]
    fn enabling_stamps_variant_birth_with_the_clock_it_was_given() {
        let (tuning, dir) = scratch("birth-stamp");
        let mut c = Core::new(CoreConfig {
            dry_seed_balance: dec!(1000),
            shadow_evolution_enabled: false,
            shadow_evolution_tuning: tuning,
            ..Default::default()
        });
        c.enable_engine(se_engine());
        // A wall-clock-shaped instant, not a small synthetic one: the defect this
        // guards against is only visible against a realistic clock.
        let now = 1_790_003_600_000;
        c.shadow_evolution_enable(now);

        let views = c.shadow_evolution_variants(now);
        assert_eq!(
            views.len(),
            9,
            "3 evolvable strategies × (baseline + 2 single-knob variants)"
        );
        for v in &views {
            assert_eq!(
                v.age_sec, 0,
                "{} / {} was stamped with a placeholder instead of the enable clock {now}",
                v.strategy, v.id
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Apply names exactly one strategy: a multi-strategy bag is refused rather
    /// than partially applied, and an unknown strategy is an error (not a silent
    /// no-op that another strategy's change could be mistaken for).
    #[test]
    fn apply_and_rollback_are_per_strategy() {
        let (tuning, dir) = scratch("per-strategy");
        let mut c = Core::new(CoreConfig {
            dry_seed_balance: dec!(1000),
            shadow_evolution_tuning: tuning,
            ..Default::default()
        });
        c.enable_engine(se_engine());
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_by_default_is_fully_inert() {
        let (tuning, dir) = scratch("inert");
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig::default(),
            shadow_evolution_tuning: tuning,
            ..Default::default()
        });
        c.enable_engine(se_engine());
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
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn spread_arb_entry_overrides_flow_through_engine_config() {
        // Defaults must stay byte-for-byte the shipped configuration: None
        // overrides leave every SpreadArbConfig field at its crate default.
        let default_cfg = CoreConfig::default().engine_config().spread_arb;
        assert_eq!(default_cfg, crate::signal::SpreadArbConfig::default());
        // An override flows through the one mapping that drives live and
        // backtest alike (E15: a sweep is a pure config variation).
        let cfg = CoreConfig {
            trend_confirm_sec: 90,
            spread_arb_trend_entry_factor: Some(dec!(0.8)),
            spread_arb_entry_min_obi: Some(dec!(-0.1)),
            spread_arb_entry_max_spread_pct: Some(dec!(2.5)),
            spread_arb_entry_dip_max_pct: Some(dec!(6)),
            spread_arb_entry_bounce_min_pct: Some(dec!(0.4)),
            spread_arb_entry_bounce_window_sec: Some(7),
            ..Default::default()
        }
        .engine_config()
        .spread_arb;
        assert_eq!(cfg.trend_confirm_sec, 90);
        assert_eq!(cfg.trend_entry_factor, dec!(0.8));
        assert_eq!(cfg.entry_min_obi, dec!(-0.1));
        assert_eq!(cfg.entry_max_spread_pct, dec!(2.5));
        assert_eq!(cfg.entry_dip_max_pct, dec!(6));
        assert_eq!(cfg.entry_bounce_min_pct, dec!(0.4));
        assert_eq!(cfg.entry_bounce_window_sec, 7);
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

    /// P1 #235: the day's percentage cap is a share of a BOOK, and which book the
    /// kernel hands the position manager is decided by the same predicate that
    /// seeded the ledger — so the label cannot drift from the number.
    #[test]
    fn the_daily_budget_basis_follows_the_mode() {
        assert_eq!(dry_core(dec!(100)).equity_basis(), EquityBasis::Dry);
        assert_eq!(live_core(dec!(100)).equity_basis(), EquityBasis::Live);
        // Read-only settles locally, so its money is the simulation's too.
        let read_only = Core::new(CoreConfig {
            mode: Mode::ReadOnly,
            ..Default::default()
        });
        assert_eq!(read_only.equity_basis(), EquityBasis::Dry);
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
        // A live FOK fills only against resting depth at or inside its limit:
        // mirror an ask the order can actually take.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
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
    fn taker_not_crossing_the_book_is_rejected() {
        let mut c = dry_core(dec!(10));
        // Best ask 0.50 is outside a 0.40 buy limit — a live FOK dies.
        c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 1);
        let out = c
            .place_outcome(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
            .unwrap();
        assert_eq!(out.status, OrderStatus::Rejected);
        // #180: the refusal names its cause, in the code a live FOK dies under.
        let err = out
            .rejection
            .expect("a kernel refusal must carry its reason");
        assert_eq!(err.code, CoreErrorCode::WouldCross);
        assert!(
            err.message.starts_with("taker_no_crossing_liquidity:"),
            "want the no-crossing cause, got {:?}",
            err.message
        );
        assert!(
            err.message.contains("0.50"),
            "names the best ask: {}",
            err.message
        );
        // And the same reason is the one the panel/`engine.stats` reader sees.
        let stats = c.engine_stats();
        assert_eq!(stats["lastError"]["code"], "WOULD_CROSS");
        assert_eq!(stats["lastError"]["message"], err.message);
        assert_eq!(stats["lastError"]["tsMs"], 1);
        assert_eq!(c.ledger().balance(), dec!(10));
        assert_eq!(c.ledger().reserved(), dec!(0));
        assert!(c.positions().open_positions().is_empty());
    }

    #[test]
    fn taker_insufficient_depth_is_rejected() {
        let mut c = dry_core(dec!(10));
        // Ask 0.40 crosses, but only 3 shares rest — an all-or-nothing FOK
        // cannot take 5, so the whole order is rejected.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(3))], 1);
        let out = c
            .place_outcome(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
            .unwrap();
        assert_eq!(out.status, OrderStatus::Rejected);
        let err = out
            .rejection
            .expect("a kernel refusal must carry its reason");
        assert_eq!(err.code, CoreErrorCode::WouldCross);
        // A DIFFERENT cause from the non-crossing case, and the message says
        // which one: a quote that moved is not a book too thin for our size.
        assert!(
            err.message.starts_with("taker_insufficient_depth:"),
            "want the depth cause, got {:?}",
            err.message
        );
        assert!(
            err.message.contains("only 3 of 5 shares"),
            "names the shortfall: {}",
            err.message
        );
        let stats = c.engine_stats();
        assert_eq!(stats["lastError"]["code"], "WOULD_CROSS");
        assert_eq!(stats["lastError"]["message"], err.message);
        assert_eq!(c.ledger().balance(), dec!(10));
        assert_eq!(c.ledger().reserved(), dec!(0));
        assert!(c.positions().open_positions().is_empty());
    }

    #[test]
    fn a_risk_rejection_reaches_the_error_slot_with_its_own_reason() {
        // The third refusal kind a caller can hit (the observability gate drives
        // all three): the per-order notional cap. It reaches the caller as a
        // structured Err — and, since #180, it also refreshes the one error slot,
        // so the panel's answer to "why did nothing get placed?" is the same
        // reason the caller got, not a stale banner.
        let mut c = dry_core(dec!(10));
        c.risk_config_mut().max_order_notional = dec!(1);
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
        let err = c
            .place_outcome(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
            .expect_err("2.00 notional must be refused by the 1 USD per-order cap");
        assert_eq!(err.code, CoreErrorCode::RiskRejected);
        assert!(
            err.message.contains("exceeds per-order cap"),
            "names the cap that refused it: {}",
            err.message
        );
        let stats = c.engine_stats();
        assert_eq!(stats["lastError"]["code"], "RISK_REJECTED");
        assert_eq!(stats["lastError"]["message"], err.message);
        assert_eq!(stats["lastError"]["tsMs"], 1);
        // Nothing reached the OME, and the reason is NOT one of the two liquidity
        // causes — the three refusal kinds stay distinguishable.
        assert!(c.list_orders().is_empty());
        assert!(
            !err.message.contains("taker_no_crossing_liquidity")
                && !err.message.contains("taker_insufficient_depth"),
            "a risk refusal must not read like a liquidity one: {}",
            err.message
        );
    }

    #[test]
    fn a_filled_taker_leaves_no_error_and_a_maker_leg_reports_none() {
        // The other direction: the slot must stay EMPTY on the happy path (an
        // error slot that is always populated tells an operator nothing).
        let mut c = dry_core(dec!(10));
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
        let out = c
            .place_outcome(order(FillPolicy::Taker, dec!(0.4), dec!(5), "k1"), 0, 1)
            .unwrap();
        assert_eq!(out.status, OrderStatus::Filled);
        assert!(out.rejection.is_none());
        assert_eq!(c.engine_stats()["lastError"], serde_json::Value::Null);

        // A resting maker is not a refusal either — it may fill later.
        let mut c = dry_core(dec!(10));
        let out = c
            .place_outcome(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k2"), 0, 1)
            .unwrap();
        assert_eq!(out.status, OrderStatus::Live);
        assert!(out.rejection.is_none());
        assert_eq!(c.engine_stats()["lastError"], serde_json::Value::Null);
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
        // At timeout: maker cancelled, taker order crosses immediately — at
        // the price the book is actually offering (0.50), not the rejected
        // passive limit.
        c.tick(1002).unwrap();
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
        // A new escalated taker order exists and is filled at the ask.
        let filled: Vec<_> = c
            .ome()
            .all()
            .into_iter()
            .filter(|o| o.status == OrderStatus::Filled)
            .collect();
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].filled_size, dec!(5));
        assert_eq!(filled[0].price, dec!(0.50));
        assert_eq!(filled[0].avg_fill_price, Some(dec!(0.50)));
        // 10 - 0.5*5 = 7.5, minus the taker entry fee
        // (0.125 × (0.5·0.5)² × 5 = 0.0390625 — the E17 schedule, price-dependent).
        assert_eq!(c.ledger().balance(), dec!(7.4609375));
    }

    #[test]
    fn escalation_without_crossing_depth_rearms_and_keeps_maker() {
        let mut c = dry_core(dec!(10));
        let (id, st) = c
            .place(
                order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
                1000,
                1,
            )
            .unwrap();
        assert_eq!(st, OrderStatus::Live);
        // No asks rest at all at the deadline: a taker leg has nothing to
        // cross into, so the maker stays and the clock re-arms.
        c.tick(1002).unwrap();
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);
        // Re-armed one maker timeout later (1001 − 1 = 1000 from now).
        assert_eq!(c.ome().get(&id).unwrap().escalate_at_ms, Some(2002));
        // No escalated order was spawned on the empty book.
        assert_eq!(
            c.ome()
                .all()
                .into_iter()
                .filter(|o| o.internal_key.ends_with(":escalated"))
                .count(),
            0
        );
        // Once the book does cross, the next due tick escalates and fills
        // at the ask.
        c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2003);
        c.tick(2003).unwrap();
        let filled: Vec<_> = c
            .ome()
            .all()
            .into_iter()
            .filter(|o| o.status == OrderStatus::Filled)
            .collect();
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].avg_fill_price, Some(dec!(0.50)));
    }

    /// #261: an escalated leg the risk gate would refuse must not cost the
    /// resting maker its life.
    ///
    /// Before this fix the sweep cancelled the maker first and escalated
    /// second, so a book that only offers depth above `cap / size` (3.00 / 5
    /// shares = 0.60 here; a live 10-share lot under a 6.00 cap gives 0.60 the
    /// same way) destroyed the entry outright AND failed the whole tick. On the
    /// deployed dry stack, whose books offer a single 0.99 ask, that was every
    /// spread_arb entry for a day and a half — zero fills, 15 aborted ticks.
    #[test]
    fn unaffordable_cross_keeps_the_maker_and_does_not_fail_the_tick() {
        let mut c = dry_core(dec!(10));
        let (id, st) = c
            .place(
                order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
                1000,
                1,
            )
            .unwrap();
        assert_eq!(st, OrderStatus::Live);
        // The only ask rests at 0.99: crossing 5 shares there costs 4.95, past
        // the 3.00 per-order cap, so the gate refuses the taker leg.
        c.book_snapshot("tok", vec![], vec![(dec!(0.99), dec!(100))], 2);
        c.tick(1002)
            .expect("an unaffordable cross must not abort the maintenance pass");
        let o = c.ome().get(&id).cloned().unwrap();
        assert_eq!(o.status, OrderStatus::Live, "the maker must stay resting");
        assert_eq!(o.escalate_at_ms, Some(2002), "the clock re-arms");
        assert_eq!(
            c.ome()
                .all()
                .into_iter()
                .filter(|o| o.internal_key.ends_with(":escalated"))
                .count(),
            0,
            "no escalated leg may be placed"
        );
        // The entry intent survived: once the book is affordable again, the
        // next due tick escalates and fills as a taker.
        c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2003);
        c.tick(2003).unwrap();
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
        let filled: Vec<_> = c
            .ome()
            .all()
            .into_iter()
            .filter(|o| o.status == OrderStatus::Filled)
            .collect();
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].avg_fill_price, Some(dec!(0.50)));
    }

    /// The second half of #261: once the gate has passed, a placement can still
    /// fail (here the reservation: the maker's own 2.00 was all the cash, and
    /// the escalated 5 × 0.50 = 2.50 has nothing to reserve). The maker is
    /// genuinely gone by then, but ONE dead escalation must not abort the
    /// maintenance pass — the remaining due orders still have their turn.
    #[test]
    fn an_escalation_that_dies_after_the_maker_does_not_abort_the_pass() {
        let mut c = dry_core(dec!(2));
        let (id, st) = c
            .place(
                order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
                1000,
                1,
            )
            .unwrap();
        assert_eq!(st, OrderStatus::Live);
        c.book_snapshot("tok", vec![], vec![(dec!(0.50), dec!(100))], 2);
        c.tick(1002)
            .expect("a failed escalation must not abort the maintenance pass");
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Cancelled);
        assert_eq!(
            c.ome()
                .all()
                .into_iter()
                .filter(|o| o.status == OrderStatus::Filled)
                .count(),
            0,
            "nothing may fill: the escalation was refused"
        );
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

    fn live_core(balance: Decimal) -> Core {
        let mut c = Core::new(CoreConfig {
            mode: Mode::Live,
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

    fn fill(order_id: &str, size: Decimal, price: Decimal, maker: Option<bool>) -> Fill {
        Fill {
            order_id: order_id.into(),
            trade_id: Some("t1".into()),
            token_id: "tok".into(),
            side: Side::Buy,
            price,
            size,
            status: FillStatus::Confirmed,
            ts_ms: 10,
            tx_hash: None,
            maker,
        }
    }

    #[test]
    fn cancel_of_a_venue_bound_order_queues_a_real_venue_cancel() {
        let mut c = live_core(dec!(10));
        // E31-a: the venue has not acked yet (no id) — nothing to forward.
        let (id, st) = c
            .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
            .unwrap();
        assert_eq!(st, OrderStatus::Pending);
        c.cancel(&id, 2).unwrap();
        assert!(c.take_pending_venue_cancels().is_empty());

        // The venue accepts: id bound, order live, ledger still reserved.
        let (id, _) = c
            .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k2"), 0, 3)
            .unwrap();
        c.bind_venue(&id, "0xv1".into(), 4).unwrap();
        assert_eq!(c.ome().get(&id).unwrap().status, OrderStatus::Live);

        c.cancel(&id, 5).unwrap();
        // A local-only retire would leave the venue resting: the id rides the
        // cancel queue for the live bridge to DELETE for real.
        assert_eq!(c.take_pending_venue_cancels(), vec!["0xv1".to_string()]);
        assert!(c.take_pending_venue_cancels().is_empty());
    }

    #[test]
    fn confirm_live_arms_escalation_from_the_bound_maker_timeout() {
        let mut c = live_core(dec!(10));
        let (id, _) = c
            .place(
                order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k1"),
                5_000,
                1,
            )
            .unwrap();
        assert!(c.ome().get(&id).unwrap().escalate_at_ms.is_none());
        c.confirm_live(&id, 100).unwrap();
        assert_eq!(c.ome().get(&id).unwrap().escalate_at_ms, Some(5_100));

        // Zero timeout falls back to the configured default window.
        let (id2, _) = c
            .place(
                order(FillPolicy::MakerThenTaker, dec!(0.40), dec!(5), "k3"),
                0,
                2,
            )
            .unwrap();
        c.confirm_live(&id2, 200).unwrap();
        assert_eq!(
            c.ome().get(&id2).unwrap().escalate_at_ms,
            Some(200 + c.config.default_maker_timeout_ms)
        );
    }

    #[test]
    fn cancel_remaining_keeps_filled_shares_and_retires_the_remainder() {
        let mut c = live_core(dec!(10));
        let (id, _) = c
            .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
            .unwrap();
        c.bind_venue(&id, "0xv1".into(), 2).unwrap();
        // Partial fill: 2 of 5.
        c.ingest_fill(fill(&id, dec!(2), dec!(0.40), None), 3)
            .unwrap();
        let o = c.ome().get(&id).unwrap();
        assert_eq!(o.status, OrderStatus::PartiallyFilled);
        assert_eq!(o.filled_size, dec!(2));

        c.cancel_remaining(&id, 4).unwrap();
        let o = c.ome().get(&id).unwrap();
        assert_eq!(o.status, OrderStatus::Cancelled);
        // The filled part is NOT refunded — it is real inventory now.
        assert_eq!(c.positions().open_positions().len(), 1);
        // And the venue-side delete rides the queue.
        assert_eq!(c.take_pending_venue_cancels(), vec!["0xv1".to_string()]);
    }

    #[test]
    fn close_filled_flattens_only_the_shares_behind_the_order() {
        let mut c = dry_core(dec!(10));
        // Maker BUY 5 rests; a partial fill accrues 2 real shares.
        let (id, _) = c
            .place(order(FillPolicy::Maker, dec!(0.40), dec!(5), "k1"), 0, 1)
            .unwrap();
        c.ingest_fill(fill(&id, dec!(2), dec!(0.40), None), 3)
            .unwrap();
        let o = c.ome().get(&id).unwrap();
        assert_eq!(o.status, OrderStatus::PartiallyFilled);

        // Unknown orders / positions without inventory are errors, not panics.
        assert_eq!(
            c.close_filled("nope", 4).unwrap_err().code,
            CoreErrorCode::UnknownOrder
        );

        // A bid exists → the flatten SELL crosses and closes the position.
        c.book_snapshot(
            "tok",
            vec![(dec!(0.44), dec!(100))],
            vec![(dec!(0.50), dec!(100))],
            4,
        );
        assert_eq!(c.close_filled(&id, 5).unwrap(), 1);
        assert!(c.positions().open_positions().is_empty());
    }

    #[test]
    fn three_consecutive_sweep_failures_freeze_trading() {
        let mut c = live_core(dec!(10));
        let err = crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::VenueError,
            "venue down",
        );
        c.on_reconcile_failed(err.clone(), 1);
        c.on_reconcile_failed(err.clone(), 2);
        assert!(
            !c.risk.is_killed(),
            "one or two failures are noise, not a freeze"
        );
        c.on_reconcile_failed(err, 3);
        assert!(c.risk.is_killed());
        let e = c
            .place(order(FillPolicy::Taker, dec!(0.4), dec!(1), "k1"), 0, 4)
            .unwrap_err();
        assert_eq!(e.code, CoreErrorCode::KillSwitchActive);
    }

    #[test]
    fn a_successful_sweep_resets_the_failure_streak() {
        let mut c = live_core(dec!(10));
        let err = crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::VenueError,
            "venue down",
        );
        c.on_reconcile_failed(err.clone(), 1);
        c.on_reconcile_failed(err, 2);
        // A sweep that RUNS (empty snapshot is fine) proves the channel alive.
        let snap = crate::reconcile::VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![],
            now_ms: 3,
        };
        c.reconcile(snap).unwrap();
        c.on_reconcile_failed(
            crate::model::CoreError::new(
                blitzkrieg_market_api::CoreErrorCode::VenueError,
                "venue down again",
            ),
            4,
        );
        assert!(
            !c.risk.is_killed(),
            "streak must count CONSECUTIVE failures only"
        );
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
        // Taker BUY 0.40 x 5 fills → position opened. A FOK taker needs
        // resting ask depth to fill.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 0);
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
        // A FOK taker needs resting ask depth to fill.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 0);
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
        // A FOK taker entry needs resting ask depth to fill.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 0);
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
            // The entry is a FOK taker: it needs resting ask depth to fill.
            c.book_snapshot(token, vec![], vec![(dec!(0.40), dec!(100))], 0);
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
    use crate::service::shadow_evolution_tests::scratch;
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
            max_orderbook_stale_ms: 8000,
            momentum_window_sec: 30,
            momentum_tol_pct: dec!(0.03),
            size_usd: dec!(2.5),
            min_shares: dec!(10),
            max_shares: dec!(10),
            size_pct: Decimal::ZERO,
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
        // PR-B: the kernel registers no strategies, so the dispatch tests that
        // drive the reference dip buyer host the test adapter explicitly.
        crate::strategies::test_support::host(
            c.engine.as_mut().unwrap(),
            engine_cfg().trend,
            engine_cfg().spread_arb,
        );
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
        fn on_round(&mut self, _slot: i64, _time_left_sec: i64, _now_ms: i64) {}
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
                            shares: None,
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
        let mut eng = Engine::new(cfg.clone());
        // Host the reference adapters too (the kernel ships no strategies, PR-B),
        // so gate-exemption accounting has the same declarations to list the
        // production session would have.
        crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
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
        // No isolation step is needed any more: the kernel ships zero strategies
        // (PR-B), so a freshly built engine hosts exactly the dips registered above.
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
        // `install_engine` loads only what `strategy_dir` holds (PR-B: the
        // kernel registers nothing itself), so the test hosts the three
        // adapters right after it — the same registration the loader performs
        // for a real library. Nothing is enabled unless the config says so.
        let install = |cfg: CoreConfig| {
            let mut c = Core::new(cfg.clone());
            // No `requested_strategies` here, so the startup self-check has
            // nothing to insist on and install cannot refuse.
            cfg.install_engine(&mut c).expect("engine install");
            let cfg2 = cfg.engine_config();
            crate::strategies::test_support::host_disabled(
                c.engine.as_mut().expect("engine installed"),
                cfg2.trend,
                cfg2.spread_arb,
            );
            // The startup selection is applied after the strategy dir loads, in
            // the same order production uses — so a name asked for at startup
            // lands on the just-registered adapter (an unknown name stays a
            // silent no-op, exactly what install_engine does).
            for name in &cfg.enabled_strategies {
                c.set_strategy_enabled(name, true);
            }
            for name in &cfg.disabled_strategies {
                c.set_strategy_enabled(name, false);
            }
            c
        };
        let base = CoreConfig {
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            ..Default::default()
        };

        // Default: ZERO strategies enabled (the kernel couples to none). The
        // hosted adapters all register, all disabled.
        let plain = install(base.clone());
        assert!(
            plain.enabled_strategy_names().is_empty(),
            "a fresh boot must enable nothing: {:?}",
            plain.enabled_strategy_names()
        );
        assert_eq!(
            plain.strategy_names(),
            vec![
                "spread_arb".to_string(),
                "trend_follow".to_string(),
                "mean_reversion".to_string()
            ],
            "registered but disabled"
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
            vec!["trend_follow".to_string()]
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
        assert!(
            both.enabled_strategy_names().is_empty(),
            "enable then disable the same name nets to off: {:?}",
            both.enabled_strategy_names()
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

    /// The enabled-set is recorded on every toggle and on boot, and a fresh
    /// install with the same state path replays it — that is the whole
    /// "enable it once, it comes back after a restart" contract.
    #[test]
    fn strategy_enablement_persists_to_the_state_file() {
        let dir =
            std::env::temp_dir().join(format!("bk-strategy-state-svc-{}", std::process::id()));
        let state = dir.join("strategy-state.json");
        let _ = std::fs::remove_file(&state);

        let cfg = CoreConfig {
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            strategy_state_path: Some(state.to_string_lossy().into()),
            ..Default::default()
        };
        let install = || {
            let mut c = Core::new(cfg.clone());
            cfg.install_engine(&mut c).expect("engine install");
            let cfg2 = cfg.engine_config();
            crate::strategies::test_support::host_disabled(
                c.engine.as_mut().expect("engine installed"),
                cfg2.trend,
                cfg2.spread_arb,
            );
            c
        };

        // Boot with an empty set: install_engine records that empty set — and
        // the file now exists with nothing enabled.
        let mut c = install();
        assert!(c.enabled_strategy_names().is_empty());
        assert_eq!(
            crate::strategy_state::load(&state),
            Vec::<String>::new(),
            "boot reconciles the file to the effective set"
        );

        // The operator enables two strategies; both land in the file.
        assert!(c.set_strategy_enabled("spread_arb", true));
        assert!(c.set_strategy_enabled("trend_follow", true));
        assert_eq!(
            crate::strategy_state::load(&state),
            vec!["spread_arb".to_string(), "trend_follow".to_string()]
        );

        // Disabling records the off just the same.
        assert!(c.set_strategy_enabled("spread_arb", false));
        assert_eq!(
            crate::strategy_state::load(&state),
            vec!["trend_follow".to_string()]
        );

        // A brand-new install with the same state path replays the intent: the
        // boot list is read from the file (exactly what main.rs does), and the
        // selection applies after the strategy dir "loads" — the same order
        // production uses.
        let mut replay_cfg = cfg.clone();
        replay_cfg.enabled_strategies = crate::strategy_state::load(&state);
        let mut rebooted = Core::new(replay_cfg.clone());
        replay_cfg
            .install_engine(&mut rebooted)
            .expect("engine install");
        let rcfg = replay_cfg.engine_config();
        crate::strategies::test_support::host_disabled(
            rebooted.engine.as_mut().expect("engine installed"),
            rcfg.trend,
            rcfg.spread_arb,
        );
        for name in &replay_cfg.enabled_strategies {
            rebooted.set_strategy_enabled(name, true);
        }
        assert_eq!(
            rebooted.enabled_strategy_names(),
            vec!["trend_follow".to_string()],
            "the persisted intent survives the restart"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No `strategy_state_path` (backtests, most tests) — a toggle works but
    /// writes nothing anywhere.
    #[test]
    fn toggles_without_a_state_path_persist_nothing() {
        let cfg = CoreConfig {
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            ..Default::default()
        };
        let mut c = Core::new(cfg.clone());
        cfg.install_engine(&mut c).expect("engine install");
        let cfg2 = cfg.engine_config();
        crate::strategies::test_support::host_disabled(
            c.engine.as_mut().expect("engine installed"),
            cfg2.trend,
            cfg2.spread_arb,
        );
        assert!(c.set_strategy_enabled("trend_follow", true));
        assert_eq!(c.enabled_strategy_names(), vec!["trend_follow".to_string()]);
    }

    /// #265: naming strategies that do not exist is a REFUSAL, not a warning. A
    /// kernel that boots with a healthy-looking banner and then trades nothing
    /// because the library it was told to run was never loaded is exactly the
    /// failure this guards; `--allow-zero-strategies` is the acknowledged
    /// opt-out, and a name the operator also disabled explicitly was never
    /// requested in the first place.
    #[test]
    fn an_explicit_request_that_resolves_to_nothing_refuses_the_boot() {
        // The kernel registers nothing itself (PR-B), so in-process every
        // requested name is unresolved — which is precisely the production
        // shape of "the strategy library was not found".
        let boot = |requested: &[&str], disabled: &[&str], allow: bool| {
            let cfg = CoreConfig {
                dry_seed_balance: dec!(1000),
                engine_enabled: true,
                requested_strategies: requested.iter().map(|s| (*s).to_string()).collect(),
                disabled_strategies: disabled.iter().map(|s| (*s).to_string()).collect(),
                allow_zero_strategies: allow,
                ..Default::default()
            };
            let mut core = Core::new(cfg.clone());
            cfg.install_engine(&mut core)
        };

        // Nothing was asked for: the ordinary zero-strategy boot (tests, the
        // backtester, a session that only hosts builtins) is untouched.
        assert!(
            boot(&[], &[], false).is_ok(),
            "an unrequested empty set must never refuse"
        );

        // An explicit request that resolves to nothing refuses, and names both
        // what was asked for and the door out.
        let err = boot(&["dog_strategy"], &[], false)
            .expect_err("a request that resolves to nothing must refuse the boot");
        assert!(
            err.contains("refusing to start") && err.contains("dog_strategy"),
            "the refusal must name the request, got: {err}"
        );
        assert!(
            err.contains("--allow-zero-strategies"),
            "the refusal must name the opt-out, got: {err}"
        );
        // The message states the ENGINE's live set, not the request twice: the
        // first draft filled this slot with the enabled list under the label
        // "Requested:", which a probe read as "Requested: ." — a refusal that
        // cannot say what it refused is not a diagnosis.
        assert!(
            err.contains("the engine's live set is []"),
            "the refusal must state the live set, got: {err}"
        );

        // The door is not welded shut: the flag acknowledges the empty set.
        assert!(
            boot(&["dog_strategy"], &[], true).is_ok(),
            "--allow-zero-strategies must start normally"
        );

        // `--enable-strategy x --disable-strategy x` asked for nothing, so it is
        // not a request that failed — the same rule the toggle order follows.
        assert!(
            boot(&["dog_strategy"], &["dog_strategy"], false).is_ok(),
            "an explicitly disabled name is not part of the request"
        );

        // Several names: the refusal counts and lists them all.
        let err = boot(&["dog_strategy", "cat_strategy"], &[], false).expect_err("still a refusal");
        assert!(
            err.contains("dog_strategy") && err.contains("cat_strategy"),
            "got: {err}"
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
        let (tuning, dir) = scratch("toggle-cell");
        let base = CoreConfig {
            dry_seed_balance: dec!(1000),
            engine_enabled: true,
            shadow_evolution_tuning: tuning,
            ..Default::default()
        };
        let mut c = Core::new(base.clone());
        base.install_engine(&mut c).expect("engine install");
        // The kernel registers nothing itself (PR-B); host the adapters so the
        // evolution manager has strategies to build units for.
        let ecfg = base.engine_config();
        crate::strategies::test_support::host(
            c.engine.as_mut().expect("engine installed"),
            ecfg.trend,
            ecfg.spread_arb,
        );
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
        let _ = std::fs::remove_dir_all(&dir);
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

    // ── #202: the engine sizes from the balance the ledger hands it ─────────

    /// A one-asset dip core whose sizing is the production mapping
    /// (`CoreConfig::engine_config()`), so the ticket a test reads is the ticket
    /// the live server would emit on that account.
    fn equity_core(balance: Decimal, size_pct: Decimal) -> Core {
        let mut c = Core::new(CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(1000),
                ..Default::default()
            },
            dry_seed_balance: balance,
            engine_enabled: true,
            round_duration_sec: 900,
            auto_exits_enabled: false,
            size_usd: dec!(2.5),
            // The engine's own ceiling stays wide so the equity budget is what
            // decides the lot in these tests.
            min_shares: dec!(1),
            max_shares: dec!(1000),
            size_pct,
            assets: vec!["BTC".into()],
            positions: crate::position::PositionConfig {
                max_positions: 5,
                exit: crate::exit_policy::ExitConfig {
                    min_time_left_sec: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        });
        c.set_balance(balance);
        let cfg = c.config().engine_config();
        let mut eng = Engine::new(cfg.clone());
        crate::strategies::test_support::host(&mut eng, cfg.trend, cfg.spread_arb);
        eng.register_user_strategy(
            Box::new(TargetDip {
                name: "dip".into(),
                buy_below: dec!(0.45),
                assets: vec!["BTC".into()],
                gates: crate::strategies::GateExemptions::none(),
            }),
            "test".into(),
        )
        .unwrap();
        assert!(eng.set_strategy_enabled("dip", true));
        c.enable_engine(eng);
        c
    }

    /// Round + a 0.44-mid book on BTC: the dip strategy's one candidate.
    fn feed_btc_dip(c: &mut Core, now: i64) {
        c.engine_on_data(
            DataEvent::RoundMarkets {
                markets: three_markets(now),
                now_ms: now,
            },
            now,
        );
        feed_dip_on(c, now + 1_000, &["BTC"]);
    }

    /// The acceptance behind #202's first half: the SAME 20% on two account
    /// sizes produces lots 100× apart, and the engine reads the balance the
    /// ledger actually holds rather than a copy taken at configuration time.
    #[test]
    fn equity_sizing_scales_one_order_with_the_account() {
        let now = 1_000_000i64;
        let mut small = equity_core(dec!(4.8), dec!(20));
        feed_btc_dip(&mut small, now);
        assert_eq!(small.engine_evaluate(now + 1_000), 1);
        // 4.8 × 20% = 0.96 → 0.96/0.44 = 2.18 → 2 shares. The pre-#202 path
        // sent 10 shares (4.40 USD, 92% of this account) whatever the price.
        assert_eq!(small.list_orders()[0].size, dec!(2));
        assert_eq!(
            small.engine.as_ref().unwrap().equity_usd(),
            small.ledger.balance(),
            "the engine must have been handed the ledger's own balance"
        );

        let mut big = equity_core(dec!(480), dec!(20));
        feed_btc_dip(&mut big, now);
        assert_eq!(big.engine_evaluate(now + 1_000), 1);
        // 96/0.44 = 218.18 → 218 shares: the same statement, 100× the account.
        assert_eq!(big.list_orders()[0].size, dec!(218));
        // The panel states both the percentage and what it is worth here, so the
        // scaling is visible without re-deriving it from the flags.
        let small_stats = small.strategy_stats();
        assert_eq!(
            dec_of(&strategy_entry(&small_stats, "dip")["effectiveSizePct"]),
            dec!(20)
        );
        assert_eq!(
            dec_of(&strategy_entry(&small_stats, "dip")["entryBudgetUsd"]),
            dec!(0.96)
        );
        let big_stats = big.strategy_stats();
        assert_eq!(
            dec_of(&strategy_entry(&big_stats, "dip")["entryBudgetUsd"]),
            dec!(96)
        );
    }

    /// A budget that cannot buy one whole share emits NO order and says so in
    /// the stats, rather than turning into a 0-size rejection on every tick.
    #[test]
    fn an_unaffordable_equity_budget_skips_the_signal_visibly() {
        let now = 1_000_000i64;
        let mut c = equity_core(dec!(4.8), dec!(1)); // 0.048 USD per entry
        feed_btc_dip(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 1_000), 0, "no ticket, no order");
        assert!(c.list_orders().is_empty());
        assert_eq!(
            c.engine_stats_at(now + 1_000)["sizing"]["sizePctSkippedSignals"].as_u64(),
            Some(1),
            "the skip must be counted, not silent"
        );
    }

    /// Unconfigured (the shipped default) is untouched: the service pushes a
    /// balance every cycle now, and this is the test that proves that push alone
    /// changes nothing about an absolute-budget deployment's orders.
    #[test]
    fn an_unconfigured_core_keeps_its_historical_lot() {
        let now = 1_000_000i64;
        let mut c = equity_core(dec!(4.8), Decimal::ZERO);
        feed_btc_dip(&mut c, now);
        assert_eq!(c.engine_evaluate(now + 1_000), 1);
        // The historical path: 2.5/0.44 ≈ 5.68 → 6 shares, inside [1, 1000].
        assert_eq!(c.list_orders()[0].size, dec!(6));
        assert_eq!(
            c.engine_stats_at(now + 1_000)["sizing"]["sizePctSkippedSignals"].as_u64(),
            Some(0)
        );
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
                timing_min_time_left_sec: None,
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
        // Only the hosted test adapter declares an exemption (momentum) here;
        // the test dip strategies declare nothing beyond what their builder gave
        // them. (PR-B: there is no builtin set — every hosted strategy declares
        // its own gates through the same seam.)
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
                timing_min_time_left_sec: None,
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
    fn a_declared_timing_floor_is_reported_beside_the_gates() {
        // D-31: the floor is the difference between "this opt-out reaches into the
        // closing window" and "it stops at the global window", so it has to be
        // visible from the stats snapshot alone. `fader` declares a floor;
        // `plain` does not, and a strategy that declares nothing must NOT grow a
        // key it never asked for.
        let c = core_with_declared_dips(
            HashMap::new(),
            &[("fader", &["BTC"]), ("plain", &["ETH"])],
            5,
            0,
            |name| crate::strategies::GateExemptions {
                timing: name == "fader",
                momentum: name == "fader",
                timing_min_time_left_sec: (name == "fader").then_some(180),
            },
        );
        let stats = c.engine_stats();
        assert_eq!(
            stats["blocked"]["declaredExemptions"],
            serde_json::json!([
                { "strategy": "mean_reversion", "gates": ["momentum"] },
                { "strategy": "fader", "gates": ["timing", "momentum"], "timingFloorSec": 180 }
            ]),
            "a declared floor is named; an undeclared one adds nothing"
        );

        let by_name = |n: &str| -> serde_json::Value {
            stats["strategies"]
                .as_array()
                .expect("strategies array")
                .iter()
                .find(|s| s["name"] == n)
                .unwrap_or_else(|| panic!("no stats row for {n}"))["gateExemptionTimingFloorSec"]
                .clone()
        };
        assert_eq!(by_name("fader"), serde_json::json!(180));
        // `null`, not absent and not 0: "this strategy left the floor to the kernel"
        // must be distinguishable from "it declared a floor of zero".
        assert_eq!(by_name("plain"), serde_json::Value::Null);
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
        // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
        // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
        c.book_snapshot(
            "up",
            vec![(dec!(0.38), dec!(100))],
            vec![(dec!(0.39), dec!(100))],
            now + 13_000,
        );
        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "spread_arb");
        assert_eq!(s["enabled"], true);
        // The kernel has no `builtin` class any more (PR-B): the test adapter is
        // hosted by the test itself, and a production session would report the
        // library's `dylib:<path>` provenance here instead.
        assert_eq!(s["source"], "test");
        assert_eq!(s["ordersPlaced"], 1);
        assert_eq!(s["openPositions"], 1);
        assert_eq!(dec_of(&s["openNotionalUsd"]), dec!(3.90));
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

    /// E16/#98: the portfolio-level open-notional cap bounds the TOTAL open
    /// commitment across all strategies (0 = off = shipped behaviour).
    #[test]
    fn portfolio_notional_cap_bounds_total_open_exposure() {
        // Entry notional is 0.43 * 10 = 4.30: a 2.00 portfolio cap rejects it
        // even though no per-strategy limit is configured at all.
        let mut c = core_with_engine(HashMap::new());
        c.risk_config_mut().max_open_notional_usd = dec!(2);
        let now = 1_000_000i64;
        feed_entry_setup(&mut c, now);
        assert_eq!(
            c.engine_evaluate(now + 12_000),
            0,
            "a 4.30 entry breaches a 2.00 portfolio cap"
        );
        assert_eq!(c.engine_stats()["strategyLimitRejected"], 1);
        let stats = c.strategy_stats();
        let s = strategy_entry(&stats, "spread_arb");
        assert_eq!(
            s["rejectionCauses"]["limit.portfolioNotionalCap"], 1,
            "the portfolio cap must be bucketed so tooling can answer why: {s}"
        );

        // 0 = off: the same entry goes through untouched.
        let mut c2 = core_with_engine(HashMap::new());
        feed_entry_setup(&mut c2, now);
        assert_eq!(c2.engine_evaluate(now + 12_000), 1);
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
        // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
        // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
        c.book_snapshot(
            "up",
            vec![(dec!(0.38), dec!(100))],
            vec![(dec!(0.39), dec!(100))],
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
        // The tuned 0.88 discount rests the entry bid at round2(0.44*0.88)
        // = 0.39, so the crossing ask is 0.39 (was 0.42 under the 0.98 factor).
        c.book_snapshot(
            "up",
            vec![(dec!(0.38), dec!(100))],
            vec![(dec!(0.39), dec!(100))],
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
        // Ask rests AT the buy limit: the walk prices 0.40, and the slippage
        // dial prices the impact of our own 10 shares on top of it.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(500))], 900);
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

    // ── #183 · a dry maker fills at most what the book offers ──────────────
    //
    // Before this, a crossing maker filled its WHOLE size at once, so a dry run
    // could never reproduce the partial fills live hits on day one and every
    // backtest read as a 100% fill rate.

    /// ACCEPTANCE (#183): a maker only trades the crossing depth, and the three
    /// records of that fact — order, position and ledger — agree.
    #[test]
    fn crossing_depth_limits_the_maker_fill_and_the_books_agree() {
        let mut c = core_with(FillModel::default());
        let (id, _) = c
            .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();
        // Only 4 shares rest at the crossing ask. That is the whole of what a
        // maker can trade on this book.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(4))], 1_100);

        let o = c.ome().get(&id).unwrap();
        assert_eq!(o.filled_size, dec!(4), "depth, not order size: {o:?}");
        assert_eq!(o.status, OrderStatus::PartiallyFilled);
        assert_eq!(c.ome().remaining(&id), dec!(6), "the rest is still working");
        // What the fill cost is what the ledger moved: 4 × 0.40.
        assert_eq!(
            c.ledger().balance(),
            dec!(1000) - dec!(1.6),
            "cash follows the fill, not the order"
        );
        assert_eq!(
            c.ledger().reserved(),
            dec!(2.4),
            "0.40 × 6 unfilled shares stay committed while the order rests"
        );
        assert_eq!(c.ledger().available(), c.ledger().balance() - dec!(2.4));
        assert!(c.ledger().is_balanced());
        // The position holds the shares that actually traded.
        let pos = &c.positions().open_positions()[0];
        assert_eq!(pos.shares, dec!(4), "position = filled size");
        assert_eq!(pos.entry_price, dec!(0.40));
        assert_eq!(pos.cost_usd, dec!(1.6), "basis = cash actually spent");
        let audit = c.run_accounting_audit(1_200);
        assert!(audit.ok, "{}", audit.note);
    }

    /// ACCEPTANCE (#183): the remainder of a partially filled maker is a real,
    /// cancellable order — the entry that live leaves half-open must be
    /// closable in dry too.
    #[test]
    fn a_partially_filled_maker_keeps_its_remainder_cancellable() {
        let mut c = core_with(FillModel::default());
        let (id, _) = c
            .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(4))], 1_100);

        c.cancel(&id, 1_200).unwrap();

        let o = c.ome().get(&id).unwrap();
        assert_eq!(o.status, OrderStatus::Cancelled);
        assert_eq!(o.filled_size, dec!(4), "cancelling never unwinds a fill");
        assert_eq!(c.ledger().reserved(), Decimal::ZERO, "2.4 released");
        assert_eq!(
            c.ledger().available(),
            c.ledger().balance(),
            "stranded cash is unusable cash"
        );
        assert!(c.ledger().is_balanced());
        // The filled half stays a position worth exactly what was paid.
        let pos = &c.positions().open_positions()[0];
        assert_eq!(pos.shares, dec!(4));
        assert_eq!(pos.cost_usd, dec!(1.6));
        assert_eq!(c.ledger().balance(), dec!(1000) - dec!(1.6));
        let audit = c.run_accounting_audit(1_300);
        assert!(audit.ok, "{}", audit.note);
    }

    /// ACCEPTANCE (#183): successive books keep filling the order up to each
    /// book's depth until it is whole — a partial fill is not a terminal state.
    #[test]
    fn partial_maker_fills_accumulate_until_the_order_is_whole() {
        let mut c = core_with(FillModel::default());
        let (id, _) = c
            .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();

        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(4))], 1_100);
        assert_eq!(c.ome().get(&id).unwrap().filled_size, dec!(4));
        // A shallower book a moment later adds only what it offers.
        c.book_snapshot("tok", vec![], vec![(dec!(0.39), dec!(3))], 1_200);
        assert_eq!(c.ome().get(&id).unwrap().filled_size, dec!(7));
        assert_eq!(
            c.ome().get(&id).unwrap().status,
            OrderStatus::PartiallyFilled
        );
        // The third book covers the remaining 3: the order ends whole.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(10))], 1_300);

        let o = c.ome().get(&id).unwrap();
        assert_eq!(o.filled_size, dec!(10));
        assert_eq!(o.status, OrderStatus::Filled);
        assert_eq!(c.ome().remaining(&id), Decimal::ZERO);
        assert_eq!(c.ledger().reserved(), Decimal::ZERO);
        assert_eq!(c.ledger().balance(), dec!(1000) - dec!(4));
        // Every chunk was a maker fill at the resting limit, so the average is
        // the quote itself and no taker fee was ever charged.
        assert_eq!(o.avg_fill_price, Some(dec!(0.40)));
        assert_eq!(c.positions().open_positions()[0].shares, dec!(10));
        let audit = c.run_accounting_audit(1_400);
        assert!(audit.ok, "{}", audit.note);
    }

    /// Two chunks inside the SAME millisecond are two executions, not one
    /// revised report: each carries its own trade identity, so the second books
    /// on top of the first instead of being swallowed as a downward revision.
    #[test]
    fn two_partial_chunks_in_one_millisecond_both_book() {
        let mut c = core_with(FillModel::default());
        let (id, _) = c
            .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(3))], 1_100);
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(4))], 1_100);

        assert_eq!(
            c.ome().get(&id).unwrap().filled_size,
            dec!(7),
            "3 + 4 in the same millisecond: the second chunk is a second trade"
        );
        assert_eq!(c.ledger().balance(), dec!(1000) - dec!(2.8));
        assert!(c.ledger().is_balanced());
    }

    /// A crossing book whose levels are empty offers nothing to take: the order
    /// stays whole and resting rather than filling against zero volume.
    #[test]
    fn a_crossing_book_with_no_volume_leaves_the_maker_resting() {
        let mut c = core_with(FillModel::default());
        let (id, _) = c
            .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), Decimal::ZERO)], 1_100);

        let o = c.ome().get(&id).unwrap();
        assert!(o.status.is_live(), "no volume traded: {o:?}");
        assert_eq!(o.filled_size, Decimal::ZERO);
        assert!(c.positions().open_positions().is_empty());
        assert_eq!(c.ledger().reserved(), dec!(4), "still committed");
    }

    /// The queue-share dial scales a maker fill below the depth as well, so a
    /// backtest can price being behind the queue. Identity is its ceiling.
    #[test]
    fn depth_share_dial_scales_the_maker_fill() {
        let mut c = core_with(FillModel {
            maker_depth_share_bps: 5_000,
            ..FillModel::default()
        });
        let (id, _) = c
            .place(buy(FillPolicy::Maker, dec!(0.40), dec!(10)), 0, 1_000)
            .unwrap();
        // Deep book: the dial, not the depth, is the binding constraint.
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1_100);

        let filled = c.ome().get(&id).unwrap().filled_size;
        assert!(
            filled > Decimal::ZERO && filled <= dec!(5),
            "a 50% queue share of a 10-share order: {filled}"
        );
        assert_eq!(c.positions().open_positions()[0].shares, filled);
        assert_eq!(c.ledger().balance(), dec!(1000) - filled * dec!(0.40));
        assert!(c.ledger().is_balanced());
        let audit = c.run_accounting_audit(1_200);
        assert!(audit.ok, "{}", audit.note);
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
        // order and crosses the remaining 6 shares as a TAKER. The ask rests
        // ABOVE the resting bid (the matcher stays quiet) and deep enough
        // for the honest FOK walk to cover all 6 shares.
        c.book_snapshot("tok", vec![], vec![(dec!(0.45), dec!(500))], 1_001);
        c.tick(1_002).unwrap();
        let pos = c.positions().open_positions()[0].clone();
        assert_eq!(pos.shares, dec!(10), "both legs accrue onto one position");
        assert_eq!(
            pos.entry_role,
            OrderRole::MakerThenTaker,
            "the position records that both kinds of fill happened"
        );
        // Basis is the real money spent: 4×0.43 rested, 6×0.45 crossed at the
        // book's ask (the escalated leg pays the walk price, not the maker's).
        assert_eq!(pos.cost_usd, dec!(4) * dec!(0.43) + dec!(6) * dec!(0.45));
        // The fee covers the TAKER leg only — not all 10 shares at the taker rate.
        let expected_fee = (crate::exit_policy::taker_fee_pct(dec!(0.45)) / Decimal::ONE_HUNDRED)
            * dec!(0.45)
            * dec!(6);
        assert_eq!(pos.flows.entry_fee_usd, expected_fee);
        assert!(expected_fee > Decimal::ZERO);

        // Exit in full as a taker at a profit.
        c.book_snapshot("tok", vec![(dec!(0.95), dec!(500))], vec![], 4);
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
        // Gross = proceeds − real basis: 9.5 − (4×0.43 + 6×0.45), minus the
        // taker legs' fees (the escalated entry leg and the exit).
        let exit_fee =
            (crate::exit_policy::taker_fee_pct(dec!(0.95)) / Decimal::ONE_HUNDRED) * dec!(9.5);
        assert_eq!(
            closed.net_pnl_usd,
            dec!(9.5) - (dec!(4) * dec!(0.43) + dec!(6) * dec!(0.45)) - expected_fee - exit_fee
        );
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

        // Close the whole holding as a taker at a loss. The 0.40 bid is deep
        // enough for the honest FOK exit to fill at the walked price.
        c.book_snapshot("tok", vec![(dec!(0.40), dec!(500))], vec![], 3);
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

#[cfg(test)]
mod fee_quote_tests {
    use super::*;
    use crate::exit_policy::{FeeSchedule, legacy_quadratic_schedule, official_schedule};

    fn core_with(fee_schedule_replay: bool) -> Core {
        Core::new(CoreConfig {
            fee_schedule_replay,
            ..CoreConfig::default()
        })
    }

    /// #234, item 1 — the check has to be able to say NO. Both sides used to read
    /// `fee_schedule()`, so changing the shipped rate moved the declaration and the
    /// charge together and `model_matches` stayed `true` while the fee moved 44%.
    /// Here the charge side is the shipped curve with exactly that rate change.
    #[test]
    fn model_matches_reports_a_changed_rate() {
        let shipped = legacy_quadratic_schedule();
        // The shipped curve is what this repository pins, so it matches...
        assert!(fee_quote_at(shipped, Some(dec!(0.5))).model_matches);
        // ...and 0.125 -> 0.07 on the same curve must not be reported as a match.
        let repriced = FeeSchedule {
            rate: dec!(0.07),
            ..shipped
        };
        assert_eq!(repriced.exponent, 2, "only the rate moves here");
        let quote = fee_quote_at(repriced, Some(dec!(0.5)));
        assert!(
            !quote.model_matches,
            "rate 0.125 -> 0.07 is a 44% fee change and the quote must say so"
        );
        // The quote still reports what it CHARGES: the flag is the split between
        // the charge and the pin, not a reason to hide the number.
        assert_eq!(quote.fee_per_share, dec!(0.004375));
        assert_eq!(quote.fee_pct_of_price, dec!(0.875));
        assert_eq!(quote.model, "legacy_quadratic");
    }

    /// #234, item 1 — a curve that only COINCIDES with the pinned one at the
    /// sampled price is still not the pinned curve. `fee_quote` samples ONE price
    /// (0.5 unless the caller asks otherwise), and different `(rate, exponent)`
    /// pairs cross there: `0.03125*(p(1-p))^1` prices the pinned legacy curve's
    /// `0.0078125` per share at p=0.5 and a different fee at every other price,
    /// so a comparison made only at the sampled number reports a wrong
    /// configuration as a match.
    #[test]
    fn model_matches_rejects_a_curve_that_only_coincides_at_the_sampled_price() {
        let pinned = legacy_quadratic_schedule();
        let crossed = FeeSchedule {
            rate: dec!(0.03125),
            exponent: 1,
            ..pinned
        };
        // The coincidence, stated as the arithmetic it is: the same number at
        // 0.5, a different one an eighth of a price away. Without both halves the
        // test would also pass on two curves that never cross at all.
        assert_eq!(
            charged_per_share(crossed, dec!(0.5)),
            charged_per_share(pinned, dec!(0.5)),
            "the two curves must actually cross at the sampled price"
        );
        assert_ne!(
            crossed.fee_per_share(dec!(0.375)),
            pinned.fee_per_share(dec!(0.375)),
            "…and must diverge elsewhere, or they are the same curve"
        );

        // It claims the pinned name while charging a different curve.
        assert_eq!(crossed.name, "legacy_quadratic");
        for p in [dec!(0.1), dec!(0.375), dec!(0.5), dec!(0.75), dec!(0.9)] {
            assert!(
                !fee_quote_at(crossed, Some(p)).model_matches,
                "a configuration that is not the pinned one must be refused at p={p}"
            );
        }
        // …and the refusal did not turn into "nothing ever matches".
        assert!(fee_quote_at(pinned, Some(dec!(0.5))).model_matches);
    }

    /// The exponent is a parameter of the pin too, and a schedule this repository
    /// cannot describe is not one it can vouch for.
    #[test]
    fn model_matches_reports_a_changed_exponent_and_an_unknown_name() {
        let shipped = legacy_quadratic_schedule();
        let re_exponented = FeeSchedule {
            exponent: 1,
            ..shipped
        };
        assert!(!fee_quote_at(re_exponented, Some(dec!(0.5))).model_matches);
        let unknown = FeeSchedule {
            name: "not_a_schedule",
            ..shipped
        };
        assert!(
            !fee_quote_at(unknown, Some(dec!(0.5))).model_matches,
            "an undescribed schedule must report 'no', not 'nothing to check'"
        );
    }

    /// The production entry point reads the schedule in force and agrees with the
    /// pinned numbers — the anchor #224's裁决 rests on: legacy @0.50 = 1.5625%.
    #[test]
    fn the_shipped_quote_matches_the_pin_and_the_anchor() {
        let quote = fee_quote(None);
        assert_eq!(quote.model, "legacy_quadratic");
        assert_eq!(quote.rate, dec!(0.125));
        assert_eq!(quote.exponent, 2);
        assert_eq!(quote.price, dec!(0.5));
        assert_eq!(quote.fee_per_share, dec!(0.0078125));
        assert_eq!(quote.fee_pct_of_price, dec!(1.5625));
        assert!(quote.model_matches);
        // The public helper the quote is built from spells the charge the same way.
        assert_eq!(charged_fee_per_share(dec!(0.5)), quote.fee_per_share);
        // A counterfactual curve is checked against the pin FOR ITS OWN NAME, so a
        // replay's official schedule matches here — `model_matches` answers "does
        // this curve price what this repository says that curve prices", not "is
        // this the shipped default". Which schedule is the default is a separate
        // assertion (`exit_policy::tests::default_fee_schedule_is_the_shipped_one`,
        // and PINNED_DEFAULT_MODEL on the gate side); confusing the two would make
        // this flag unable to answer either question.
        let official = fee_quote_at(official_schedule(), Some(dec!(0.5)));
        assert!(official.model_matches);
        assert_eq!(
            official.fee_pct_of_price,
            dec!(3.5),
            "official @0.50 = 3.5% of price (#224 anchor)"
        );
        assert_eq!(official.fee_per_share, dec!(0.0175));
    }

    /// #234, item 4 — the live charge sites' invariant: only a core that declared
    /// itself a replay may charge anything but the shipped schedule. The predicate
    /// is tested directly because the process-wide schedule is set-once, so a test
    /// cannot install a second one.
    #[test]
    fn only_a_replay_may_charge_a_counterfactual_schedule() {
        let live = core_with(false);
        assert!(live.may_charge(legacy_quadratic_schedule()));
        assert!(
            !live.may_charge(official_schedule()),
            "a live core must refuse the counterfactual schedule"
        );
        let repriced = FeeSchedule {
            rate: dec!(0.07),
            ..legacy_quadratic_schedule()
        };
        assert!(
            !live.may_charge(repriced),
            "a repriced legacy curve keeps the name and is still not the shipped one"
        );
        let replay = core_with(true);
        assert!(replay.may_charge(official_schedule()));

        // The guard does not obstruct the shipped deployment: the live charge
        // sites still price the schedule in force (this call is the assert's
        // happy path, and would panic if the default config tripped it).
        assert_eq!(live.live_taker_fee_pct(dec!(0.5)), dec!(1.5625));
        assert_eq!(replay.live_taker_fee_pct(dec!(0.5)), dec!(1.5625));
    }
}

#[cfg(test)]
mod trading_capability_tests {
    use super::*;
    use crate::risk::RiskConfig;
    use rust_decimal_macros::dec;

    fn core() -> Core {
        Core::new(CoreConfig {
            mode: Mode::Dry,
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            auto_exits_enabled: false,
            ..Default::default()
        })
    }

    fn pending_req(key: &str, strategy: &str) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price: dec!(0.4),
            size: dec!(10),
            internal_key: key.into(),
            strategy: strategy.into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    fn venue_err() -> crate::model::CoreError {
        crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::NotAuthenticated,
            "auth failed",
        )
        .with_raw("401 unauthorized")
    }

    /// After the venue refuses an order, a retry of the SAME intent must wait
    /// out its backoff locally instead of re-POSTing at full tick rate (the
    /// 380+-rejection storm this regression pins). A DIFFERENT intent is
    /// unaffected, and the intent opens again once its backoff expires.
    #[test]
    fn venue_rejection_pauses_only_that_intent() {
        let mut c = core();
        let (id, _) = c.place_pending(pending_req("k1", "s1"), 1_000).unwrap();
        c.reject_live_result(&id, Some(venue_err()), 1_100).unwrap();

        // Same intent, immediately after: locally paused, with the real reason.
        // Goes through place() (the placement gate), not just the adapter path.
        let e = c.place(pending_req("k1", "s1"), 0, 1_200).unwrap_err();
        assert_eq!(e.code, blitzkrieg_market_api::CoreErrorCode::RiskRejected);
        assert!(e.message.contains("rejection cooldown"));

        // A different intent is free to go.
        c.place_pending(pending_req("k2", "s1"), 1_200).unwrap();

        // Backoff expiry re-opens the intent (dry settles it → position opens).
        // The ask rests AT the 0.4 buy limit, deep enough for the honest FOK.
        c.book_snapshot("tok", vec![], vec![(dec!(0.4), dec!(500))], 3_000);
        c.place(pending_req("k1", "s1"), 0, 1_100 + 2_000).unwrap();
        assert_eq!(c.positions().open_positions().len(), 1);
    }

    /// Consecutive venue refusals freeze trading: the risk gate goes
    /// kill-switch, the panel sees a reason, and further placement is refused
    /// with KillSwitchActive. One rejection short of the threshold must NOT
    /// freeze.
    #[test]
    fn consecutive_venue_rejections_freeze_trading() {
        let mut c = core();
        for i in 0..4 {
            let (id, _) = c
                .place_pending(pending_req(&format!("k{i}"), "s1"), i * 1_000 + 1)
                .unwrap();
            c.reject_live_result(&id, Some(venue_err()), i * 1_000 + 100)
                .unwrap();
        }
        assert!(
            !c.is_killed(),
            "4 consecutive rejections must not freeze yet"
        );
        let (id, _) = c.place_pending(pending_req("k4", "s1"), 5_000).unwrap();
        c.reject_live_result(&id, Some(venue_err()), 5_100).unwrap();
        assert!(c.is_killed(), "the 5th consecutive rejection must freeze");
        let stats = c.engine_stats();
        assert_eq!(stats["venueRejected"], 5);
        assert_eq!(stats["tradingFrozen"]["active"], true);
        assert!(
            stats["tradingFrozen"]["reason"]
                .as_str()
                .unwrap()
                .contains("consecutive venue rejections")
        );
        assert!(
            stats["lastVenueError"]["message"]
                .as_str()
                .unwrap()
                .contains("NotAuthenticated"),
            "the panel must show the RAW venue error, not the freeze message: {stats}"
        );
        // #180: and the slot is structured now — the raw message AND the code
        // that classifies it, so a client can branch without parsing text.
        assert_eq!(
            stats["lastError"]["code"], "NOT_AUTHENTICATED",
            "the slot keeps the ROOT CAUSE, not the KILL_SWITCH_ACTIVE paraphrase"
        );
        assert_eq!(stats["lastError"]["message"], "auth failed");
        // Both keys are the same record: one timestamp, one writer.
        assert_eq!(stats["lastVenueError"]["tsMs"], stats["lastError"]["tsMs"]);
        assert_eq!(
            stats["lastVenueError"]["message"], "NotAuthenticated: auth failed",
            "the legacy key keeps its pre-#180 rendering exactly"
        );
        let e = c.place_pending(pending_req("k9", "s1"), 6_000).unwrap_err();
        assert_eq!(
            e.code,
            blitzkrieg_market_api::CoreErrorCode::KillSwitchActive
        );
    }

    /// A failed capability self-check freezes trading, and the report stays
    /// visible in the diagnostics; a passing report does not.
    #[test]
    fn failed_self_check_freezes_trading() {
        let mut c = core();
        c.on_self_check(blitzkrieg_market_api::SelfCheckReport {
            ok: false,
            ts_ms: 1_000,
            items: vec![blitzkrieg_market_api::SelfCheckItem {
                name: "balance".into(),
                ok: false,
                detail: "401 unauthorized".into(),
            }],
        });
        assert!(c.is_killed());
        let stats = c.engine_stats();
        assert_eq!(stats["selfCheck"]["ok"], false);
        assert_eq!(stats["tradingFrozen"]["active"], true);

        // An explicit resume clears the freeze (operator decision).
        c.risk.resume();
        assert!(!c.is_killed());
    }

    /// A venue sell on an order the OME never issued (a manual close made
    /// directly on the venue) must fold into the position book: the position
    /// disappears with a Manual close and the ledger receives the proceeds —
    /// instead of haunting the exit rules forever.
    #[test]
    fn manual_venue_close_reconciles_the_position() {
        let mut c = core();
        // Ask at the buy limit, deep enough for the honest FOK entry to fill.
        c.book_snapshot("tok", vec![], vec![(dec!(0.4), dec!(500))], 900);
        c.place(buy_taker(dec!(0.4), dec!(10)), 0, 1_000).unwrap();
        assert_eq!(c.positions().open_positions().len(), 1);
        let cost = c.positions().open_positions()[0].cost_usd;
        let seed = dec!(1000);

        let snap = crate::reconcile::VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![crate::reconcile::VenueTrade {
                venue_order_id: "manual-1".into(),
                trade_id: "t1".into(),
                token_id: "tok".into(),
                side: Side::Sell,
                size: dec!(10),
                price: dec!(0.6),
                ts_ms: 2_000,
                tx_hash: None,
                maker: Some(false),
            }],
            now_ms: 3_000,
        };
        c.reconcile(snap).unwrap();

        assert!(
            c.positions().open_positions().is_empty(),
            "the manually closed position must vanish from the book"
        );
        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.exit_reason, ExitReason::Manual);
        assert_eq!(closed.shares, dec!(10));
        // Cash identity: seed - entry (cost + taker fee) + exit proceeds - exit fee.
        let entry_fee = (crate::exit_policy::taker_fee_pct(dec!(0.4)) / dec!(100)) * dec!(4);
        let fee = (crate::exit_policy::taker_fee_pct(dec!(0.6)) / dec!(100)) * dec!(6);
        assert_eq!(
            c.ledger().balance(),
            seed - cost - entry_fee + dec!(6) - fee,
            "the ledger must reflect the cash the manual close actually moved"
        );
        assert_eq!(c.recon_watermark_ms, 2_000);

        // Idempotent: replaying the same sweep must not double-count.
        let snap2 = crate::reconcile::VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![crate::reconcile::VenueTrade {
                venue_order_id: "manual-1".into(),
                trade_id: "t1".into(),
                token_id: "tok".into(),
                side: Side::Sell,
                size: dec!(10),
                price: dec!(0.6),
                ts_ms: 2_000,
                tx_hash: None,
                maker: Some(false),
            }],
            now_ms: 4_000,
        };
        c.reconcile(snap2).unwrap();
        assert_eq!(
            c.positions().closed_positions().len(),
            1,
            "no duplicate close on a replayed sweep"
        );
        assert_eq!(
            c.ledger().balance(),
            seed - cost - entry_fee + dec!(6) - fee
        );
    }

    fn buy_taker(price: Decimal, size: Decimal) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price,
            size,
            internal_key: "k1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }
}

/// Settlement & redemption end-to-end (issue #175), at the `Core` level: a
/// position that reaches its market's resolution has to become cash without ever
/// breaking the accounting identity. These tests own the acceptance criteria —
/// the settle → `run_accounting_audit` → ok proof, the idempotency proof, and the
/// loud-failure proof.
#[cfg(test)]
mod settlement_service_tests {
    use super::*;
    use crate::model::{FillPolicy, OrderStatus, Side};
    use rust_decimal_macros::dec;

    pub(super) fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bk-settle-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A dry core whose market expires 1s after the entry round and whose durable
    /// logs (positions, trades, settlement journal) all land in `dir`.
    pub(super) fn settling_core(dir: &std::path::Path, balance: Decimal) -> Core {
        let path = |name: &str| Some(dir.join(name).to_string_lossy().to_string());
        let mut c = Core::new(CoreConfig {
            mode: Mode::Dry,
            round_duration_sec: 1,
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: balance,
            position_log_path: path("positions.jsonl"),
            trade_log_path: path("trades.jsonl"),
            settlement_log_path: path("settlements.jsonl"),
            ..Default::default()
        });
        c.set_balance(balance);
        c
    }

    fn entry_order(price: Decimal, size: Decimal) -> OrderRequest {
        OrderRequest {
            token_id: "tok".into(),
            condition_id: "cond".into(),
            side: Side::Buy,
            mode: FillPolicy::Taker,
            price,
            size,
            internal_key: "k1".into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    /// Open a 5-share position at 0.40 (taker: cost 2.00 + 0.036 fee) on a market
    /// that expires at t=2000ms, and take the audit anchor with it open.
    pub(super) fn open_and_anchor(c: &mut Core, balance: Decimal) {
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
        let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
        assert_eq!(status, OrderStatus::Filled);
        assert_eq!(c.positions().open_positions().len(), 1);
        assert_eq!(c.positions().open_positions()[0].expires_at_ms, 2_000);
        // The book moves to 0.95/0.96 before expiry: the market's own last mid is
        // what the dry resolution pays on (> 0.5 wins, a full 1.00/share).
        c.book_snapshot(
            "tok",
            vec![(dec!(0.95), dec!(100))],
            vec![(dec!(0.96), dec!(100))],
            1_998,
        );
        let anchor = c.run_accounting_audit(1_999);
        assert!(anchor.ok, "anchor audit: {}", anchor.summary());
        assert_eq!(c.ledger().balance(), balance - dec!(2.036));
    }

    /// The resolution the venue would report: the held token pays 1.00/share.
    pub(super) fn winning_resolution() -> blitzkrieg_market_api::MarketResolution {
        blitzkrieg_market_api::MarketResolution {
            condition_id: "cond".into(),
            resolved: true,
            payouts: vec![("tok".to_string(), Decimal::ONE)],
            neg_risk: false,
            source: "test".into(),
        }
    }

    fn settlement_json(c: &Core) -> serde_json::Value {
        c.engine_stats_at(3_000)["settlement"].clone()
    }

    /// Acceptance: settle → `run_accounting_audit` → ok, with the payout held as a
    /// receivable (not cash) until the redemption confirms.
    #[test]
    fn settlement_keeps_the_accounting_identity_true() {
        let dir = scratch("identity");
        let mut c = settling_core(&dir, dec!(10));
        open_and_anchor(&mut c, dec!(10));
        let balance_before = c.ledger().balance();

        c.on_market_resolution(winning_resolution(), 2_001);

        // The position is closed at its redemption value, reason `settlement`.
        assert!(c.positions().open_positions().is_empty());
        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.exit_reason, ExitReason::Settlement);
        assert_eq!(closed.exit_price, Decimal::ONE);
        assert_eq!(
            closed.net_pnl_usd,
            dec!(2.964),
            "payout 5 − cost 2 − entry fee 0.036"
        );
        let record =
            serde_json::to_value(crate::trade_db::TradeRecord::from_closed(closed)).unwrap();
        assert_eq!(record["exitReason"], "settlement");
        assert_eq!(record["netPnlUsd"], serde_json::json!(2.964));

        // Cash did NOT move: the payout is an account receivable (the wallet
        // holds conditional tokens, not USDC, until the redeem is mined).
        assert_eq!(c.ledger().balance(), balance_before);
        let stats = settlement_json(&c);
        assert_eq!(stats["receivableUsd"], serde_json::json!(5.0));
        assert_eq!(stats["settledPositions"], serde_json::json!(1));
        assert_eq!(stats["pendingRedemptions"], serde_json::json!(1));
        assert_eq!(
            stats["trackedMarkets"],
            serde_json::json!(0),
            "nothing left to watch"
        );

        // The audit is green with the receivable on the books: `total` explains
        // the settlement exactly (Δrealized 2.964 + Δspent 2.036 = 5.0).
        let report = c.run_accounting_audit(2_002);
        assert!(report.ok, "audit after settlement: {}", report.summary());

        // The claim the venue has to execute.
        let due = c.take_pending_redemptions(2_003);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "cond");
        assert_eq!(due[0].condition_id, "cond");
        assert_eq!(due[0].outcome_shares, vec![dec!(5), dec!(0)]);
        assert_eq!(due[0].expected_payout_usd, dec!(5));
        assert!(!due[0].neg_risk);
        assert!(
            c.take_pending_redemptions(2_003).is_empty(),
            "not re-sent in flight"
        );

        // The redemption confirms: receivable → cash, and `total` does not move.
        c.on_redemption_result(
            blitzkrieg_market_api::RedemptionResult {
                id: "cond".into(),
                condition_id: "cond".into(),
                tx_hash: Some("0xabc".into()),
                block_number: Some(42),
                failure: None,
                at_ms: 2_004,
            },
            2_004,
        );
        assert_eq!(c.ledger().balance(), balance_before + dec!(5));
        let stats = settlement_json(&c);
        assert_eq!(stats["receivableUsd"], serde_json::json!(0.0));
        assert_eq!(stats["pendingRedemptions"], serde_json::json!(0));
        assert_eq!(stats["redeemedClaims"], serde_json::json!(1));
        let report = c.run_accounting_audit(2_005);
        assert!(report.ok, "audit after redemption: {}", report.summary());
        assert_eq!(
            c.engine_stats_at(2_005)["accountingAudit"]["receivableUsd"],
            serde_json::json!(0.0)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Idempotency: a re-delivered resolution (the venue's own retry, a restart,
    /// a duplicated answer) must not book, close or pay the same position twice.
    #[test]
    fn a_redelivered_resolution_does_not_settle_twice() {
        let dir = scratch("idempotent");
        let mut c = settling_core(&dir, dec!(10));
        open_and_anchor(&mut c, dec!(10));

        c.on_market_resolution(winning_resolution(), 2_001);
        // The same answer again, and a second query round's answer as well.
        c.on_market_resolution(winning_resolution(), 2_002);
        c.on_market_resolution(winning_resolution(), 2_003);

        assert_eq!(c.positions().closed_positions().len(), 1, "one close only");
        let trades = c.trade_summary();
        assert_eq!(trades["totalTrades"], serde_json::json!(1));
        assert_eq!(trades["totalNetPnl"], serde_json::json!(2.964));
        let stats = settlement_json(&c);
        assert_eq!(
            stats["receivableUsd"],
            serde_json::json!(5.0),
            "paid once, not 15"
        );
        assert_eq!(stats["pendingRedemptions"], serde_json::json!(1));
        assert!(c.run_accounting_audit(2_004).ok);

        // The same resolution after the claim was redeemed: still nothing new.
        let due = c.take_pending_redemptions(2_005);
        c.on_redemption_result(
            blitzkrieg_market_api::RedemptionResult {
                id: due[0].id.clone(),
                condition_id: "cond".into(),
                tx_hash: Some("0xabc".into()),
                block_number: Some(42),
                failure: None,
                at_ms: 2_006,
            },
            2_006,
        );
        c.on_market_resolution(winning_resolution(), 2_007);
        assert_eq!(c.ledger().balance(), dec!(10) - dec!(2.036) + dec!(5));
        assert_eq!(c.trade_summary()["totalTrades"], serde_json::json!(1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A redemption failure is loud, credited with nothing, retried on the
    /// book's backoff, and never blocks the next entry.
    #[test]
    fn a_failed_redemption_is_loud_and_retried() {
        let dir = scratch("failure");
        let mut c = settling_core(&dir, dec!(10));
        open_and_anchor(&mut c, dec!(10));
        c.on_market_resolution(winning_resolution(), 2_001);
        let balance_before = c.ledger().balance();

        let due = c.take_pending_redemptions(2_002);
        let id = due[0].id.clone();
        let mut errors = Vec::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        c.set_event_sink(tx);
        c.on_redemption_result(
            blitzkrieg_market_api::RedemptionResult {
                id: id.clone(),
                condition_id: "cond".into(),
                tx_hash: None,
                block_number: None,
                failure: Some(blitzkrieg_market_api::RedemptionFailure {
                    message: "nonce too low".into(),
                    manual: false,
                }),
                at_ms: 2_003,
            },
            2_003,
        );
        while let Ok(ev) = rx.try_recv() {
            if let Event::Error { error } = ev {
                errors.push(error.message);
            }
        }
        assert_eq!(errors.len(), 1, "a failed redemption must be loud");
        assert!(errors[0].contains("nonce too low"), "{}", errors[0]);
        // Nothing was credited, and the money is still owed to us.
        assert_eq!(c.ledger().balance(), balance_before);
        let stats = settlement_json(&c);
        assert_eq!(stats["receivableUsd"], serde_json::json!(5.0));
        assert_eq!(stats["retryableRedemptions"], serde_json::json!(1));
        assert_eq!(stats["lastError"]["message"], "nonce too low");
        assert!(
            c.run_accounting_audit(2_004).ok,
            "a failed redeem is not a books failure"
        );

        // Backoff: not re-attempted immediately, attempted again once armed.
        assert!(c.take_pending_redemptions(2_005).is_empty());
        let retry_at = c.settlement.claim(&id).unwrap().next_attempt_ms;
        let due = c.take_pending_redemptions(retry_at);
        assert_eq!(due.len(), 1, "the retry is armed");

        // A failed redemption never blocks a new entry (a different asset, so
        // the settlement's exit cooldown on this one does not mask the result).
        c.book_snapshot("tok2", vec![], vec![(dec!(0.40), dec!(100))], 2_006);
        let order = OrderRequest {
            token_id: "tok2".into(),
            condition_id: "cond2".into(),
            asset: "ETH".into(),
            ..entry_order(dec!(0.40), dec!(2))
        };
        let (_id, status) = c.place(order, 0, 2_006).unwrap();
        assert_eq!(status, OrderStatus::Filled, "entries keep working");

        // The retry lands: exactly one credit.
        c.on_redemption_result(
            blitzkrieg_market_api::RedemptionResult {
                id,
                condition_id: "cond".into(),
                tx_hash: Some("0xdef".into()),
                block_number: Some(43),
                failure: None,
                at_ms: retry_at + 1,
            },
            retry_at + 1,
        );
        assert_eq!(
            c.ledger().balance(),
            balance_before + dec!(5) - dec!(0.8) - dec!(0.0144)
        );
        // A duplicate confirmation for an already-redeemed claim credits nothing.
        c.on_redemption_result(
            blitzkrieg_market_api::RedemptionResult {
                id: "cond".into(),
                condition_id: "cond".into(),
                tx_hash: Some("0xdef".into()),
                block_number: Some(43),
                failure: None,
                at_ms: retry_at + 2,
            },
            retry_at + 2,
        );
        assert_eq!(
            c.ledger().balance(),
            balance_before + dec!(5) - dec!(0.8) - dec!(0.0144)
        );
        assert!(c.run_accounting_audit(retry_at + 3).ok, "audit stays green");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manual verdict (the signer cannot move the positions) is reported, not
    /// retried forever.
    #[test]
    fn a_manual_redemption_verdict_stops_the_retries() {
        let dir = scratch("manual");
        let mut c = settling_core(&dir, dec!(10));
        open_and_anchor(&mut c, dec!(10));
        c.on_market_resolution(winning_resolution(), 2_001);
        let due = c.take_pending_redemptions(2_002);
        c.on_redemption_result(
            blitzkrieg_market_api::RedemptionResult {
                id: due[0].id.clone(),
                condition_id: "cond".into(),
                tx_hash: None,
                block_number: None,
                failure: Some(blitzkrieg_market_api::RedemptionFailure {
                    message: "positions are held by another address".into(),
                    manual: true,
                }),
                at_ms: 2_003,
            },
            2_003,
        );
        let stats = settlement_json(&c);
        assert_eq!(stats["manualRedemptions"], serde_json::json!(1));
        assert_eq!(stats["retryableRedemptions"], serde_json::json!(0));
        assert!(
            c.take_pending_redemptions(i64::MAX / 2).is_empty(),
            "no automatic attempt after a manual verdict"
        );
        assert_eq!(
            stats["receivableUsd"],
            serde_json::json!(5.0),
            "still owed to us"
        );
        assert!(c.run_accounting_audit(2_004).ok);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A worthless leg is closed at 0 (the loss was already cash) and books no
    /// claim: there is nothing on-chain to redeem.
    #[test]
    fn a_losing_position_settles_to_zero_without_a_claim() {
        let dir = scratch("loser");
        let mut c = settling_core(&dir, dec!(10));
        c.book_snapshot("tok", vec![], vec![(dec!(0.40), dec!(100))], 1);
        let (_id, status) = c.place(entry_order(dec!(0.40), dec!(5)), 0, 1).unwrap();
        assert_eq!(status, OrderStatus::Filled);
        assert!(c.run_accounting_audit(1_999).ok);

        c.on_market_resolution(
            blitzkrieg_market_api::MarketResolution {
                condition_id: "cond".into(),
                resolved: true,
                payouts: vec![("tok".to_string(), Decimal::ZERO)],
                neg_risk: false,
                source: "test".into(),
            },
            2_001,
        );
        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.exit_reason, ExitReason::Settlement);
        assert_eq!(
            closed.net_pnl_usd,
            dec!(-2.036),
            "cost + entry fee, nothing back"
        );
        let stats = settlement_json(&c);
        assert_eq!(stats["receivableUsd"], serde_json::json!(0.0));
        assert_eq!(stats["pendingRedemptions"], serde_json::json!(0));
        assert!(
            c.take_pending_redemptions(2_002).is_empty(),
            "nothing to redeem"
        );
        let report = c.run_accounting_audit(2_003);
        assert!(report.ok, "audit after a loss: {}", report.summary());
        // Total cash is short by exactly the loss: the money is accounted for,
        // not held hostage by a position nobody can convert.
        assert_eq!(c.ledger().balance(), dec!(10) - dec!(2.036));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A resolution that does not price a held token leaves the position open and
    /// says so: inventing a payout would be worse than the drift it fixes.
    #[test]
    fn an_unpriced_token_is_reported_and_left_open() {
        let dir = scratch("unpriced");
        let mut c = settling_core(&dir, dec!(10));
        open_and_anchor(&mut c, dec!(10));
        let (tx, mut rx) = mpsc::unbounded_channel();
        c.set_event_sink(tx);
        c.on_market_resolution(
            blitzkrieg_market_api::MarketResolution {
                condition_id: "cond".into(),
                resolved: true,
                payouts: vec![("someone-elses-token".to_string(), Decimal::ONE)],
                neg_risk: false,
                source: "test".into(),
            },
            2_001,
        );
        let mut errors = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::Error { error } = ev {
                errors.push(error.message);
            }
        }
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("prices no outcome"), "{}", errors[0]);
        assert_eq!(c.positions().open_positions().len(), 1, "still open");
        assert_eq!(settlement_json(&c)["receivableUsd"], serde_json::json!(0.0));
        assert!(
            c.run_accounting_audit(2_002).ok,
            "an open position is not drift"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dry mode answers its own queries on the tick: the whole path (query →
    /// resolution → close → receivable → simulated redemption) runs without a
    /// chain, and the ledger ends up holding the payout as cash.
    #[test]
    fn dry_mode_settles_on_its_own_tick_without_a_chain() {
        let dir = scratch("dry-tick");
        let mut c = settling_core(&dir, dec!(10));
        open_and_anchor(&mut c, dec!(10));
        let balance_before = c.ledger().balance();

        c.tick(2_001).unwrap();

        assert!(c.positions().open_positions().is_empty());
        assert_eq!(
            c.positions().closed_positions()[0].exit_reason,
            ExitReason::Settlement
        );
        // The simulated redemption confirmed, so the payout is cash.
        assert_eq!(c.ledger().balance(), balance_before + dec!(5));
        let stats = settlement_json(&c);
        assert_eq!(stats["receivableUsd"], serde_json::json!(0.0));
        assert_eq!(stats["redeemedClaims"], serde_json::json!(1));
        assert_eq!(stats["settledPositions"], serde_json::json!(1));
        assert!(c.run_accounting_audit(2_002).ok);
        // The journal records the dry redemption as dry, never as a real tx.
        let journal = std::fs::read_to_string(dir.join("settlements.jsonl")).unwrap();
        assert!(journal.contains("dry-simulated"), "{journal}");
        assert!(journal.contains("\"kind\":\"closed\""), "{journal}");

        // Nothing settles twice on later ticks.
        c.tick(2_500).unwrap();
        assert_eq!(c.ledger().balance(), balance_before + dec!(5));
        assert_eq!(c.trade_summary()["totalTrades"], serde_json::json!(1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A crash between the settlement commit and the close is repaired on the
    /// next start, exactly once.
    #[test]
    fn an_interrupted_settlement_recovers_once_on_restart() {
        let dir = scratch("recover");
        let balance = dec!(10);
        let mut c = settling_core(&dir, balance);
        open_and_anchor(&mut c, balance);
        let pos = c.positions().open_positions()[0].clone();
        c.persist_positions();
        drop(c); // the process dies here

        // What the crash left behind: the settlement is durable, the close is not.
        {
            let journal = dir.join("settlements.jsonl");
            let mut book = crate::settlement::SettlementBook::new(Some(&journal));
            let booking =
                crate::settlement::booking_for(&pos, &winning_resolution()).expect("bookable");
            assert!(book.book(&booking, &winning_resolution(), 2_001).is_some());
            assert_eq!(book.receivable_usd(), dec!(5));
        }

        // Restart: the position log still holds it, and the settlement stands.
        let mut c = settling_core(&dir, balance - dec!(2.036));
        assert_eq!(c.restore_positions(), 1);
        assert!(
            c.positions().open_positions().is_empty(),
            "closed by recovery"
        );
        assert_eq!(
            c.positions().closed_positions()[0].exit_reason,
            ExitReason::Settlement
        );
        assert_eq!(c.trade_summary()["totalTrades"], serde_json::json!(1));
        assert_eq!(c.trade_summary()["totalNetPnl"], serde_json::json!(2.964));
        let stats = settlement_json(&c);
        assert_eq!(stats["receivableUsd"], serde_json::json!(5.0));
        let report = c.run_accounting_audit(3_001);
        assert!(report.ok, "audit after recovery: {}", report.summary());

        // A second restart must not close or record it again.
        let mut c = settling_core(&dir, balance - dec!(2.036));
        assert_eq!(c.restore_positions(), 0, "the position log is empty now");
        assert_eq!(c.trade_summary()["totalTrades"], serde_json::json!(1));
        assert_eq!(settlement_json(&c)["receivableUsd"], serde_json::json!(5.0));
        c.on_market_resolution(winning_resolution(), 3_002);
        assert_eq!(c.trade_summary()["totalTrades"], serde_json::json!(1));
        assert_eq!(settlement_json(&c)["receivableUsd"], serde_json::json!(5.0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The panel sees settled-but-unredeemed money through the existing
    /// `engine_stats` outlet, with no new event type.
    #[test]
    fn the_panel_can_see_a_settled_but_unredeemed_claim() {
        let dir = scratch("panel");
        let mut c = settling_core(&dir, dec!(10));
        open_and_anchor(&mut c, dec!(10));
        c.on_market_resolution(winning_resolution(), 2_001);

        let stats = settlement_json(&c);
        assert_eq!(stats["claims"][0]["id"], "cond");
        assert_eq!(stats["claims"][0]["payoutUsd"], serde_json::json!(5.0));
        assert_eq!(stats["claims"][0]["positions"], serde_json::json!(1));
        assert_eq!(stats["claims"][0]["attempts"], serde_json::json!(0));
        assert_eq!(stats["claims"][0]["manual"], serde_json::json!(false));
        assert_eq!(stats["blindSinceMs"], serde_json::Value::Null);
        // And the audit view names the receivable beside the balance it is not part of.
        let audit = c.engine_stats_at(2_002)["accountingAudit"].clone();
        assert_eq!(audit["receivableUsd"], serde_json::json!(5.0));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Pending-close exit reasons (issue #190). The table is a ledger of IN-FLIGHT
/// closes, not a history: an entry must not outlive the position it belongs to,
/// the table must never grow without bound, and a later position on the same
/// token must never inherit a dead reason.
#[cfg(test)]
mod exit_reason_table_tests {
    use super::settlement_service_tests::{
        open_and_anchor, scratch, settling_core, winning_resolution,
    };
    use super::*;
    use crate::model::{FillPolicy, OrderStatus, Side};
    use crate::position::OpenParams;
    use rust_decimal_macros::dec;

    /// The fixture's config, exposed so a test can vary exactly one field.
    fn cfg() -> CoreConfig {
        CoreConfig {
            mode: Mode::Dry,
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: dec!(1000),
            auto_exits_enabled: false,
            ..Default::default()
        }
    }

    /// Dry core, seed 1000, automated exits OFF: the fixture drives every close
    /// explicitly so the path under test is the only one that runs.
    fn core() -> Core {
        let mut c = Core::new(cfg());
        c.set_balance(dec!(1000));
        c
    }

    fn order(side: Side, price: Decimal, size: Decimal, token: &str, key: &str) -> OrderRequest {
        OrderRequest {
            token_id: token.into(),
            condition_id: "cond".into(),
            side,
            mode: FillPolicy::Taker,
            price,
            size,
            internal_key: key.into(),
            strategy: "spread_arb".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    /// Open a 10-share position on `token` at 0.40 off a resting ask.
    fn open_position(c: &mut Core, token: &str, at_ms: i64) {
        c.book_snapshot(token, vec![], vec![(dec!(0.40), dec!(500))], at_ms - 1);
        let (_, status) = c
            .place(
                order(Side::Buy, dec!(0.40), dec!(10), token, "entry"),
                0,
                at_ms,
            )
            .unwrap();
        assert_eq!(status, OrderStatus::Filled);
    }

    /// Close the position on `token` the way a venue-side manual sell would: a
    /// SELL fill the OME never issued, folded in by the reconcile sweep. This
    /// path does NOT go through `on_position_closed`.
    fn venue_close(c: &mut Core, token: &str, at_ms: i64) {
        let snap = crate::reconcile::VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![crate::reconcile::VenueTrade {
                venue_order_id: format!("manual-{token}"),
                trade_id: format!("t-{token}"),
                token_id: token.into(),
                side: Side::Sell,
                size: dec!(10),
                price: dec!(0.60),
                ts_ms: at_ms,
                tx_hash: None,
                maker: Some(false),
            }],
            now_ms: at_ms + 100,
        };
        c.reconcile(snap).unwrap();
        assert!(
            c.positions()
                .open_positions()
                .iter()
                .all(|p| p.token_id != token),
            "the venue close must leave no open position on {token}"
        );
    }

    /// A close retires the reason that was pending for it, whichever path lands
    /// the close — including the reconciled venue close, which does not funnel
    /// through `on_position_closed`.
    #[test]
    fn a_close_retires_the_pending_exit_reason() {
        let mut c = core();
        open_position(&mut c, "tok", 1_000);
        // The exit policy decided to leave (a stop); the reason stays pending
        // until the closing sell fills.
        c.note_exit_reason("tok", ExitReason::StopLoss);
        assert_eq!(c.exit_reasons.len(), 1);
        assert_eq!(c.exit_reasons_view()["tracked"], serde_json::json!(1));
        assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(0));

        venue_close(&mut c, "tok", 1_100);

        assert!(
            c.exit_reasons.is_empty(),
            "a closed position must not leave its exit reason behind: {:?}",
            c.exit_reasons
        );
        assert_eq!(c.exit_reasons_view()["tracked"], serde_json::json!(0));
        assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(0));
    }

    /// The other cleanup site, and the one every internal close funnels through:
    /// a position that settles locally never had its exit fill, so
    /// `take_exit_reason` never ran for it and the retirement inside
    /// `on_position_closed` is the only thing that removes the entry.
    #[test]
    fn a_local_settlement_retires_the_pending_exit_reason() {
        let dir = scratch("settle-reason");
        let mut c = settling_core(&dir, dec!(10));
        open_and_anchor(&mut c, dec!(10));
        // A stop was decided, its exit never filled, and the market resolved
        // under it: the reason must die with the position it belonged to.
        c.note_exit_reason("tok", ExitReason::StopLoss);
        assert_eq!(c.exit_reasons_view()["tracked"], serde_json::json!(1));

        c.on_market_resolution(winning_resolution(), 2_001);

        assert_eq!(
            c.positions().closed_positions()[0].exit_reason,
            ExitReason::Settlement,
            "the settlement, not the dead stop, is what closed this position"
        );
        assert!(
            c.exit_reasons.is_empty(),
            "a settled position must not leave its exit reason behind: {:?}",
            c.exit_reasons
        );
        assert_eq!(c.exit_reasons_view()["tracked"], serde_json::json!(0));
        assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mis-attribution this issue is about: a reason recorded for an exit
    /// that never filled must not be applied to the NEXT position on the same
    /// token. The internal exit-fill path is the one that reads the table, so
    /// the assertion is on the recorded close reason.
    #[test]
    fn a_later_position_never_inherits_a_dead_exit_reason() {
        let mut c = core();
        open_position(&mut c, "tok", 1_000);
        c.note_exit_reason("tok", ExitReason::StopLoss); // an exit that never filled
        venue_close(&mut c, "tok", 1_100); // the position dies first

        // The next round reuses the token (past the exit cooldown): a new
        // position, with no reason of its own.
        open_position(&mut c, "tok", 200_000);
        c.book_snapshot("tok", vec![(dec!(0.55), dec!(500))], vec![], 200_100);
        let (_, status) = c
            .place(
                order(Side::Sell, dec!(0.55), dec!(10), "tok", "close-2"),
                0,
                200_100,
            )
            .unwrap();
        assert_eq!(status, OrderStatus::Filled);

        let closed = c.positions().closed_positions();
        assert_eq!(closed.len(), 2);
        assert_eq!(
            closed[1].exit_reason,
            ExitReason::Manual,
            "the second close inherited the first position's dead stop reason"
        );
    }

    /// The table is bounded, and the bound evicts the entries whose position is
    /// already gone before it touches a live one.
    #[test]
    fn the_table_is_bounded_and_evicts_dead_entries_first() {
        let mut c = core();
        open_position(&mut c, "live", 1_000);
        c.note_exit_reason("live", ExitReason::TakeProfit);
        for i in 0..EXIT_REASON_TABLE_MAX {
            c.note_exit_reason(&format!("dead-{i}"), ExitReason::StopLoss);
        }
        assert_eq!(
            c.exit_reasons.len(),
            EXIT_REASON_TABLE_MAX,
            "the table must never exceed its bound"
        );
        assert_eq!(
            c.exit_reasons.get("live"),
            Some(&ExitReason::TakeProfit),
            "a live position's reason must survive while dead entries remain"
        );
        assert_eq!(c.exit_reasons_view()["evictedTotal"], serde_json::json!(1));
        assert_eq!(c.exit_reason_order.len(), c.exit_reasons.len());
    }

    /// …and when EVERY entry still has an open position, the oldest is the one
    /// dropped (the table cannot be allowed to exceed the bound either way).
    #[test]
    fn the_oldest_entry_is_evicted_when_all_of_them_are_live() {
        let mut c = core();
        for i in 0..=EXIT_REASON_TABLE_MAX {
            let token = format!("tok-{i}");
            c.positions.open(
                OpenParams {
                    strategy: "spread_arb".into(),
                    asset: "BTC".into(),
                    direction: crate::model::SignalDirection::Up,
                    token_id: token.clone(),
                    condition_id: "cond".into(),
                    entry_price: dec!(0.40),
                    expires_at_ms: 9_999,
                    was_maker: false,
                    target_exit_price: None,
                },
                1_000,
            );
            c.note_exit_reason(&token, ExitReason::TakeProfit);
        }
        assert_eq!(c.exit_reasons.len(), EXIT_REASON_TABLE_MAX);
        assert!(
            !c.exit_reasons.contains_key("tok-0"),
            "the oldest entry is the one evicted"
        );
        assert!(
            c.exit_reasons
                .contains_key(&format!("tok-{EXIT_REASON_TABLE_MAX}"))
        );
        assert_eq!(c.exit_reason_order.len(), c.exit_reasons.len());
    }

    /// The round rollover is the backstop: an entry whose position is gone by
    /// then can no longer belong to anything live, so it is swept — which is
    /// what keeps `orphaned` at 0 for the panel.
    #[test]
    fn a_round_rollover_sweeps_reasons_whose_position_is_gone() {
        let mut c = core();
        open_position(&mut c, "live", 1_000);
        c.note_exit_reason("live", ExitReason::TakeProfit);
        // An exit submitted for a position that closed in between: the entry is
        // real, its position is not.
        c.note_exit_reason("ghost", ExitReason::StopLoss);
        assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(1));

        c.engine_on_data(
            crate::engine::DataEvent::RoundMarkets {
                markets: vec![],
                now_ms: 5_000,
            },
            5_000,
        );

        assert!(
            !c.exit_reasons.contains_key("ghost"),
            "a round rollover must sweep a reason whose position is gone"
        );
        assert_eq!(
            c.exit_reasons.get("live"),
            Some(&ExitReason::TakeProfit),
            "the live position's reason is not swept"
        );
        assert_eq!(c.exit_reasons_view()["orphaned"], serde_json::json!(0));
        assert_eq!(c.exit_reasons_view()["sweptTotal"], serde_json::json!(1));
    }

    /// Issue #205 requirement 4: the orderbook-staleness budget in force is
    /// readable from `engine.stats`, so a panel or a harness never has to parse
    /// the boot log. Built through `install_engine` — the production
    /// `CoreConfig -> EngineConfig` mapping — so this covers the whole chain and
    /// reports the value the engine *enforces*, not just the field that was set.
    #[test]
    fn engine_stats_reports_the_orderbook_freshness_budget() {
        let view = |budget_ms: i64| -> serde_json::Value {
            let mut c = Core::new(CoreConfig {
                max_orderbook_stale_ms: budget_ms,
                ..cfg()
            });
            let engine_cfg = c.config().engine_config();
            c.enable_engine(crate::engine::Engine::new(engine_cfg));
            c.engine_stats_at(1_000)["orderbookFreshness"].clone()
        };

        let widened = view(25_000);
        assert_eq!(widened["maxStaleMs"], serde_json::json!(25_000));
        assert_eq!(widened["checkEnabled"], serde_json::json!(true));
        // The shipped default is the historic hardcoded value: #205 made the knob
        // configurable without moving it.
        let shipped = view(crate::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS);
        assert_eq!(shipped["maxStaleMs"], serde_json::json!(8_000));
        assert_eq!(shipped["checkEnabled"], serde_json::json!(true));
        // 0 = the operator switched the gate off. Reported as such rather than as
        // a "0ms budget", which would read as "refuse everything".
        let off = view(0);
        assert_eq!(off["maxStaleMs"], serde_json::json!(0));
        assert_eq!(off["checkEnabled"], serde_json::json!(false));
    }
}

/// ── Audit F2/F10/F4: reverse acceptance tests ───────────────────────────────
///
/// One test per audit finding, each driving the exact counterexample from
/// `BlitzkriegBot_核心逻辑审计.md` through the REAL production path and
/// asserting the account does NOT move the way the bug moved it:
///
///  - F2: a counterparty's 100-share SELL must never book against a 10-share
///    inventory — the clamp truncates size AND proceeds; nothing attributable
///    to inventory is never booked at all.
///  - F4: a fully-closing SELL that later arrives FAILED must restore the
///    position inventory and reverse the profit (TradeDb, strategy PnL, daily
///    PnL, breaker) instead of leaving phantom profit behind.
///  - F10: a manual (external) full close at a LOSS must reach the SAME close
///    ledger the engine's own fills do — TradeDb record, strategy net PnL,
///    win/loss counts and the PositionClosed event.
#[cfg(test)]
mod audit_fix_tests {
    use super::*;
    use rust_decimal_macros::dec;

    const SEED: Decimal = dec!(1000);
    const STRATEGY: &str = "acc";

    fn core_with_trade_log(path: Option<std::path::PathBuf>) -> Core {
        let mut cfg = CoreConfig {
            risk: RiskConfig {
                max_order_notional: dec!(100),
                ..Default::default()
            },
            dry_seed_balance: SEED,
            auto_exits_enabled: false,
            ..Default::default()
        };
        cfg.trade_log_path = path.map(|p| p.to_string_lossy().into_owned());
        let mut c = Core::new(cfg);
        c.set_balance(SEED);
        c
    }

    fn core() -> Core {
        core_with_trade_log(None)
    }

    fn tmp_trade_log(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // TradeDb keeps summary.json beside the log, so each fixture needs its
        // own DIRECTORY — a bare file in the shared temp root would make every
        // test load (and persist onto) the same summary.json.
        let dir = std::env::temp_dir().join(format!("bk-audit-{tag}-{nanos}"));
        dir.join("trades.jsonl")
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
            strategy: STRATEGY.into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    fn fill(
        order_id: &str,
        trade: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        status: FillStatus,
    ) -> Fill {
        Fill {
            order_id: order_id.into(),
            trade_id: Some(trade.into()),
            token_id: "tok".into(),
            side,
            price,
            size,
            status,
            ts_ms: 0,
            tx_hash: None,
            // Maker-mode orders with no venue report → the policy decides, and
            // these fixtures are exactly what the policy would produce.
            maker: None,
        }
    }

    /// Open a maker position of `size` shares at `price` through the REAL
    /// live fill path (register → venue ack → user-WS fill).
    fn open_maker_position(c: &mut Core, price: Decimal, size: Decimal, now: i64) {
        let (id, _) = c
            .place_pending(req(Side::Buy, FillPolicy::Maker, price, size, "entry"), now)
            .unwrap();
        c.confirm_live(&id, now).unwrap();
        c.ingest_fill(
            fill(&id, "e1", Side::Buy, price, size, FillStatus::Confirmed),
            now + 1,
        )
        .unwrap();
    }

    fn external_sell(size: Decimal, price: Decimal, ts_ms: i64) -> crate::reconcile::VenueSnapshot {
        crate::reconcile::VenueSnapshot {
            open_order_ids: vec![],
            trades: vec![crate::reconcile::VenueTrade {
                venue_order_id: "0xmanual".into(),
                trade_id: "t-manual".into(),
                token_id: "tok".into(),
                side: Side::Sell,
                size,
                price,
                ts_ms,
                tx_hash: None,
                maker: Some(true),
            }],
            now_ms: ts_ms + 1_000,
        }
    }

    /// F2, counterexample: our maker BUY bought 10 shares @ 0.40 (cost 4);
    /// the counterparty taker SOLD 100 shares in that same match, the other 90
    /// bought by other makers. A venue SELL leg that cannot be attributed to
    /// our inventory must not mint cash.
    ///
    /// (a) a SELL with NO open position for the token: booked nowhere;
    /// (b) an oversized SELL against 10 held shares: size AND proceeds are
    ///     truncated to the attributable inventory — proceeds 4, net 0, cash
    ///     back to seed — NOT the audit's 40 proceeds / +36 net (882%).
    #[test]
    fn external_sell_beyond_attributable_inventory_never_mints_cash() {
        let mut c = core();

        // (a) no position: the SELL matches nothing and must change nothing.
        c.reconcile(external_sell(dec!(100), dec!(0.40), 1_000))
            .unwrap();
        assert_eq!(c.positions().open_positions().len(), 0);
        assert_eq!(c.positions().closed_positions().len(), 0);
        assert_eq!(c.ledger().balance(), SEED, "no position, no booking");
        assert_eq!(c.positions().daily_pnl(), Decimal::ZERO);

        // (b) hold 10 shares @ 0.40 (maker entry, cost 4, no fee).
        open_maker_position(&mut c, dec!(0.40), dec!(10), 4_000);
        assert_eq!(c.positions().open_positions()[0].shares, dec!(10));
        assert_eq!(c.positions().open_positions()[0].cost_usd, dec!(4));
        assert_eq!(c.ledger().balance(), SEED - dec!(4));
        let bal_after_entry = c.ledger().balance();

        // The counterparty's 100-share sell: clamped to the 10 shares we hold.
        c.reconcile(external_sell(dec!(100), dec!(0.40), 6_000))
            .unwrap();

        let closed = &c.positions().closed_positions();
        assert_eq!(closed.len(), 1, "the held shares were sold off");
        assert_eq!(closed[0].shares, dec!(10));
        // Proceeds truncated to inventory value: 10 × 0.40 = 4, not 100 × 0.40.
        assert_eq!(closed[0].pnl_usd, dec!(4) - dec!(4), "proceeds capped at 4");
        assert_eq!(
            closed[0].net_pnl_usd,
            Decimal::ZERO,
            "no phantom profit from the counterparty's size"
        );
        // Cash: entry cost 4, proceeds 4 → back to seed. NOT seed + 36.
        assert_eq!(
            c.ledger().balance(),
            bal_after_entry + dec!(4),
            "the ledger received exactly the attributable proceeds"
        );
        assert_eq!(c.positions().daily_pnl(), Decimal::ZERO);
        // Strategy accounting: a flat close is NOT a +36 win.
        let acc = c.strategy_accounting.get(STRATEGY).expect("accounting row");
        assert_eq!(acc.net_pnl_usd, Decimal::ZERO);
        assert_eq!(acc.wins, 1);
        assert_eq!(acc.losses, 0);
    }

    /// F4, counterexample: maker BUY 10 @ 0.40, maker SELL 10 @ 0.80 booked
    /// +4 realized on MATCHED; the venue then reports the same trade FAILED.
    /// The sale never happened: inventory must come back and every ledger the
    /// close touched must reverse.
    #[test]
    fn failed_status_after_full_close_restores_inventory_and_reverses_profit() {
        let log = tmp_trade_log("f4");
        let mut c = core_with_trade_log(Some(log.clone()));

        open_maker_position(&mut c, dec!(0.40), dec!(10), 1_000);
        let (sid, _) = c
            .place_pending(
                req(Side::Sell, FillPolicy::Maker, dec!(0.80), dec!(10), "exit"),
                2_000,
            )
            .unwrap();
        c.confirm_live(&sid, 2_000).unwrap();
        c.ingest_fill(
            fill(
                &sid,
                "x1",
                Side::Sell,
                dec!(0.80),
                dec!(10),
                FillStatus::Confirmed,
            ),
            3_000,
        )
        .unwrap();

        // The close was fully booked.
        assert!(c.positions().open_positions().is_empty());
        let closed = &c.positions().closed_positions()[0];
        assert_eq!(closed.net_pnl_usd, dec!(4));
        assert_eq!(c.ledger().balance(), SEED + dec!(4));
        assert_eq!(c.positions().daily_pnl(), dec!(4));
        {
            let acc = c.strategy_accounting.get(STRATEGY).expect("accounting row");
            assert_eq!(acc.closed_trades, 1);
            assert_eq!(acc.wins, 1);
            assert_eq!(acc.net_pnl_usd, dec!(4));
        }
        assert_eq!(c.trade_db.as_ref().unwrap().summary().total_trades, 1);
        assert_eq!(c.trade_db.as_ref().unwrap().summary().total_net_pnl, 4.0);

        // FAILED arrives for the same trade → the rollback must unwind it all.
        c.ingest_fill(
            fill(
                &sid,
                "x1",
                Side::Sell,
                dec!(0.80),
                dec!(10),
                FillStatus::Failed,
            ),
            4_000,
        )
        .unwrap();

        // Inventory restored exactly: 10 shares @ 0.40 basis.
        assert_eq!(
            c.positions().open_positions().len(),
            1,
            "the unsold position is back on the books"
        );
        let pos = &c.positions().open_positions()[0];
        assert_eq!(pos.shares, dec!(10));
        assert_eq!(pos.cost_usd, dec!(4));
        assert_eq!(pos.entry_price, dec!(0.40));
        // The phantom close is gone from the realized book.
        assert!(
            c.positions().closed_positions().is_empty(),
            "a trade that never happened must not stay realized"
        );
        // Cash refunded to post-entry; profit reversed.
        assert_eq!(c.ledger().balance(), SEED - dec!(4));
        assert_eq!(c.positions().daily_pnl(), Decimal::ZERO);
        {
            let acc = c.strategy_accounting.get(STRATEGY).expect("accounting row");
            assert_eq!(acc.closed_trades, 0);
            assert_eq!(acc.wins, 0);
            assert_eq!(acc.fees_usd, Decimal::ZERO);
            assert_eq!(acc.net_pnl_usd, Decimal::ZERO);
        }
        // TradeDb: record withdrawn, summary back to zero.
        assert_eq!(c.trade_db.as_ref().unwrap().summary().total_trades, 0);
        assert_eq!(c.trade_db.as_ref().unwrap().summary().total_net_pnl, 0.0);
        assert!(
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "the retracted trade must not remain in the JSONL"
        );

        // E17 cash identity over the whole round trip: cash + open basis
        // equals the seed — nothing leaked in either direction.
        assert_eq!(
            c.ledger().balance() + c.positions().open_positions()[0].cost_usd,
            SEED
        );

        let _ = std::fs::remove_file(&log);
    }

    /// F10, counterexample: the strategy booked an automatic close, then the
    /// operator flat-closed a position manually on the venue at a LOSS. The
    /// loss must reach the SAME close ledger: TradeDb record, strategy net
    /// PnL, win/loss counts and the PositionClosed event — not just cash.
    #[test]
    fn manual_venue_loss_reaches_the_trade_ledger() {
        let log = tmp_trade_log("f10");
        let mut c = core_with_trade_log(Some(log.clone()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        c.set_event_sink(tx);

        // Hold 10 @ 0.60 (cost 6).
        open_maker_position(&mut c, dec!(0.60), dec!(10), 1_000);
        assert_eq!(c.ledger().balance(), SEED - dec!(6));

        // Operator sells flat on the venue at 0.40: a −2 realized LOSS.
        c.reconcile(external_sell(dec!(10), dec!(0.40), 6_000))
            .unwrap();

        let closed = &c.positions().closed_positions();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].exit_reason, ExitReason::Manual);
        assert_eq!(closed[0].net_pnl_usd, dec!(-2));

        // TradeDb received the manual close (audit F10: it used to be missed).
        assert_eq!(c.trade_db.as_ref().unwrap().summary().total_trades, 1);
        assert_eq!(c.trade_db.as_ref().unwrap().summary().losses, 1);
        assert_eq!(c.trade_db.as_ref().unwrap().summary().total_net_pnl, -2.0);

        // Strategy accounting: the loss IS in the strategy's net PnL.
        let acc = c.strategy_accounting.get(STRATEGY).expect("accounting row");
        assert_eq!(acc.closed_trades, 1);
        assert_eq!(acc.losses, 1);
        assert_eq!(acc.wins, 0);
        assert_eq!(acc.net_pnl_usd, dec!(-2));

        // Daily PnL sees it too, and the PositionClosed event fired.
        assert_eq!(c.positions().daily_pnl(), dec!(-2));
        let position_closed = {
            let mut found = false;
            while let Ok(ev) = rx.try_recv() {
                if matches!(ev, Event::PositionClosed { ref net_pnl_usd, .. } if *net_pnl_usd == dec!(-2))
                {
                    found = true;
                }
            }
            found
        };
        assert!(
            position_closed,
            "the manual close must emit PositionClosed like engine closes do"
        );

        // Cash identity: SEED + realized net.
        assert_eq!(c.ledger().balance(), SEED + dec!(-2));

        let _ = std::fs::remove_file(&log);
    }
}
