// #699 — the bin's boot layer, moved here verbatim from `main.rs`:
// the argument surface (`Args`, `parse_args`, config choice, env vars and
// the parse helpers behind them) plus the `--net-check` probe. Declared
// only from `main.rs`, so it stays private to the bin target; items
// referenced from `main()` or from the tests carry `pub(crate)`.
//
// #780 — the file is now a directory: `Args`, the config/env helpers and
// the value macros stay here (the macros via `macros.rs`), `parse_args`
// lives in `args.rs`, and the contiguous flag-match tail it delegates to
// lives in `replay_flags.rs`. Every item keeps its original path through
// the `pub(crate) use` below; the split is a pure code move.

use blitzkrieg_core::model::Mode;
use rust_decimal::Decimal;
use std::str::FromStr;

#[macro_use]
mod macros;
mod args;
mod replay_flags;

pub(crate) use args::parse_args;

pub(crate) struct Args {
    pub(crate) socket: String,
    pub(crate) mode: Mode,
    /// `--readonly`: refuse order egress entirely. Recorded separately from
    /// `mode` because the flag is parsed before the mode precedence is settled —
    /// and it must win over `--mode live` / `DRY_RUN=false`, not be overwritten
    /// by whichever of those was read last.
    pub(crate) readonly: bool,
    pub(crate) tick_ms: u64,
    pub(crate) seed_balance: Decimal,
    pub(crate) max_order_notional: Decimal,
    pub(crate) max_open_notional: Decimal,
    pub(crate) min_shares: Option<Decimal>,
    pub(crate) max_shares: Option<Decimal>,
    /// #202 — the sizing knobs that are relative to the account: `size_pct` is
    /// the per-entry budget as a percentage of cash equity (0 = off, the
    /// absolute `size_usd` path), `max_order_notional_pct` the per-order
    /// notional bound in the same unit (0 = off). Both default to off, so an
    /// unconfigured deployment keeps its exact order sizes.
    pub(crate) size_pct: Decimal,
    pub(crate) max_order_notional_pct: Decimal,
    /// The startup echo for the sizing/notional knobs, printed unconditionally
    /// (same rationale as `daily_loss_echo`: "what bounds one order here?"
    /// must be answerable from the boot log of a run that set no flags).
    pub(crate) sizing_echo: Vec<String>,
    pub(crate) markets: Vec<String>,
    pub(crate) auto_exits: bool,
    pub(crate) max_positions: usize,
    /// #363 — how many positions may be open at once on ONE asset. `None`
    /// (flag absent) = the execution-policy file decides (or 1, the
    /// historical one-per-asset gate); an explicit flag wins over the file.
    pub(crate) max_positions_per_asset: Option<u32>,
    /// #380 — the explicit policy-file override (`--execution-policy <path>`).
    /// `None` = the shipped default, resolved against the install root before
    /// the core is constructed (the resolution and its boot log live in
    /// `resolved_policy_path` below). An explicit path is honoured verbatim.
    pub(crate) execution_policy: Option<String>,
    /// #173 — the daily-loss breaker's budget. `daily_loss_usd` is the absolute
    /// cap (0 = none) and `daily_loss_pct` the cap as a percentage of the day's
    /// opening cash equity (0 = off). The EFFECTIVE cap is the tighter of the
    /// two, and it is echoed at startup whether or not anything configured it:
    /// "the breaker was armed, against what?" must be answerable from the boot
    /// log of a run that set no flags (that silence was half of P0-2).
    pub(crate) daily_loss_usd: Decimal,
    pub(crate) daily_loss_pct: Decimal,
    /// E26 (§4.2): which layer armed the daily-loss budget — the drawdown
    /// bound on `risk.limits` must name the TRUE source (a `--max-daily-loss`
    /// run reads `flag` there, not `default`), and only the resolver knows.
    pub(crate) daily_loss_source: blitzkrieg_core::config::Source,
    /// The startup echo lines for the daily-loss budget, printed unconditionally
    /// (kept out of `config_report`, which lists only settings moved away from
    /// their compiled default).
    pub(crate) daily_loss_echo: Vec<String>,
    pub(crate) engine: bool,
    pub(crate) min_round_age: i64,
    pub(crate) min_time_left: i64,
    pub(crate) trend_confirm: i64,
    pub(crate) trend_floor_ms: i64,
    pub(crate) spread_arb_entry_factor: Option<Decimal>,
    pub(crate) spread_arb_min_obi: Option<Decimal>,
    pub(crate) spread_arb_max_spread_pct: Option<Decimal>,
    pub(crate) spread_arb_dip_max_pct: Option<Decimal>,
    pub(crate) spread_arb_bounce_min_pct: Option<Decimal>,
    pub(crate) spread_arb_bounce_window_sec: Option<i64>,
    /// E17/#264 — the engine-level exit ladder, resolved CLI > `[exit]` in the
    /// config file > the shipped `ExitConfig` value. Exits belong to the kernel
    /// (they must fire for every strategy, including a dylib nobody rebuilt), so
    /// these are `ExitConfig` knobs rather than per-strategy parameters. The
    /// defaults are the compiled ones, unchanged: the flags exist to A/B a
    /// ladder, not to move the shipped one.
    pub(crate) exit_take_profit_pct: Decimal,
    pub(crate) exit_stop_loss_pct: Decimal,
    pub(crate) exit_trailing_min_high_pct: Decimal,
    pub(crate) exit_min_trail_pct: Decimal,
    /// The force-exit deadline in seconds-left. The compiled default is 0 = OFF
    /// (calibration: the deadline amputated rounds that went on to pay); a
    /// non-zero value re-arms it.
    pub(crate) exit_force_sec: i64,
    pub(crate) feed_ws: bool,
    /// `--net-check`: probe the venue's network paths, print one JSON report and
    /// exit (0 all passed / 1 any failed). Answered before any service starts,
    /// so it needs no socket, writes no ledger and places no order.
    pub(crate) net_check: bool,
    pub(crate) replay: Option<String>,
    pub(crate) replay_near_miss: Option<String>,
    pub(crate) round_sec: i64,
    pub(crate) near_miss_path: Option<String>,
    pub(crate) trade_log: Option<String>,
    pub(crate) no_trade_log: bool,
    pub(crate) order_log: Option<String>,
    pub(crate) no_order_log: bool,
    pub(crate) position_log: Option<String>,
    pub(crate) no_position_log: bool,
    /// The ledger cash-journal directory (`data/ledger/`). `None` after
    /// `--no-ledger-journal`: the historical memory-only ledger — nothing is
    /// written per cash movement, and boot seeds instead of replaying.
    pub(crate) ledger_journal_dir: Option<String>,
    /// E25 (#331): silence the intent-audit WRITES only — the arbitration
    /// gates always run (P1 proves both settings hit the same baseline).
    pub(crate) no_intent_audit: bool,
    /// #199: acknowledge that this process may write a data directory another
    /// core is already writing (fixtures, backtests). Only disables the
    /// live-owner refusal — the boot banner still says so, loudly.
    pub(crate) allow_shared_data: bool,
    pub(crate) market_plugin: Option<String>,
    pub(crate) discovery: bool,
    pub(crate) shadow_evolution: bool,
    /// Resolved asset list (CLI `--assets` > `BK_ASSETS` > `[engine].assets` >
    /// the built-in list).
    pub(crate) assets: Vec<String>,
    pub(crate) se_min_samples: Option<u32>,
    pub(crate) se_cooldown_secs: Option<i64>,
    pub(crate) se_min_obs_secs: Option<i64>,
    /// Shadow-evolution metrics window (secs), resolved through the same chain.
    pub(crate) se_eval_window_secs: Option<i64>,
    /// Per-evolution step ceiling (Lock 2), resolved through the same chain and
    /// clamped to the built-in ceiling — a file may tighten it, never widen it.
    pub(crate) se_max_gradient: Option<Decimal>,
    pub(crate) se_variant_count: Option<usize>,
    pub(crate) se_audit_dir: Option<String>,
    pub(crate) se_min_win_rate: Option<Decimal>,
    pub(crate) se_min_profit_factor: Option<Decimal>,
    /// E13 proposal workflow (all resolved through the same chain).
    pub(crate) se_auto_evolve: Option<bool>,
    pub(crate) se_cycle_secs: Option<i64>,
    pub(crate) se_ttl_secs: Option<i64>,
    pub(crate) se_deep_dims: Option<usize>,
    /// Update-check switches (VERSIONING.md §7), resolved through the same
    /// chain; `None` keeps the built-in default (off).
    pub(crate) update_check_enabled: Option<bool>,
    pub(crate) update_auto: Option<bool>,
    /// #379: seconds between AUTOMATIC update checks; `None`/`0` = no
    /// scheduler (the default — the timer is an explicit opt-in, INV-3).
    pub(crate) update_interval_secs: Option<i64>,
    /// Per-strategy entry caps: `name:max_open_positions:max_notional_usd`
    /// (repeatable; `-` or empty = no cap on that segment).
    pub(crate) strategy_limits: Vec<String>,
    /// E-VA virtual-account pots for strategies that declare `atomic_batch`:
    /// `name:max_batch_usd:acct1,acct2` (repeatable; `-` = no cap — the entry
    /// is dropped with a warning). Parsed into `CoreConfig::virtual_limits`.
    pub(crate) virtual_limits: Vec<String>,
    /// Strategies to switch ON at startup (repeatable, E4-a). The kernel ships
    /// ZERO enabled strategies — what starts enabled is the operator's persisted
    /// intent (`--strategy-state` file, replayed and rewritten) plus these flags.
    /// A name that resolves to no live strategy is REPORTED and, when it is the
    /// only thing asked for, REFUSES the boot (#265) unless
    /// `--allow-zero-strategies` acknowledges it; a name in the persisted set
    /// that no longer resolves stays a warning (nobody asked for it in this
    /// invocation).
    pub(crate) enable_strategy: Vec<String>,
    /// Strategies to switch OFF at startup (repeatable). Applied after
    /// `--enable-strategy`, so an explicit "off" wins.
    pub(crate) disable_strategy: Vec<String>,
    /// #265 — acknowledge that an explicit `--enable-strategy` may resolve to
    /// nothing. Without it, a boot that asked for strategies and loaded none is
    /// refused (exit 1) rather than started with a healthy-looking banner and no
    /// trading logic. This never enables anything: it only stops the refusal.
    pub(crate) allow_zero_strategies: bool,
    /// Where the effective enabled-set is recorded so toggles and boot flags
    /// survive a restart. Default `data/strategy-state.json`; `none`/empty = no
    /// persistence (a backtest or a hermetic harness wants this).
    pub(crate) strategy_state: Option<String>,
    /// Directory scanned at startup for user-layer strategy libraries
    /// (`*.dylib`/`*.so`): every library found is LOADED, and registers
    /// DISABLED. Enabling is a separate decision, made in `install_engine` from
    /// the persisted set (`strategy_state`) plus `--enable-strategy`, and the
    /// directory is never consulted for it — so dropping a file in the folder
    /// INSTALLS a strategy, it does not switch one on. `none`/empty = off.
    /// Default `user_layer/strategies` under the repo; overridden by
    /// `BK_STRATEGY_DIR` (a file-config key would be dead weight: this belongs
    /// to "where is the checkout", not to strategy parameters).
    pub(crate) strategy_dir: Option<String>,
    /// E30 (§6.4): directory scanned at startup for Lua strategy PACKAGES
    /// (directories carrying `manifest.json`). Same install/enable split as
    /// `strategy_dir`: dropping a package in the folder INSTALLS it DISABLED;
    /// enabling comes from the persisted set plus `--enable-strategy`.
    /// Deliberately separate from the dylib scanner's root (§6.4: Lua packages
    /// are never dylibs, and `user_layer/strategies` is also the measurement
    /// fixtures' root). Default `user_layer/strategies_lua` under the repo;
    /// overridden by `BK_LUA_STRATEGY_DIR`.
    pub(crate) lua_strategy_dir: Option<String>,
    /// Mirror every market-data event into this JSONL archive (P-1.3).
    /// None = use the always-on default for an engine session (see `--no-event-archive`).
    pub(crate) event_archive: Option<String>,
    /// Disable market-data capture entirely. Required for a harness that spawns
    /// the engine: it would otherwise record into the default archive path.
    pub(crate) no_event_archive: bool,
    /// Stop recording at this archive size (MiB); 0 = unlimited (the default).
    /// None = not given; the always-on default then supplies its own value.
    pub(crate) event_archive_max_mb: Option<u64>,
    /// Rotate into a new UTC-stamped segment every this many MiB (0 = never).
    /// Required for a 24/7 capture: an un-rotated archive hits the cap and goes
    /// dark. Rotation only renames; nothing is ever deleted.
    pub(crate) event_archive_rotate_mb: Option<u64>,
    /// Stop recording before the volume has less than this many MiB free
    /// (0 = no guard). Checked once per rotation.
    pub(crate) event_archive_min_free_mb: Option<u64>,
    /// Maker→taker escalation deadline for engine entries (ms).
    pub(crate) entry_maker_timeout_ms: i64,
    /// #205 — how old an orderbook may be before the engine refuses to price off
    /// it (ms). 0 = the freshness check is OFF (any age accepted). This is the
    /// value in force after CLI/env resolution and validation; the compiled
    /// default is `engine::DEFAULT_MAX_ORDERBOOK_STALE_MS`.
    pub(crate) max_orderbook_stale_ms: i64,
    /// The startup echo for the freshness budget, printed unconditionally: the
    /// answer to "how long after the feed goes quiet does this bot stop
    /// trading?" must not depend on someone having thought to configure it.
    pub(crate) orderbook_stale_echo: Vec<String>,
    /// 任务二 1-3 — NTP 时钟偏差探针的服务器主机。空串 = 探针关闭（零网络
    /// 任务启动，回放安全）。编译默认 `probe::DEFAULT_NTP_HOST`。
    pub(crate) ntp_host: String,
    /// 任务二 1-3 — NTP 探针的查询间隔（秒）。0 = 关（不启动循环）。
    /// 默认 `probe::DEFAULT_NTP_INTERVAL_SECS`（300）。
    pub(crate) ntp_interval_secs: i64,
    /// The startup echo for the NTP probe, printed unconditionally (same
    /// posture as `orderbook_stale_echo`): "is this run watching the clock,
    /// and against whom?" must be answerable from the boot log alone.
    pub(crate) ntp_echo: Vec<String>,
    /// Offline replay of an archive through the same core (P-1.2).
    pub(crate) backtest: Option<String>,
    /// Where to write the backtest report as JSON (None = stdout only).
    pub(crate) backtest_report: Option<String>,
    /// Virtual-clock step (ms) between maintenance cycles in a replay.
    pub(crate) backtest_tick_ms: i64,
    /// Keep the replay clock running this long after the last event (ms).
    pub(crate) backtest_tail_ms: i64,
    /// Counterfactual `--backtest-knob <strategy>:<knob>=<value>` overrides,
    /// handed to the replayed strategies through the Shadow Evolution hot-param
    /// path (so a replay can A/B a knob value on the SAME frozen corpus).
    pub(crate) backtest_knobs: Vec<(String, String, Decimal)>,
    /// #351: the replay mode — `mine` / `verify` / `sweep`. Empty = the
    /// historic single-run behaviour (no mode requested, one replay, one
    /// report — the shape every existing gate and script drives).
    pub(crate) backtest_mode: String,
    /// #351: `verify` mode's latency dial (ms). The HK-VPS number the spec
    /// names is 286; `sweep` ignores this (it owns its own ladder).
    pub(crate) verify_latency_ms: i64,
    /// Counterfactual taker-fee schedule for a replay (#203): `--fee-model
    /// <name>`. None = the shipped schedule. Replay-only, exactly like
    /// `--backtest-knob`: what a live run charges is not a per-invocation choice,
    /// so asking for this without `--backtest` is refused rather than ignored.
    pub(crate) fee_model: Option<blitzkrieg_core::exit_policy::FeeSchedule>,
    /// Model the venue's maker rebate in a replay: `--maker-rebate`. Each
    /// MAKER fill then earns `0.20 ×` the taker fee that same notional would
    /// have paid (the official rule's pool share, approximated per fill —
    /// almach-benchmark REPORT §5 失真点 5), credited as cash and accrued into
    /// the trade records' net. Replay-only like `--fee-model`: a live core
    /// must never credit cash the venue has not reported.
    pub(crate) maker_rebate: bool,
    /// Replay maintenance-gating fast path: `--backtest-fast`. Skips the
    /// per-tick work an every-tick pass provably discards (the holder ladder,
    /// full-book re-valuation, per-tick Lua on quiet ticks, the settlement /
    /// escalation sweeps between their deadlines) while keeping every
    /// timestamp and reaction instant the historic path produced. Replay-only
    /// for the same reason as `--fee-model`: a live kernel's tick cadence is
    /// not a per-invocation choice, so asking for this without `--backtest`
    /// is refused rather than ignored.
    pub(crate) backtest_fast: bool,
    /// MarketRegime evaluation over an archive (E16 / #98): label windows and
    /// score the online state machine against the offline labels.
    pub(crate) regime_eval: Option<String>,
    /// Where to write the regime report (<path>.json + .md; None = stdout).
    pub(crate) regime_report: Option<String>,
    /// Evaluate exactly this token instead of the most-active defaults.
    pub(crate) regime_token: Option<String>,
    /// How many of the most active tokens to evaluate (default 3).
    pub(crate) regime_max_tokens: usize,
    /// Regime window in seconds — the label rule and the machine share it.
    pub(crate) regime_window_sec: i64,
    /// Trend bar: |net move| ≥ this many ticks with the efficiency ≥ the bar.
    pub(crate) regime_min_trend_ticks: Decimal,
    /// Trend efficiency bar (|net| / path).
    pub(crate) regime_min_efficiency: Decimal,
    /// Volatile bar: mean |per-step move| ≥ this many ticks.
    pub(crate) regime_volatile_mad_ticks: Decimal,
    /// Confirmation hysteresis before the machine switches state.
    pub(crate) regime_confirmations: u32,
    /// #352: pull a wallet's on-chain fills and convert to the backtest JSONL
    /// event stream (Polymarket Data/Gamma/CLOB APIs). The offline twin of a
    /// live feed — the same `--backtest` consumes the output.
    pub(crate) onchain_pull: Option<String>,
    /// #352 pull window start (`YYYY-MM-DD` or epoch seconds, UTC).
    pub(crate) onchain_start: Option<String>,
    /// #352 pull window end (inclusive of fills up to this instant).
    pub(crate) onchain_end: Option<String>,
    /// #352 pull filter: one asset's updown rounds (`BTC`/`ETH`/`SOL`/`XRP`).
    pub(crate) onchain_asset: Option<String>,
    /// #352 pull filter: one round duration (`5m`/`15m`/`1h`/`4h`).
    pub(crate) onchain_market: Option<String>,
    /// #355: read the fill universe from a pre-fetched `/activity` JSONL
    /// export instead of walking `/trades` (which answers only a taker-side
    /// subset — 17.6% of the same window's notional, measured 2026-10-02).
    pub(crate) onchain_fills: Option<String>,
    /// #352 output root (default `data/onchain`).
    pub(crate) onchain_out_dir: Option<String>,
    /// #361: compile this blueprint JSON to a Lua strategy body and exit
    /// (offline, like --regime-eval: no mode, no feed, no ledger).
    pub(crate) blueprint_compile: Option<String>,
    /// #361 output override (default `strategy.lua` beside the blueprint).
    pub(crate) blueprint_out: Option<String>,
    /// Fill model: taker slippage in ticks (0.01).
    pub(crate) slippage_ticks: u32,
    /// Fill model: maker latency (ms).
    pub(crate) latency_ms: i64,
    /// #351 fill model: taker latency (ms) — a taker leg fills against the
    /// book as it stands `decision_at + taker_latency_ms`, never the
    /// decision-time quote. 0 = the historic immediate fill.
    pub(crate) taker_latency_ms: i64,
    /// #351 fill model: taker fill-report rtt (ms) — ledger effects land at
    /// `execute_at + taker_rtt_ms`. 0 = effects apply at execution.
    pub(crate) taker_rtt_ms: i64,
    /// Fill model: maker fill probability (bps of 10000); None = untouched.
    pub(crate) fill_prob_bps: Option<u32>,
    /// Fill model: share of the crossing depth a resting maker takes, in bps of
    /// 10000 (queue position); None = untouched.
    pub(crate) maker_depth_share_bps: Option<u32>,
    /// Dry-mode test hook: the first N simulated redemption attempts of each
    /// claim fail before one lands (0 = every attempt lands).
    pub(crate) dry_redeem_fail: u32,
    /// Dry-mode test hook: report those injected failures as `manual`, i.e. stop
    /// the automatic retries for good.
    pub(crate) dry_redeem_manual: bool,
    /// One line per file-settable setting, `key=value (source)`, for the startup
    /// report. Only settings resolved from above the compiled default appear, so
    /// "where did this number come from" is answerable from the log alone.
    pub(crate) config_report: Vec<String>,
    /// The evolution engine's state at startup, printed unconditionally (#269):
    /// "is the evaluator running?" must be answerable from the boot log of a run
    /// that set no flags — it is the question the switch-conflict warning below
    /// is about.
    pub(crate) shadow_evolution_echo: Vec<String>,
}

/// Canonical socket name.
pub const SOCKET_PREFIX: &str = "blitzkrieg-core";

fn socket_path_for(prefix: &str) -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/{prefix}-{user}.sock").replace("//", "/")
}

fn default_socket() -> String {
    socket_path_for(SOCKET_PREFIX)
}

fn default_strategy_dir() -> Option<String> {
    let local = std::path::Path::new("user_layer/strategies");
    if local.is_dir() {
        return Some("user_layer/strategies".to_string());
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.parent();
        while let Some(dir) = cur {
            let candidate = dir.join("user_layer/strategies");
            if candidate.is_dir() {
                return Some(candidate.to_string_lossy().into_owned());
            }
            cur = dir.parent();
        }
    }
    Some("user_layer/strategies".to_string())
}

/// E30 (§6.4): the Lua package scan root, resolved like `default_strategy_dir`
/// — repo checkout first, then upward from the binary.
fn default_lua_strategy_dir() -> Option<String> {
    let local = std::path::Path::new("user_layer/strategies_lua");
    if local.is_dir() {
        return Some("user_layer/strategies_lua".to_string());
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.parent();
        while let Some(dir) = cur {
            let candidate = dir.join("user_layer/strategies_lua");
            if candidate.is_dir() {
                return Some(candidate.to_string_lossy().into_owned());
            }
            cur = dir.parent();
        }
    }
    Some("user_layer/strategies_lua".to_string())
}

/// Where the config file comes from, settled BEFORE argument parsing (the file
/// has to be loaded to resolve the arguments against it).
pub(crate) enum ConfigChoice {
    /// No file: every setting comes from CLI/env/default.
    Off,
    Path(String),
}

/// #380: settle WHICH execution-policy file this run loads, and say so.
///
/// The shipped default (`user_layer/configs/execution_policy.toml`) used to be
/// interpreted relative to the process cwd, so the same binary with the same
/// arguments loaded a guarded policy from the repo root and the NEUTRAL kernel
/// (code defaults, no rules — #372 semantics) from anywhere else. The default
/// is now resolved against the install root (the executable's directory and
/// its ancestors — a release bundle's `bin/`, or a checkout's
/// `target/release/` walking up to the repo root) first, and the cwd's
/// ancestors second; an explicit `--execution-policy <path>` passes through
/// untouched.
///
/// The outcome lands in `CoreConfig.execution_policy_path` as the path the
/// library will read, and the decision is echoed here — INFO with the winning
/// base, or a LOUD WARN when nothing was found anywhere (the run continues on
/// the neutral kernel, but "my policy did not load" must be visible in the
/// boot log, not discovered from the entry counts).
pub(crate) fn resolved_policy_path(
    explicit: Option<&str>,
) -> blitzkrieg_core::execution_policy::ResolvedPolicyPath {
    use blitzkrieg_core::execution_policy::{DefaultPathSource, ResolvedPolicyPath};
    let resolved = blitzkrieg_core::execution_policy::resolve_execution_policy_path(explicit);
    match &resolved {
        ResolvedPolicyPath::Explicit(path) => {
            eprintln!(
                "blitzkrieg-core: execution policy file (explicit): {}",
                path.display()
            );
        }
        ResolvedPolicyPath::Default(r) => match r.source {
            DefaultPathSource::ExeRoot => eprintln!(
                "blitzkrieg-core: execution policy file {} (install root: {})",
                r.path.display(),
                r.base_dir
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| r.path.display().to_string()),
            ),
            DefaultPathSource::Cwd => eprintln!(
                "blitzkrieg-core: execution policy file {} (working directory: {})",
                r.path.display(),
                r.base_dir
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| r.path.display().to_string()),
            ),
            DefaultPathSource::Fallback => {
                let dirs = r
                    .searched
                    .iter()
                    .filter_map(|p| p.parent())
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>();
                eprintln!(
                    "blitzkrieg-core: WARNING — execution policy file not found; running the \
                     NEUTRAL policy (code defaults, no rules). Searched: {}. Ship \
                     user_layer/configs/execution_policy.toml beside the executable or pass \
                     --execution-policy <path>.",
                    if dirs.is_empty() {
                        "(no directories could be probed)".to_string()
                    } else {
                        dirs.join(", ")
                    }
                );
            }
        },
    }
    resolved
}

/// The environment layer of the precedence chain, wrapped in a struct so a test
/// can supply its own map. Mutating the process environment to test precedence
/// would be global state that parallel tests race on.
#[derive(Debug, Clone, Default)]
pub(crate) struct EnvVars {
    pub(crate) vars: std::collections::HashMap<String, String>,
}

impl EnvVars {
    /// Every `BK_*` variable in the process environment. Only the kernel's own
    /// namespace is copied: `HFT_*` belongs to the UI shell and `DRY_RUN` is
    /// read at its own call site (its meaning predates this layer).
    pub(crate) fn from_process() -> Self {
        let mut vars = std::collections::HashMap::new();
        for (k, v) in std::env::vars() {
            if k.starts_with("BK_") || k.starts_with("SHADOW_") {
                vars.insert(k, v);
            }
        }
        Self { vars }
    }

    fn text(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }

    fn num<T: FromStr>(&self, key: &str) -> Option<T> {
        self.text(key).and_then(|v| v.trim().parse::<T>().ok())
    }
}

/// Pre-scan `argv` for `--config <path>` / `--config=<path>` / `--no-config`.
///
/// Two passes over `argv` are unavoidable — the file must be read before the
/// flags it competes with can be resolved — so this is kept deliberately dumb:
/// it looks for exactly those three spellings and hands everything else to the
/// real parser.
pub(crate) fn prescan_config(argv: &[String]) -> Option<ConfigChoice> {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => {
                return Some(match it.next() {
                    Some(p) if !p.trim().is_empty() => ConfigChoice::Path(p.clone()),
                    _ => ConfigChoice::Off,
                });
            }
            "--no-config" => return Some(ConfigChoice::Off),
            other => {
                if let Some(v) = other.strip_prefix("--config=") {
                    return Some(if v.trim().is_empty() {
                        ConfigChoice::Off
                    } else {
                        ConfigChoice::Path(v.to_string())
                    });
                }
            }
        }
    }
    None
}

/// Resolve the config file: `--config` wins over `BK_CONFIG`, which wins over the
/// repo's shipped default. `BK_CONFIG=none` (like `--no-config`) turns the file
/// off entirely, which is what a harness wants when it must not inherit an
/// operator's edits.
pub(crate) fn config_choice(argv: &[String], env: &EnvVars) -> ConfigChoice {
    if let Some(c) = prescan_config(argv) {
        return c;
    }
    match env.text("BK_CONFIG") {
        Some(v) if v.trim() == "none" => ConfigChoice::Off,
        Some(v) if !v.trim().is_empty() => ConfigChoice::Path(v.trim().to_string()),
        _ => ConfigChoice::Path(blitzkrieg_core::config::DEFAULT_CONFIG_PATH.to_string()),
    }
}

/// Split a comma-separated asset list ("BTC,ETH") the one way, wherever it came
/// from.
fn split_assets(s: &str) -> Vec<String> {
    s.split(',')
        .map(|a| a.trim().to_uppercase())
        .filter(|a| !a.is_empty())
        .collect()
}

/// Parse one `--backtest-knob <strategy>:<knob>=<value>` override.
///
/// Shape is deliberate: exactly one `:` and one `=`, the strategy and knob names
/// non-empty, and the value a decimal string (the wire rule for knob values
/// everywhere else). A malformed spec is a startup error, never a silently
/// ignored counterfactual that would make a comparison meaningless.
fn parse_backtest_knob(spec: &str) -> Result<(String, String, Decimal), String> {
    let (strategy, rest) = spec
        .split_once(':')
        .ok_or_else(|| "want <strategy>:<knob>=<value>".to_string())?;
    let (knob, value) = rest
        .split_once('=')
        .ok_or_else(|| "want <strategy>:<knob>=<value>".to_string())?;
    let strategy = strategy.trim();
    let knob = knob.trim();
    if strategy.is_empty() || knob.is_empty() {
        return Err("strategy and knob must not be empty".into());
    }
    let value = Decimal::from_str(value.trim()).map_err(|e| format!("bad decimal value: {e}"))?;
    Ok((strategy.to_string(), knob.to_string(), value))
}

/// Parse one `--exit-*` percentage value.
///
/// Pure so the rejection is unit-testable; the caller keeps the loud failure
/// (stderr + exit 2). Deliberately STRICTER than the `--spread-arb-*` knobs,
/// which swallow a typo'd decimal and leave the flag doing nothing: an exit
/// ladder is what bounds a loss, so a `--exit-stop-loss-pct 1x` that silently
/// kept the shipped 12 would be the "silent downgrade of a safety knob" #228
/// removed for unknown arguments, one layer down. Nothing here validates the
/// VALUE (the engine has its own guards, e.g. `take_profit_pct` only fires
/// between 0 and 9999) — only that the operator's number was understood.
pub(crate) fn parse_exit_pct(raw: &str) -> Result<Decimal, String> {
    Decimal::from_str(raw.trim())
        .map_err(|e| format!("want a decimal percentage (e.g. 12, 8.5): {e}"))
}

/// Why `--max-orderbook-stale-ms` / `BK_MAX_ORDERBOOK_STALE_MS` cannot be used as
/// given, or `None` when it can (#205).
///
/// Pure so the rule is unit-testable while the caller keeps the loud failure
/// (stderr + exit 2): a budget that silently falls back to the compiled default
/// is the exact failure mode this knob exists to remove. `0` is legal and means
/// "freshness check OFF" — a supported setting, not an invalid one — so it is
/// deliberately not rejected.
pub(crate) fn stale_budget_rejection(ms: i64) -> Option<String> {
    if ms < 0 {
        return Some(format!(
            "a negative value ({ms} ms): pass 0 to disable the check, or a positive number of \
             milliseconds (compiled default {})",
            blitzkrieg_core::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS
        ));
    }
    if ms > blitzkrieg_core::engine::MAX_ORDERBOOK_STALE_MS_CEILING {
        return Some(format!(
            "{} ms, past the {} ms ceiling — this is almost certainly a typo; pass 0 to disable \
             the check entirely",
            ms,
            blitzkrieg_core::engine::MAX_ORDERBOOK_STALE_MS_CEILING
        ));
    }
    None
}

// ── Strict value parsing for every value-taking flag (#441) ──────────────────
//
// Until #441 roughly half the parse arms read `it.next().and_then(parse.ok())
// .unwrap_or(default)` (or `.or(option)`): a value the kernel could not parse
// silently fell back to the compiled default. The unknown-ARGUMENT refusal
// (#228) never saw these — the spelling was fine, the VALUE was not. For a
// knob that bounds the process (`--max-positions`, `--max-daily-loss-pct`,
// `--mode`) a fallback is the #228 defect in its second spelling: the operator
// believes a limit is in force and the boot they got is not the boot they
// asked for. So every value-taking arm parses through the helpers below, and
// a value that cannot be honoured is a startup error, never a fallback.
//
// The strict arms that predate this (exit ladder, entry-maker timeout,
// orderbook staleness, fee model, backtest knobs) now share the same
// `bad_value` landing pad, so the message and the exit code are one contract.

/// The one landing pad for an unparseable or missing flag value: name the
/// flag and the value, say what was wanted, exit 2 (same code as #228's
/// unknown-argument refusal).
pub(crate) fn bad_value(flag: &str, raw: &str, want: &str) -> ! {
    eprintln!("blitzkrieg-core: {flag} '{raw}': {want}");
    eprintln!(
        "blitzkrieg-core: refusing to start: a value this flag cannot honour is never silently \
         replaced by its default — the boot that would produce is not the one you asked for. \
         Run `blitzkrieg-core --help` for the value form."
    );
    std::process::exit(2);
}

/// Defaults for the file-settable settings. Named constants because each one now
/// has three potential suppliers (CLI, env, file) and a bare literal at the
/// bottom of a four-deep chain is unreadable.
pub(crate) const DEFAULT_ROUND_SEC: i64 = 900;
pub(crate) const DEFAULT_MIN_ROUND_AGE_SEC: i64 = 30;
pub(crate) const DEFAULT_MIN_TIME_LEFT_SEC: i64 = 180;
pub(crate) const DEFAULT_ASSETS: [&str; 4] = ["BTC", "ETH", "SOL", "XRP"];

/// Default archive path, relative to the core's working directory (the repo root
/// for the production shell).
pub(crate) const DEFAULT_EVENT_ARCHIVE: &str = "data/archive/events.jsonl";

/// Resolve the market-data capture settings.
///
/// Capture is ON by default for an engine session (see the rationale at the call
/// site). `--no-event-archive` disables it; an explicit `--event-archive <path>`
/// wins over the default path. Unset tuning gets the 24/7-safe values: no session
/// cap (`0`), 256 MiB segments, and a 5 GiB free-space floor — a cap with no
/// rotation would silently go dark, which is the failure this default exists to
/// avoid.
pub(crate) fn resolve_event_archive(
    engine: bool,
    no_event_archive: bool,
    path: Option<&str>,
    max_mb: Option<u64>,
    rotate_mb: Option<u64>,
    min_free_mb: Option<u64>,
) -> (Option<String>, u64, u64, u64) {
    if no_event_archive {
        return (None, 0, 0, 0);
    }
    let path = path
        .map(str::to_string)
        .or_else(|| engine.then(|| DEFAULT_EVENT_ARCHIVE.to_string()));
    match path {
        Some(p) => (
            Some(p),
            max_mb.unwrap_or(0),
            rotate_mb.unwrap_or(256),
            min_free_mb.unwrap_or(5120),
        ),
        None => (None, 0, 0, 0),
    }
}

/// Parse repeated `--strategy-limit` flags ("-" or an empty segment = inherit
/// the global value there). Three shapes are accepted, all colon-separated:
///
/// * `name:max_open_positions:max_notional_usd` (P-1.1, unchanged)
/// * `name:max_open_positions:max_notional_usd:size_usd:min_shares:max_shares`
///   (E2-a: per-strategy sizing, clamped by the global risk band)
/// * …`:weight` (E16/#98: 7-segment form; the leg's allocation weight, "-"/""
///   = unweighted)
/// * …`:weight:size_pct` (#202: 8-segment form; the leg's own share of the
///   account per entry, clamped to the global `--size-pct` when that is armed)
///
/// Malformed flags are ignored with a warning so a typo cannot brick startup.
pub(crate) fn parse_strategy_limits(
    args: &[String],
) -> std::collections::HashMap<String, blitzkrieg_core::service::StrategyLimit> {
    /// Parse an optional decimal segment: ""/"-" = None, bad value = Err.
    fn opt_dec(seg: &str) -> Result<Option<Decimal>, ()> {
        match seg.trim() {
            "" | "-" => Ok(None),
            v => Decimal::from_str(v).map(Some).map_err(|_| ()),
        }
    }

    let mut out = std::collections::HashMap::new();
    for raw in args {
        let parts: Vec<&str> = raw.split(':').collect();
        if !matches!(parts.len(), 3 | 6 | 7 | 8) || parts[0].trim().is_empty() {
            eprintln!(
                "blitzkrieg-core: ignoring malformed --strategy-limit '{raw}' \
                 (want name:max_open:max_notional[:size_usd:min_shares:max_shares[:weight[:size_pct]]])"
            );
            continue;
        }
        let max_open_positions = match parts[1].trim() {
            "" | "-" => None,
            v => match v.parse::<usize>() {
                Ok(n) => Some(n),
                Err(_) => {
                    eprintln!(
                        "blitzkrieg-core: ignoring --strategy-limit '{raw}' (bad position cap)"
                    );
                    continue;
                }
            },
        };
        let max_open_notional_usd = match opt_dec(parts[2]) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("blitzkrieg-core: ignoring --strategy-limit '{raw}' (bad notional cap)");
                continue;
            }
        };
        // E2-a sizing: absent entirely (3-segment form) or inherited per segment.
        let (size_usd, min_shares, max_shares) = if parts.len() >= 6 {
            let mut vals = [None, None, None];
            let labels = ["size_usd", "min_shares", "max_shares"];
            let mut bad = None;
            for (i, seg) in parts[3..6].iter().enumerate() {
                match opt_dec(seg) {
                    Ok(v) => vals[i] = v,
                    Err(_) => {
                        bad = Some(labels[i]);
                        break;
                    }
                }
            }
            if let Some(label) = bad {
                eprintln!("blitzkrieg-core: ignoring --strategy-limit '{raw}' (bad {label})");
                continue;
            }
            (vals[0], vals[1], vals[2])
        } else {
            (None, None, None)
        };
        // E16/#98 allocation weight: 7- and 8-segment forms.
        let size_weight = if parts.len() >= 7 {
            match opt_dec(parts[6]) {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("blitzkrieg-core: ignoring --strategy-limit '{raw}' (bad weight)");
                    continue;
                }
            }
        } else {
            None
        };
        // #202 equity-relative sizing: 8-segment form only; the weight segment
        // stays mandatory (write "-" to skip it) so the trailing percentage can
        // never be mistaken for it.
        let size_pct = if parts.len() == 8 {
            match opt_dec(parts[7]) {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("blitzkrieg-core: ignoring --strategy-limit '{raw}' (bad size_pct)");
                    continue;
                }
            }
        } else {
            None
        };
        out.insert(
            parts[0].trim().to_string(),
            blitzkrieg_core::service::StrategyLimit {
                max_open_positions,
                max_open_notional_usd,
                size_usd,
                min_shares,
                max_shares,
                size_weight,
                size_pct,
            },
        );
    }
    out
}

/// E-VA: parse repeated `--virtual-limit` flags
/// (`name:max_batch_usd:acct1,acct2`) into the virtual pots
/// `CoreConfig::virtual_limits` holds. `-` in the cap segment = "no cap" —
/// such an entry is DROPPED with a warning (an armed-but-uncapped pot would
/// flip the atomic-batch posture on while enforcing nothing, which is the
/// worst of both worlds); malformed flags are ignored with a warning so a
/// typo cannot brick startup (the `parse_strategy_limits` posture).
pub(crate) fn parse_virtual_limits(
    args: &[String],
) -> std::collections::HashMap<String, blitzkrieg_core::service::VirtualAccountLimit> {
    let mut out = std::collections::HashMap::new();
    for raw in args {
        let parts: Vec<&str> = raw.split(':').collect();
        if parts.len() != 3 || parts[0].trim().is_empty() {
            eprintln!(
                "blitzkrieg-core: ignoring malformed --virtual-limit '{raw}' \
                 (want name:max_batch_usd:acct1,acct2)"
            );
            continue;
        }
        let cap = match parts[1].trim() {
            "-" | "" => {
                eprintln!(
                    "blitzkrieg-core: ignoring --virtual-limit '{raw}' \
                     (no cap — an uncapped pot would enforce nothing; omit the flag instead)"
                );
                continue;
            }
            v => match Decimal::from_str(v) {
                Ok(c) => c,
                Err(_) => {
                    eprintln!("blitzkrieg-core: ignoring --virtual-limit '{raw}' (bad cap)");
                    continue;
                }
            },
        };
        let accounts: Vec<String> = parts[2]
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect();
        if accounts.is_empty() {
            eprintln!("blitzkrieg-core: ignoring --virtual-limit '{raw}' (no accounts)");
            continue;
        }
        out.insert(
            parts[0].trim().to_string(),
            blitzkrieg_core::service::VirtualAccountLimit {
                max_batch_notional_usd: Some(cap),
                accounts,
            },
        );
    }
    out
}

/// #173: where the day's realized-loss budget is persisted. A sibling of the
/// position log, derived the same way the `.recon` watermark is
/// (`path.with_extension(..)`), so `data/positions/positions.jsonl` gets
/// `data/positions/positions.daily-loss.json` beside it: the two files describe
/// the same book and belong in the same place (both under the git-ignored
/// `data/`).
pub(crate) fn daily_loss_path_for(position_log: &str) -> String {
    std::path::Path::new(position_log)
        .with_extension("daily-loss.json")
        .to_string_lossy()
        .into_owned()
}

/// `--net-check`: probe every network path the ACTIVE market plugin uses and
/// print one JSON `NetCheckReport` on stdout, returning the code the shell
/// should see (0 = every probe passed, 1 = something failed).
///
/// The registry is built exactly as the server builds it, so the probe covers
/// the venue this build and its `--market-plugin` would actually talk to —
/// answering "is it us or the venue?" for the run that is in front of you
/// rather than for a hardcoded host. The answer is JSON because it is a data
/// contract: the launcher, the TUI overlay and the WebUI card all render the
/// same struct in Chinese, and no user-facing wording belongs in the core.
pub(crate) async fn run_net_check(plugin: Option<&str>) -> i32 {
    let registry = blitzkrieg_core::market::registry::MarketPluginRegistry::new();
    blitzkrieg_core::market::register_builtin_markets(&registry);
    let active = blitzkrieg_core::market::active_market_plugin(&registry, plugin);
    let report = active.net_check().await;
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("blitzkrieg-core: net-check report is not serializable: {e}");
            return 1;
        }
    }
    if report.ok { 0 } else { 1 }
}

/// Which tier owns `engine.round_sec` at boot — and therefore whether the UI
/// card's persisted cell (data/runtime/config.json) may apply. Only an
/// explicit CLI flag or BK_* env var suppresses the cell; the factory TOML's
/// `engine.round_sec` is a default FOR the card, not a veto over it. The
/// config_report lines carry the winning layer in parentheses, so the tier
/// is readable straight off them.
pub(crate) fn round_duration_source_from(report: &[String]) -> &'static str {
    if report.iter().any(|l| {
        l.starts_with("engine.round_sec=") && (l.ends_with("(cli)") || l.ends_with("(env)"))
    }) {
        "cli_or_env"
    } else {
        "default"
    }
}
