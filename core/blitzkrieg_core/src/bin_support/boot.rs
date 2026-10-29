// #699 — the bin's boot layer, moved here verbatim from `main.rs`:
// the argument surface (`Args`, `parse_args`, config choice, env vars and
// the parse helpers behind them) plus the `--net-check` probe. Declared
// only from `main.rs`, so it stays private to the bin target; items
// referenced from `main()` or from the tests carry `pub(crate)`.

use blitzkrieg_core::model::Mode;
use blitzkrieg_core::position::PositionConfig;
use rust_decimal::Decimal;
use std::str::FromStr;

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

/// Fetch the next argv token as the value of `flag`, refusing a missing value.
macro_rules! next_value {
    ($it:expr, $flag:literal) => {
        match $it.next() {
            Some(v) => v,
            None => bad_value($flag, "", "requires a value (see --help)"),
        }
    };
}

/// Parse the next token as `T`, refusing a non-number.
macro_rules! num_value {
    ($it:expr, $flag:literal, $t:ty, $want:literal) => {{
        let raw = next_value!($it, $flag);
        match raw.trim().parse::<$t>() {
            Ok(v) => v,
            Err(_) => bad_value($flag, &raw, $want),
        }
    }};
}

/// Parse the next token as a Decimal, refusing a non-decimal.
macro_rules! dec_value {
    ($it:expr, $flag:literal, $want:literal) => {{
        let raw = next_value!($it, $flag);
        match rust_decimal::Decimal::from_str(raw.trim()) {
            Ok(v) => v,
            Err(_) => bad_value($flag, raw.trim(), $want),
        }
    }};
}

/// Parse the next token as an explicit boolean (`true|1|on`, `false|0|off`),
/// refusing anything else.
macro_rules! bool_value {
    ($it:expr, $flag:literal) => {{
        let raw = next_value!($it, $flag);
        match raw.as_str() {
            "true" | "1" | "on" => true,
            "false" | "0" | "off" => false,
            _ => bad_value($flag, &raw, "want true|false (1|on / 0|off accepted)"),
        }
    }};
}

/// Fetch the next argv token as a path-like string value, refusing a missing
/// one. The `.filter(non-empty)` arms keep their empty-is-off semantics — an
/// explicit empty string is a documented spelling for "off" on those flags —
/// but a MISSING token is a dangling flag, not an off switch.
macro_rules! path_value {
    ($it:expr, $flag:literal) => {
        next_value!($it, $flag)
    };
}

pub(crate) fn parse_args(
    file: &blitzkrieg_core::config::FileConfig,
    argv: &[String],
    env: &EnvVars,
) -> Args {
    let mut socket = default_socket();
    let mut mode = Mode::Dry;
    let mut readonly = false;
    let mut tick_ms = 50u64;
    let mut seed_balance = Decimal::from(10_000);
    let mut max_order_notional = Decimal::from(100);
    let mut max_open_notional = Decimal::ZERO;
    let mut min_shares: Option<Decimal> = None;
    let mut max_shares: Option<Decimal> = None;
    // #202: the equity-relative sizing and per-order cap. Both tracked as Option
    // so CLI > env > default resolution can tell "the operator spoke" from
    // "nobody did", and both default to 0 (off) — the shipped absolute
    // behaviour, untouched.
    let mut size_pct: Option<Decimal> = None;
    let mut max_order_notional_pct: Option<Decimal> = None;
    let mut markets: Vec<String> = Vec::new();
    let mut auto_exits = true;
    let mut max_positions: usize = 2;
    let mut max_positions_per_asset: Option<u32> = None;
    // #173: the daily-loss budget, tracked as Option so CLI > env > default
    // resolution can tell "the operator spoke" from "nobody did".
    let mut max_daily_loss: Option<Decimal> = None;
    let mut max_daily_loss_pct: Option<Decimal> = None;
    let mut strategy_limits: Vec<String> = Vec::new();
    let mut virtual_limits: Vec<String> = Vec::new();
    let mut enable_strategy: Vec<String> = Vec::new();
    let mut disable_strategy: Vec<String> = Vec::new();
    let mut strategy_state: Option<String> = None;
    let mut no_strategy_state = false;
    let mut strategy_dir: Option<String> = None;
    let mut no_strategy_dir = false;
    let mut lua_strategy_dir: Option<String> = None;
    let mut no_lua_strategy_dir = false;
    let mut engine = false;
    // File-settable settings are collected as Option so the precedence chain can
    // resolve them at the end; a concrete default would erase the "was this flag
    // given?" question that TOML-vs-CLI depends on.
    let mut min_round_age: Option<i64> = None;
    let mut min_time_left: Option<i64> = None;
    let mut trend_confirm: i64 = 60;
    let mut trend_floor_ms: i64 = 10_000;
    let mut spread_arb_entry_factor: Option<Decimal> = None;
    let mut spread_arb_min_obi: Option<Decimal> = None;
    let mut spread_arb_max_spread_pct: Option<Decimal> = None;
    let mut spread_arb_dip_max_pct: Option<Decimal> = None;
    let mut spread_arb_bounce_min_pct: Option<Decimal> = None;
    let mut spread_arb_bounce_window_sec: Option<i64> = None;
    // E17/#264: the four engine-level exit knobs, tracked as Option so the
    // CLI > TOML > default chain can tell "the operator spoke" from "nobody did".
    let mut exit_take_profit_pct: Option<Decimal> = None;
    let mut exit_stop_loss_pct: Option<Decimal> = None;
    let mut exit_trailing_min_high_pct: Option<Decimal> = None;
    let mut exit_min_trail_pct: Option<Decimal> = None;
    let mut exit_force_sec: Option<i64> = None;
    let mut allow_zero_strategies = false;
    let mut feed_ws = false;
    let mut net_check = false;
    let mut replay: Option<String> = None;
    let mut replay_near_miss: Option<String> = None;
    let mut round_sec: Option<i64> = None;
    let mut near_miss_path: Option<String> = None;
    let mut trade_log: Option<String> = None;
    let mut no_trade_log = false;
    let mut order_log: Option<String> = None;
    let mut no_order_log = false;
    let mut position_log: Option<String> = None;
    let mut no_position_log = false;
    let mut no_ledger_journal = false;
    let mut no_intent_audit = false;
    let mut allow_shared_data = false;
    let mut market_plugin: Option<String> = None;
    let mut discovery = true;
    let mut shadow_evolution_flag = false;
    let mut se_min_samples: Option<u32> = None;
    let mut se_cooldown_secs: Option<i64> = None;
    let mut se_min_obs_secs: Option<i64> = None;
    let mut se_auto_evolve: Option<bool> = None;
    let mut update_check_enabled: Option<bool> = None;
    let mut update_auto: Option<bool> = None;
    let mut update_interval_secs: Option<i64> = None;
    let mut se_cycle_secs: Option<i64> = None;
    let mut se_ttl_secs: Option<i64> = None;
    let mut se_deep_dims: Option<usize> = None;
    let mut assets_arg: Option<String> = None;
    let mut event_archive: Option<String> = None;
    // Tuning is tracked as Option so the always-on default can supply its own
    // values without clobbering an explicit flag (and vice versa).
    let mut event_archive_max_mb: Option<u64> = None;
    let mut event_archive_rotate_mb: Option<u64> = None;
    let mut event_archive_min_free_mb: Option<u64> = None;
    let mut no_event_archive = false;
    let mut entry_maker_timeout_ms: i64 = 5000;
    let mut backtest: Option<String> = None;
    let mut backtest_report: Option<String> = None;
    let mut backtest_tick_ms: i64 = 50;
    let mut backtest_tail_ms: i64 = 0;
    let mut backtest_knobs: Vec<(String, String, Decimal)> = Vec::new();
    // #351: the replay mode — `mine` (zero latency, friction as configured),
    // `verify` (one real-latency run), `sweep` (latency ladder). Empty = the
    // historic single-run behaviour, byte-identical CLI surface.
    let mut backtest_mode = String::new();
    let mut verify_latency_ms: i64 = 286;
    // #203: replay-only fee schedule (see `Args::fee_model`).
    let mut fee_model: Option<blitzkrieg_core::exit_policy::FeeSchedule> = None;
    // Replay-only maker-rebate model (see `Args::maker_rebate`).
    let mut maker_rebate = false;
    // Replay maintenance-gating fast path (see `Args::backtest_fast`).
    let mut backtest_fast = false;
    let mut regime_eval: Option<String> = None;
    let mut regime_report: Option<String> = None;
    let mut regime_token: Option<String> = None;
    let mut regime_max_tokens: usize = 3;
    let mut regime_window_sec: i64 = 300;
    let mut regime_min_trend_ticks = Decimal::new(3, 0);
    let mut regime_min_efficiency = Decimal::new(5, 1);
    let mut regime_volatile_mad_ticks = Decimal::new(15, 1);
    let mut regime_confirmations: u32 = 2;
    // #352: on-chain pull knobs (offline subcommand, like --regime-eval).
    let mut onchain_pull: Option<String> = None;
    let mut onchain_start: Option<String> = None;
    let mut onchain_end: Option<String> = None;
    let mut onchain_asset: Option<String> = None;
    let mut onchain_market: Option<String> = None;
    let mut onchain_fills: Option<String> = None;
    let mut onchain_out_dir: Option<String> = None;
    // #361: the offline blueprint compiler (`--blueprint-compile <json>`),
    // plus its optional output override (`--blueprint-out <path>`).
    let mut blueprint_compile: Option<String> = None;
    let mut blueprint_out: Option<String> = None;
    let mut slippage_ticks: u32 = 0;
    let mut latency_ms: i64 = 0;
    let mut taker_latency_ms: i64 = 0;
    let mut taker_rtt_ms: i64 = 0;
    let mut fill_prob_bps: Option<u32> = None;
    let mut maker_depth_share_bps: Option<u32> = None;
    // #205: the orderbook freshness budget, tracked as Option so CLI > env >
    // default resolution can tell "the operator spoke" from "nobody did".
    let mut max_orderbook_stale_ms: Option<i64> = None;
    // 任务二 1-3: the NTP clock-offset probe. Host None = nobody spoke; the
    // empty string is a DOCUMENTED "off" spelling (path_value!'s empty-is-off
    // semantics), so a deployment can disarm the probe without a rebuild.
    let mut ntp_host: Option<String> = None;
    let mut ntp_interval_secs: Option<i64> = None;
    // Dry-mode redemption test hooks (#175 gate): fail the first N simulated
    // redemption attempts of each claim, optionally as a `manual` failure that
    // stops the retries for good. 0 = production behaviour.
    let mut dry_redeem_fail: u32 = 0;
    let mut dry_redeem_manual = false;
    // #380: the explicit policy-file override. `None` = the shipped default,
    // resolved against the install root (see `resolve_execution_policy_path`).
    let mut execution_policy: Option<String> = None;

    // #228: the explicit opt-out from "an unknown argument stops the boot". Read
    // as a pre-scan rather than as an arm below, because argv order must not
    // decide whether `--allow-unknown-args` applies to an argument that comes
    // BEFORE it — an escape hatch whose validity depends on where it was written
    // is a trap, not an escape hatch.
    let allow_unknown = argv
        .iter()
        .any(|a| a == blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS);

    let mut it = argv.iter().cloned();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" => socket = next_value!(it, "--socket"),
            "--mode" => {
                let raw = next_value!(it, "--mode");
                mode = match raw.as_str() {
                    "live" => Mode::Live,
                    "dry" => Mode::Dry,
                    _ => bad_value(
                        "--mode",
                        &raw,
                        "want dry|live — a typo here would run the OPPOSITE mode of the one you named",
                    ),
                }
            }
            "--readonly" => readonly = true,
            "--tick-ms" => {
                tick_ms = num_value!(it, "--tick-ms", u64, "want a whole number of milliseconds")
            }
            "--seed-balance" => {
                seed_balance = dec_value!(it, "--seed-balance", "want a USD amount (e.g. 10000)")
            }
            "--max-order-notional" => {
                max_order_notional =
                    dec_value!(it, "--max-order-notional", "want a USD amount (e.g. 100)")
            }
            "--max-open-notional-usd" => {
                max_open_notional = dec_value!(
                    it,
                    "--max-open-notional-usd",
                    "want a USD amount (0 = no cap)"
                )
            }
            // #202: per-entry budget as a share of the account, and the
            // per-order notional bound in the same unit. Both are percentages
            // (20 = 20%), both default to 0 = off.
            "--size-pct" => {
                size_pct = Some(dec_value!(
                    it,
                    "--size-pct",
                    "want a percentage of equity (20 = 20%, 0 = off)"
                ))
            }
            "--max-order-notional-pct" => {
                max_order_notional_pct = Some(dec_value!(
                    it,
                    "--max-order-notional-pct",
                    "want a percentage of equity (0 = off)"
                ))
            }
            "--min-shares" => {
                min_shares = Some(dec_value!(it, "--min-shares", "want a share count"))
            }
            "--max-shares" => {
                max_shares = Some(dec_value!(it, "--max-shares", "want a share count"))
            }
            "--no-auto-exits" => auto_exits = false,
            "--engine" => engine = true,
            "--feed-ws" => feed_ws = true,
            "--replay" => replay = Some(path_value!(it, "--replay")),
            "--replay-near-miss" => replay_near_miss = Some(path_value!(it, "--replay-near-miss")),
            "--net-check" => net_check = true,
            "--near-miss-path" => near_miss_path = Some(path_value!(it, "--near-miss-path")),
            "--trade-log" => trade_log = Some(path_value!(it, "--trade-log")),
            "--no-trade-log" => no_trade_log = true,
            "--order-log" => order_log = Some(path_value!(it, "--order-log")),
            "--no-order-log" => no_order_log = true,
            "--position-log" => position_log = Some(path_value!(it, "--position-log")),
            "--no-position-log" => no_position_log = true,
            "--no-ledger-journal" => no_ledger_journal = true,
            "--no-intent-audit" => no_intent_audit = true,
            "--allow-shared-data" => allow_shared_data = true,
            "--market-plugin" => market_plugin = Some(path_value!(it, "--market-plugin")),
            "--no-discovery" => discovery = false,
            "--shadow-evolution" => shadow_evolution_flag = true,
            "--se-min-samples" => {
                se_min_samples = Some(num_value!(
                    it,
                    "--se-min-samples",
                    u32,
                    "want a sample count"
                ))
            }
            "--se-cooldown-secs" => {
                se_cooldown_secs = Some(num_value!(
                    it,
                    "--se-cooldown-secs",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--se-min-obs-secs" => {
                se_min_obs_secs = Some(num_value!(
                    it,
                    "--se-min-obs-secs",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--se-auto-evolve" => se_auto_evolve = Some(bool_value!(it, "--se-auto-evolve")),
            "--se-cycle-secs" => {
                se_cycle_secs = Some(num_value!(
                    it,
                    "--se-cycle-secs",
                    i64,
                    "want a number of seconds"
                ))
            }
            // Update switches (VERSIONING.md §7): explicit `true|false` values,
            // like --se-auto-evolve, so a caller can turn OFF as well as on.
            "--update-check" => update_check_enabled = Some(bool_value!(it, "--update-check")),
            "--update-auto" => update_auto = Some(bool_value!(it, "--update-auto")),
            // #379: the automatic-check interval in seconds. 0 explicitly
            // disables the scheduler; a negative value is refused here (the
            // old `.filter(>=0)` silently dropped it into the default).
            "--update-interval-secs" => {
                let v = num_value!(
                    it,
                    "--update-interval-secs",
                    i64,
                    "want a number of seconds (0 = off)"
                );
                if v < 0 {
                    bad_value(
                        "--update-interval-secs",
                        &v.to_string(),
                        "want a non-negative number of seconds (0 = off)",
                    );
                }
                update_interval_secs = Some(v);
            }
            "--se-ttl-secs" => {
                se_ttl_secs = Some(num_value!(
                    it,
                    "--se-ttl-secs",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--se-deep-dims" => {
                se_deep_dims = Some(num_value!(
                    it,
                    "--se-deep-dims",
                    usize,
                    "want a dimension count"
                ))
            }
            "--strategy-limit" => {
                let v = path_value!(it, "--strategy-limit");
                strategy_limits.push(v);
            }
            // E-VA: the virtual-account pot for strategies that declare
            // `atomic_batch` — `name:max_batch_usd:acct1,acct2` (repeatable;
            // "-" = no cap, the flag is then dropped with a warning because an
            // uncapped pot would silently disarm the whole atomic posture).
            "--virtual-limit" => {
                let v = path_value!(it, "--virtual-limit");
                virtual_limits.push(v);
            }
            "--enable-strategy" => {
                let v = path_value!(it, "--enable-strategy");
                if !v.trim().is_empty() {
                    enable_strategy.push(v);
                }
            }
            "--disable-strategy" => {
                let v = path_value!(it, "--disable-strategy");
                if !v.trim().is_empty() {
                    disable_strategy.push(v);
                }
            }
            // #265: the explicit opt-out from "an explicit request that resolved
            // to nothing refuses the boot". Named here (and in the flag table) so
            // it is never mistaken for an unknown argument; it changes nothing
            // about what starts enabled.
            "--allow-zero-strategies" => allow_zero_strategies = true,
            "--strategy-state" => {
                strategy_state =
                    Some(path_value!(it, "--strategy-state")).filter(|v| !v.trim().is_empty());
            }
            "--no-strategy-state" => no_strategy_state = true,
            "--strategy-dir" => {
                strategy_dir =
                    Some(path_value!(it, "--strategy-dir")).filter(|v| !v.trim().is_empty())
            }
            "--no-strategy-dir" => no_strategy_dir = true,
            // E30 (§6.4): the Lua package scan root (`none`/empty = off).
            "--lua-strategy-dir" => {
                lua_strategy_dir =
                    Some(path_value!(it, "--lua-strategy-dir")).filter(|v| !v.trim().is_empty());
            }
            "--no-lua-strategy-dir" => no_lua_strategy_dir = true,
            "--assets" => assets_arg = Some(path_value!(it, "--assets")),
            "--round-sec" => {
                round_sec = Some(num_value!(
                    it,
                    "--round-sec",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--min-round-age" => {
                min_round_age = Some(num_value!(
                    it,
                    "--min-round-age",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--min-time-left" => {
                min_time_left = Some(num_value!(
                    it,
                    "--min-time-left",
                    i64,
                    "want a number of seconds"
                ))
            }
            "--trend-confirm-sec" => {
                trend_confirm =
                    num_value!(it, "--trend-confirm-sec", i64, "want a number of seconds")
            }
            "--trend-window-floor-ms" => {
                trend_floor_ms = num_value!(
                    it,
                    "--trend-window-floor-ms",
                    i64,
                    "want a number of milliseconds"
                )
            }
            "--spread-arb-entry-factor" => {
                spread_arb_entry_factor = Some(dec_value!(
                    it,
                    "--spread-arb-entry-factor",
                    "want a decimal factor (e.g. 1.2)"
                ))
            }
            "--spread-arb-min-obi" => {
                spread_arb_min_obi = Some(dec_value!(
                    it,
                    "--spread-arb-min-obi",
                    "want a decimal between 0 and 1"
                ))
            }
            "--spread-arb-max-spread-pct" => {
                spread_arb_max_spread_pct = Some(dec_value!(
                    it,
                    "--spread-arb-max-spread-pct",
                    "want a decimal percentage"
                ))
            }
            "--spread-arb-dip-max-pct" => {
                spread_arb_dip_max_pct = Some(dec_value!(
                    it,
                    "--spread-arb-dip-max-pct",
                    "want a decimal percentage"
                ))
            }
            "--spread-arb-bounce-min-pct" => {
                spread_arb_bounce_min_pct = Some(dec_value!(
                    it,
                    "--spread-arb-bounce-min-pct",
                    "want a decimal percentage"
                ))
            }
            "--spread-arb-bounce-window-sec" => {
                spread_arb_bounce_window_sec = Some(num_value!(
                    it,
                    "--spread-arb-bounce-window-sec",
                    i64,
                    "want a number of seconds"
                ))
            }
            // E17/#264: the engine-level exit ladder. Parsed strictly — a value
            // the engine cannot honour is a startup error, never a silent
            // fallback to the shipped ladder (see `parse_exit_pct`).
            "--exit-take-profit-pct" => {
                let raw = next_value!(it, "--exit-take-profit-pct");
                match parse_exit_pct(&raw) {
                    Ok(v) => exit_take_profit_pct = Some(v),
                    Err(e) => bad_value("--exit-take-profit-pct", &raw, &e),
                }
            }
            "--exit-stop-loss-pct" => {
                let raw = next_value!(it, "--exit-stop-loss-pct");
                match parse_exit_pct(&raw) {
                    Ok(v) => exit_stop_loss_pct = Some(v),
                    Err(e) => bad_value("--exit-stop-loss-pct", &raw, &e),
                }
            }
            "--exit-trailing-min-high-pct" => {
                let raw = next_value!(it, "--exit-trailing-min-high-pct");
                match parse_exit_pct(&raw) {
                    Ok(v) => exit_trailing_min_high_pct = Some(v),
                    Err(e) => bad_value("--exit-trailing-min-high-pct", &raw, &e),
                }
            }
            "--exit-min-trail-pct" => {
                let raw = next_value!(it, "--exit-min-trail-pct");
                match parse_exit_pct(&raw) {
                    Ok(v) => exit_min_trail_pct = Some(v),
                    Err(e) => bad_value("--exit-min-trail-pct", &raw, &e),
                }
            }
            "--exit-force-sec" => {
                exit_force_sec = Some(num_value!(
                    it,
                    "--exit-force-sec",
                    i64,
                    "want a whole number of seconds (0 = off)"
                ));
            }
            "--regime-eval" => regime_eval = Some(path_value!(it, "--regime-eval")),
            "--regime-report" => regime_report = Some(path_value!(it, "--regime-report")),
            "--regime-token" => regime_token = Some(path_value!(it, "--regime-token")),
            // #352: the on-chain pull subcommand and its knobs.
            "--onchain-pull" => onchain_pull = Some(path_value!(it, "--onchain-pull")),
            "--onchain-start" => onchain_start = Some(path_value!(it, "--onchain-start")),
            "--onchain-end" => onchain_end = Some(path_value!(it, "--onchain-end")),
            "--onchain-asset" => onchain_asset = Some(path_value!(it, "--onchain-asset")),
            "--onchain-market" => onchain_market = Some(path_value!(it, "--onchain-market")),
            "--onchain-fills" => onchain_fills = Some(path_value!(it, "--onchain-fills")),
            "--onchain-out-dir" => onchain_out_dir = Some(path_value!(it, "--onchain-out-dir")),
            // #361: the offline blueprint compiler and its output knob.
            "--blueprint-compile" => {
                blueprint_compile = Some(path_value!(it, "--blueprint-compile"))
            }
            "--blueprint-out" => blueprint_out = Some(path_value!(it, "--blueprint-out")),
            "--regime-max-tokens" => {
                regime_max_tokens =
                    num_value!(it, "--regime-max-tokens", usize, "want a token count")
            }
            "--regime-window-sec" => {
                regime_window_sec =
                    num_value!(it, "--regime-window-sec", i64, "want a number of seconds")
            }
            "--regime-min-trend-ticks" => {
                regime_min_trend_ticks = dec_value!(
                    it,
                    "--regime-min-trend-ticks",
                    "want a decimal number of ticks"
                )
            }
            "--regime-min-efficiency" => {
                regime_min_efficiency = dec_value!(
                    it,
                    "--regime-min-efficiency",
                    "want a decimal between 0 and 1"
                )
            }
            "--regime-volatile-mad-ticks" => {
                regime_volatile_mad_ticks = dec_value!(
                    it,
                    "--regime-volatile-mad-ticks",
                    "want a decimal number of ticks"
                )
            }
            "--regime-confirmations" => {
                regime_confirmations = num_value!(
                    it,
                    "--regime-confirmations",
                    u32,
                    "want a confirmation count"
                )
            }
            "--max-positions" => {
                max_positions = num_value!(it, "--max-positions", usize, "want a position count")
            }
            // #363: per-asset entries (the @Almach replay needs > 1). Absent
            // = the execution-policy file decides; 0 is refused at boot
            // (it would refuse every entry).
            "--max-positions-per-asset" => {
                max_positions_per_asset = Some(num_value!(
                    it,
                    "--max-positions-per-asset",
                    u32,
                    "want a position count"
                ))
            }
            // #380: an explicit policy file wins over the install-root
            // resolution of the shipped default. A relative spelling is
            // honoured verbatim (resolved against the process cwd, like any
            // explicit path) — the install-root lookup is for the DEFAULT
            // only, never a second-guessing of the operator's choice.
            "--execution-policy" => {
                execution_policy = Some(path_value!(it, "--execution-policy"));
            }
            // #173: absolute USD cap for the day's realized loss (0 = none).
            "--max-daily-loss" => {
                max_daily_loss = Some(dec_value!(
                    it,
                    "--max-daily-loss",
                    "want a USD amount (0 = no budget)"
                ))
            }
            // #173: cap as a percentage of the day's opening cash equity.
            "--max-daily-loss-pct" => {
                max_daily_loss_pct = Some(dec_value!(
                    it,
                    "--max-daily-loss-pct",
                    "want a percentage (0 = off)"
                ))
            }
            "--market" => {
                if let Some(m) = it.next() {
                    markets.push(m);
                }
            }
            "--event-archive" => event_archive = Some(path_value!(it, "--event-archive")),
            "--no-event-archive" => no_event_archive = true,
            "--event-archive-max-mb" => {
                event_archive_max_mb = Some(num_value!(
                    it,
                    "--event-archive-max-mb",
                    u64,
                    "want a size in MiB (0 = unlimited)"
                ))
            }
            "--event-archive-rotate-mb" => {
                event_archive_rotate_mb = Some(num_value!(
                    it,
                    "--event-archive-rotate-mb",
                    u64,
                    "want a size in MiB (0 = never rotate)"
                ))
            }
            "--event-archive-min-free-mb" => {
                event_archive_min_free_mb = Some(num_value!(
                    it,
                    "--event-archive-min-free-mb",
                    u64,
                    "want a size in MiB (0 = no guard)"
                ))
            }
            // Strict, like `--max-orderbook-stale-ms` below: a value the engine
            // cannot honour is a startup error, never a silent fallback. The
            // silent fallback here was worse than usual — a typo produced the
            // default clock, and an operator who believed they had DISARMED the
            // maker→taker escalation was running it.
            "--entry-maker-timeout-ms" => {
                entry_maker_timeout_ms = num_value!(
                    it,
                    "--entry-maker-timeout-ms",
                    i64,
                    "want a whole number of milliseconds (0 uses the 5000 ms default, a negative value never escalates)"
                );
            }
            // #205: how stale an orderbook may be before the engine refuses to
            // price off it. Parsed strictly — a value the engine cannot honour is
            // a startup error, never a silent fallback: an operator who believes
            // they set a budget must not be running a different one.
            "--max-orderbook-stale-ms" => {
                max_orderbook_stale_ms = Some(num_value!(
                    it,
                    "--max-orderbook-stale-ms",
                    i64,
                    "want a whole number of milliseconds (0 disables the freshness check)"
                ));
            }
            // 任务二 1-3: the NTP clock-offset probe. The empty string is the
            // documented "off" spelling (zero network tasks spawned — the
            // replay-safe posture); a NEGATIVE interval is refused, an explicit
            // 0 disables the periodic loop (a single boot-time probe only).
            "--ntp-host" => {
                ntp_host = Some(path_value!(it, "--ntp-host").trim().to_string());
            }
            "--ntp-interval-secs" => {
                let v = num_value!(
                    it,
                    "--ntp-interval-secs",
                    i64,
                    "want a number of seconds (0 = off)"
                );
                if v < 0 {
                    bad_value(
                        "--ntp-interval-secs",
                        &v.to_string(),
                        "want a non-negative number of seconds (0 = off)",
                    );
                }
                ntp_interval_secs = Some(v);
            }
            "--backtest" => backtest = Some(path_value!(it, "--backtest")),
            "--backtest-report" => backtest_report = Some(path_value!(it, "--backtest-report")),
            "--backtest-tick-ms" => {
                backtest_tick_ms = num_value!(
                    it,
                    "--backtest-tick-ms",
                    i64,
                    "want a whole number of milliseconds"
                )
            }
            "--backtest-tail-ms" => {
                backtest_tail_ms = num_value!(
                    it,
                    "--backtest-tail-ms",
                    i64,
                    "want a whole number of milliseconds"
                )
            }
            // `<strategy>:<knob>=<value>`; repeatable. A replay-only counterfactual.
            "--backtest-knob" => {
                let spec = next_value!(it, "--backtest-knob");
                match parse_backtest_knob(&spec) {
                    Ok(k) => backtest_knobs.push(k),
                    Err(e) => bad_value("--backtest-knob", &spec, &e),
                }
            }
            // #203: which taker-fee schedule a REPLAY charges. Not a live knob —
            // see `Args::fee_model`.
            "--fee-model" => {
                let name = next_value!(it, "--fee-model");
                match blitzkrieg_core::exit_policy::fee_schedule_by_name(&name) {
                    Some(s) => fee_model = Some(s),
                    None => bad_value(
                        "--fee-model",
                        &name,
                        &format!(
                            "unknown schedule; known: {}",
                            blitzkrieg_core::exit_policy::FEE_SCHEDULE_NAMES.join(", ")
                        ),
                    ),
                }
            }
            "--slippage-ticks" => {
                slippage_ticks = num_value!(it, "--slippage-ticks", u32, "want a tick count")
            }
            // Replay maintenance-gating fast path: a flag, no value. Not a live
            // knob — see `Args::backtest_fast` (refusal below, with --fee-model).
            "--backtest-fast" => backtest_fast = true,
            // Maker-rebate model: a flag, no value. Replay-only like
            // --backtest-fast (refused below, same shape).
            "--maker-rebate" => maker_rebate = true,
            // #351 backtest mode: `mine` (zero latency, keep real friction),
            // `verify` (a real-latency dial set), `sweep` (latency ladder
            // 0..300). Only meaningful with --backtest; the mode re-prices
            // the fill model's LATENCY dials only — slippage/fees stay as
            // configured, so the friction between modes is comparable. A
            // MISSING value keeps the historic single-run shape (empty mode =
            // no mode runner), which is the documented default, not a
            // fallback a typo could hit.
            "--backtest-mode" => {
                if let Some(raw) = it.next() {
                    match raw.as_str() {
                        "mine" | "verify" | "sweep" => backtest_mode = raw,
                        _ => bad_value(
                            "--backtest-mode",
                            &raw,
                            "want mine|verify|sweep (see --help; requires --backtest)",
                        ),
                    }
                }
            }
            // #351 verify mode's one dial: the round-trip the strategy
            // actually trades through (the HK VPS number is 286 ms).
            "--verify-latency-ms" => {
                verify_latency_ms = num_value!(
                    it,
                    "--verify-latency-ms",
                    i64,
                    "want a whole number of milliseconds"
                )
            }
            "--latency-ms" => {
                latency_ms = num_value!(
                    it,
                    "--latency-ms",
                    i64,
                    "want a whole number of milliseconds"
                )
            }
            // #351: taker-leg travel dials (see Args docs above).
            "--taker-latency-ms" => {
                taker_latency_ms = num_value!(
                    it,
                    "--taker-latency-ms",
                    i64,
                    "want a whole number of milliseconds"
                )
            }
            "--taker-rtt-ms" => {
                taker_rtt_ms = num_value!(
                    it,
                    "--taker-rtt-ms",
                    i64,
                    "want a whole number of milliseconds"
                )
            }
            "--fill-prob-bps" => {
                fill_prob_bps = Some(num_value!(
                    it,
                    "--fill-prob-bps",
                    u32,
                    "want a probability in bps of 10000"
                ))
            }
            "--maker-depth-share-bps" => {
                maker_depth_share_bps = Some(num_value!(
                    it,
                    "--maker-depth-share-bps",
                    u32,
                    "want a depth share in bps of 10000"
                ))
            }
            // Dry-mode test hooks (#175 gate). Both are inert unless a mode that
            // simulates the redemption is running, so a live deployment can pass
            // them and see no change at all.
            "--dry-redeem-fail" => {
                dry_redeem_fail = num_value!(
                    it,
                    "--dry-redeem-fail",
                    u32,
                    "want a failure count (0 = off)"
                )
            }
            "--dry-redeem-manual" => dry_redeem_manual = true,
            // Consumed by the pre-scan; named here so they are not reported as
            // unknown flags.
            "--config" => {
                it.next();
            }
            "--no-config" => {}
            other if other.starts_with("--config=") => {}
            // Read by the pre-scan above, for the same reason (`argv` order must
            // not change what it means) and named here for the same reason.
            blitzkrieg_core::cli::ALLOW_UNKNOWN_ARGS => {}
            // #228: the fallback REFUSES. It used to print
            // `ignoring unknown argument: <flag>` and start the core anyway,
            // which turned a misspelled `--readonly` into a write-mode boot and a
            // misspelled `--max-order-notional` into the shipped cap — a silent
            // downgrade of exactly the flags that exist to bound what this
            // process may do. The refusal is decided by `cli::classify_unknown_arg`
            // (unit-tested, with the "did you mean" spelling and, for a safety
            // flag, what leaving it out would have meant); this arm only prints
            // and exits. `--allow-unknown-args` opts out, and never for a
            // near-miss of a safety flag.
            other => match blitzkrieg_core::cli::classify_unknown_arg(other, allow_unknown) {
                blitzkrieg_core::cli::UnknownArg::Reject(msg) => {
                    eprintln!("{msg}");
                    std::process::exit(2);
                }
                blitzkrieg_core::cli::UnknownArg::Ignore(msg) => eprintln!("{msg}"),
            },
        }
    }

    // ── Resolve the file-settable settings: CLI > env > TOML > default ──────
    use blitzkrieg_core::config::{Sourced, pick};
    let mut report: Vec<String> = Vec::new();

    macro_rules! resolve {
        ($label:literal, $cli:expr, $env:literal, $toml:expr, $default:expr) => {{
            let s: Sourced<_> = pick($cli, env.num($env), $toml, $default);
            if s.is_explicit() {
                report.push(format!("{}={} ({})", $label, s.value, s.source.as_str()));
            }
            s.value
        }};
    }
    // Same, for values that are not Display (the asset list).
    macro_rules! resolve_shown {
        ($label:literal, $cli:expr, $env:expr, $toml:expr, $default:expr) => {{
            let s = pick($cli, $env, $toml, $default);
            if s.is_explicit() {
                report.push(format!("{}={:?} ({})", $label, s.value, s.source.as_str()));
            }
            s.value
        }};
    }
    // For settings whose "not given" state is itself `None`. The override has to
    // be wrapped one level so `pick` can tell "a layer supplied None" from
    // "no layer spoke" — resolving those two is the whole job of this layer.
    macro_rules! resolve_opt {
        ($label:literal, $cli:expr, $env:expr, $toml:expr) => {{
            let s = pick($cli.map(Some), $env.map(Some), $toml.map(Some), None);
            if s.is_explicit() {
                let shown = s
                    .value
                    .as_ref()
                    .map(|v| format!("{v:?}"))
                    .unwrap_or_else(|| "none".to_string());
                report.push(format!("{}={} ({})", $label, shown, s.source.as_str()));
            }
            s.value
        }};
    }

    let round_sec = resolve!(
        "engine.round_sec",
        round_sec,
        "BK_ROUND_SEC",
        file.round_sec,
        DEFAULT_ROUND_SEC
    );
    let min_round_age = resolve!(
        "engine.min_round_age_sec",
        min_round_age,
        "BK_MIN_ROUND_AGE_SEC",
        file.min_round_age_sec,
        DEFAULT_MIN_ROUND_AGE_SEC
    );
    let min_time_left = resolve!(
        "engine.min_time_left_sec",
        min_time_left,
        "BK_MIN_TIME_LEFT_SEC",
        file.min_time_left_sec,
        DEFAULT_MIN_TIME_LEFT_SEC
    );
    let assets: Vec<String> = resolve_shown!(
        "engine.assets",
        assets_arg.as_deref().map(split_assets),
        env.text("BK_ASSETS").map(|v| split_assets(&v)),
        file.assets.clone(),
        DEFAULT_ASSETS
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
    );

    // Shadow Evolution. The file speaks in MINUTES for the two windows; the
    // kernel speaks in seconds, so the conversion happens exactly here and
    // nowhere else.
    let se_eval_window_secs = resolve_opt!(
        "shadow_evolution.evaluation_window_secs",
        env.num::<i64>("BK_SE_EVAL_WINDOW_SECS"),
        None::<i64>,
        file.shadow.evaluation_window_minutes.map(|m| m * 60)
    );
    let se_min_obs_secs = resolve_opt!(
        "shadow_evolution.min_observation_secs",
        se_min_obs_secs,
        None::<i64>,
        file.shadow.min_observation_minutes.map(|m| m * 60)
    );
    let se_cooldown_secs = resolve_opt!(
        "shadow_evolution.cooldown_secs",
        se_cooldown_secs,
        env.num::<i64>("BK_SE_COOLDOWN_SECS"),
        file.shadow.cooldown_minutes.map(|m| m * 60)
    );
    let se_min_samples = resolve_opt!(
        "shadow_evolution.min_sample_count",
        se_min_samples,
        env.num::<u32>("BK_SE_MIN_SAMPLES"),
        file.shadow.min_sample_count
    );
    let se_variant_count = resolve_opt!(
        "shadow_evolution.variant_count",
        None::<usize>,
        env.num::<usize>("BK_SE_VARIANT_COUNT"),
        file.shadow.variant_count
    );
    let se_audit_dir = resolve_opt!(
        "shadow_evolution.audit_dir",
        None::<String>,
        env.text("BK_SE_AUDIT_DIR"),
        file.shadow.audit_dir.clone()
    );
    let se_min_win_rate = resolve_opt!(
        "shadow_evolution.min_win_rate_improvement",
        None::<Decimal>,
        env.num::<Decimal>("BK_SE_MIN_WIN_RATE"),
        file.shadow.min_win_rate_improvement
    );
    let se_min_profit_factor = resolve_opt!(
        "shadow_evolution.min_profit_factor_improvement",
        None::<Decimal>,
        env.num::<Decimal>("BK_SE_MIN_PROFIT_FACTOR"),
        file.shadow.min_profit_factor_improvement
    );
    // Lock 2 is a safety lock, not a tuning surface: a file may tighten the
    // per-step gradient, never widen it. A value above the ceiling is refused
    // outright rather than clamped — silently ignoring a request to loosen a
    // safety lock is the kind of thing an operator must be told about.
    let se_max_gradient = match pick(
        None::<Decimal>,
        env.num::<Decimal>("BK_SE_MAX_GRADIENT"),
        file.shadow.max_gradient,
        blitzkrieg_core::config::MAX_GRADIENT_CEILING,
    ) {
        s if !s.is_explicit() => None,
        s if s.value <= blitzkrieg_core::config::MAX_GRADIENT_CEILING
            && s.value > Decimal::ZERO =>
        {
            report.push(format!(
                "shadow_evolution.max_gradient={} ({})",
                s.value,
                s.source.as_str()
            ));
            Some(s.value)
        }
        s => {
            eprintln!(
                "blitzkrieg-core: ignoring max_gradient {}: must be > 0 and <= {} \
                 (Lock 2 is not widenable)",
                s.value,
                blitzkrieg_core::config::MAX_GRADIENT_CEILING
            );
            None
        }
    };
    // E13 proposal workflow. The file speaks in MINUTES for the two clocks
    // (same convention as the other windows); the kernel speaks in seconds.
    let se_auto_evolve = resolve_opt!(
        "shadow_evolution.auto_evolve",
        se_auto_evolve,
        env.text("BK_SE_AUTO_EVOLVE")
            .or_else(|| env.text("SHADOW_AUTO_EVOLVE"))
            .and_then(|v| match v.trim() {
                "true" | "1" | "on" | "yes" => Some(true),
                "false" | "0" | "off" | "no" => Some(false),
                _ => None,
            }),
        file.shadow.auto_evolve
    );
    // VERSIONING.md §7: CLI > env > update.toml > built-in default (off).
    let update_check_enabled = resolve_opt!(
        "update.check_enabled",
        update_check_enabled,
        env.text("BLITZKRIEG_UPDATE_CHECK")
            .and_then(|v| match v.trim() {
                "true" | "1" | "on" | "yes" => Some(true),
                "false" | "0" | "off" | "no" => Some(false),
                _ => None,
            }),
        file.update.check_enabled
    );
    let update_auto = resolve_opt!(
        "update.auto_update",
        update_auto,
        env.text("BLITZKRIEG_UPDATE_AUTO")
            .and_then(|v| match v.trim() {
                "true" | "1" | "on" | "yes" => Some(true),
                "false" | "0" | "off" | "no" => Some(false),
                _ => None,
            }),
        file.update.auto_update
    );
    // #379: the automatic-check clock. Same chain; 0 = scheduler off. A
    // persisted interval is NOT used here by design — the clock is
    // restart-to-change configuration, so a UI toggle cannot silently arm it.
    let update_interval_secs = resolve_opt!(
        "update.interval_secs",
        update_interval_secs,
        env.text("BLITZKRIEG_UPDATE_INTERVAL_SECS")
            .and_then(|v| v.trim().parse::<i64>().ok()),
        file.update.interval_secs
    )
    .filter(|s| *s >= 0);
    let se_cycle_secs = resolve_opt!(
        "shadow_evolution.evolution_cycle_secs",
        se_cycle_secs,
        env.num::<i64>("BK_SE_CYCLE_SECS"),
        file.shadow.evolution_cycle_minutes.map(|m| m * 60)
    );
    let se_ttl_secs = resolve_opt!(
        "shadow_evolution.proposal_ttl_secs",
        se_ttl_secs,
        env.num::<i64>("BK_SE_TTL_SECS"),
        file.shadow.proposal_ttl_minutes.map(|m| m * 60)
    );
    let se_deep_dims = resolve_opt!(
        "shadow_evolution.deep_dims",
        se_deep_dims,
        env.num::<usize>("BK_SE_DEEP_DIMS"),
        file.shadow.deep_dims
    );

    // ── #173: the daily-loss breaker's budget ───────────────────────────────
    // Resolved CLI > env > compiled default, then echoed unconditionally below.
    // The default is RELATIVE (`max_daily_loss_equity_pct`), because an absolute
    // default cannot be meaningful on every account size: the old hard-coded
    // 200 USD silently disabled the breaker on a 4.8 USDC live book (P0-2).
    let daily_loss_defaults = PositionConfig::default();
    let daily_loss_usd = pick(
        max_daily_loss,
        env.num::<Decimal>("BK_MAX_DAILY_LOSS"),
        // E26 (§4.2): the [risk] drawdown key IS this budget's TOML face —
        // one budget, resolved through the one chain, never a second counter.
        file.risk.max_daily_drawdown_usd,
        daily_loss_defaults.max_daily_loss_usd,
    );
    let daily_loss_pct = pick(
        max_daily_loss_pct,
        env.num::<Decimal>("BK_MAX_DAILY_LOSS_PCT"),
        None,
        daily_loss_defaults.max_daily_loss_equity_pct,
    );
    if daily_loss_usd.is_explicit() {
        report.push(format!(
            "positions.max_daily_loss_usd={} ({})",
            daily_loss_usd.value,
            daily_loss_usd.source.as_str()
        ));
    }
    if daily_loss_pct.is_explicit() {
        report.push(format!(
            "positions.max_daily_loss_equity_pct={} ({})",
            daily_loss_pct.value,
            daily_loss_pct.source.as_str()
        ));
    }
    let mut daily_loss_echo = vec![format!(
        "daily loss breaker: absolute cap ${} ({}), relative cap {}% of the day's opening cash equity ({}); the TIGHTER applies, 0 = off",
        daily_loss_usd.value,
        daily_loss_usd.source.as_str(),
        daily_loss_pct.value,
        daily_loss_pct.source.as_str()
    )];
    if daily_loss_usd.value <= Decimal::ZERO && daily_loss_pct.value <= Decimal::ZERO {
        daily_loss_echo.push(
            "daily loss breaker: DISABLED (both caps are 0) — pass --max-daily-loss <usd> or \
             --max-daily-loss-pct <pct> to arm it"
                .to_string(),
        );
    }

    // ── #202: the account-relative sizing / per-order bound ─────────────────
    // Same treatment as the daily-loss budget above, and for the same reason: a
    // bound that is only in the source is not a bound anyone can verify against
    // the account it runs on. `size_pct` also changes ORDER SIZES, so the boot
    // log has to say whether the absolute path or the equity path is in force.
    let size_pct = pick(
        size_pct,
        env.num::<Decimal>("BK_SIZE_PCT"),
        None,
        Decimal::ZERO,
    );
    let notional_pct = pick(
        max_order_notional_pct,
        env.num::<Decimal>("BK_MAX_ORDER_NOTIONAL_PCT"),
        None,
        Decimal::ZERO,
    );
    if size_pct.is_explicit() {
        report.push(format!(
            "engine.size_pct={} ({})",
            size_pct.value,
            size_pct.source.as_str()
        ));
    }
    if notional_pct.is_explicit() {
        report.push(format!(
            "risk.max_order_notional_pct={} ({})",
            notional_pct.value,
            notional_pct.source.as_str()
        ));
    }
    let mut sizing_echo = vec![
        format!(
            "order sizing: per-entry budget {} (0 = off: the absolute size_usd path, order sizes unchanged), \
             per-order notional cap {}% of the account's cash equity (0 = off)",
            if size_pct.value > Decimal::ZERO {
                format!(
                    "{}% of cash equity ({}; replaces size_usd)",
                    size_pct.value,
                    size_pct.source.as_str()
                )
            } else {
                format!(
                    "absolute size_usd ({}; --size-pct/BK_SIZE_PCT off)",
                    size_pct.source.as_str()
                )
            },
            notional_pct.value,
        ),
        format!(
            "per-order bounds in force: absolute cap ${}, plus {} (relative cap ON: an over-cap order is REJECTED, never truncated, and closing intents are exempt)",
            max_order_notional,
            if notional_pct.value > Decimal::ZERO {
                format!("{}% of the account's cash equity", notional_pct.value)
            } else {
                "NO equity-relative cap".to_string()
            }
        ),
    ];
    if notional_pct.value <= Decimal::ZERO {
        sizing_echo.push(
            "per-order bounds in force: this account has NO bound as a share of itself — one order may commit \
             the whole of `max_shares × max_price` (10 shares × 1.00 = 10.00 USD by default, 208% of a 4.8 USDC \
             book). Pass --max-order-notional-pct <pct> or run `node scripts/risk-sizing-check.mjs` to see the number"
                .to_string(),
        );
    }

    // ── #205: the orderbook freshness budget ────────────────────────────────
    // How long after the feed goes quiet the engine stops pricing off its books.
    // A runtime setting because it is a venue/ops property, not a code property:
    // a slow poller or a venue that batches updates needs a wider budget, and a
    // deployment tightening it must not need a rebuild to be obeyed.
    let stale = pick(
        max_orderbook_stale_ms,
        env.num::<i64>("BK_MAX_ORDERBOOK_STALE_MS"),
        None,
        blitzkrieg_core::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS,
    );
    // Invalid values fail the boot loudly instead of falling back: a silently
    // substituted budget is the failure this knob exists to remove.
    if let Some(why) = stale_budget_rejection(stale.value) {
        eprintln!(
            "blitzkrieg-core: {} gives the orderbook freshness budget {why}",
            stale.source.as_str()
        );
        std::process::exit(2);
    }
    if stale.is_explicit() {
        report.push(format!(
            "engine.max_orderbook_stale_ms={} ({})",
            stale.value,
            stale.source.as_str()
        ));
    }
    let orderbook_stale_echo = vec![if stale.value > 0 {
        format!(
            "orderbook freshness: the engine refuses to price off a book older than {} ms ({}); past that it \
             stops taking entries until the feed catches up (closing intents are never blocked by this)",
            stale.value,
            stale.source.as_str()
        )
    } else {
        format!(
            "orderbook freshness: CHECK DISABLED ({}; any book age is accepted) — the engine keeps pricing off \
             the last book it saw however old it is. Pass --max-orderbook-stale-ms <ms> to arm it",
            stale.source.as_str()
        )
    }];

    // ── 任务二 1-3: the NTP clock-offset probe ──────────────────────────────
    // CLI > env > default. The empty string is the documented "off" spelling:
    // `probe::should_spawn_ntp` refuses it, so a configured-off probe spawns
    // ZERO network tasks (the replay-safe posture). An explicit --ntp-host ""
    // and an absent host are pinned apart by the snapshot test.
    let ntp = pick(
        ntp_host,
        env.text("BK_NTP_HOST"),
        None::<String>,
        blitzkrieg_core::probe::DEFAULT_NTP_HOST.to_string(),
    );
    let ntp_interval = pick(
        ntp_interval_secs,
        env.num::<i64>("BK_NTP_INTERVAL_SECS"),
        None::<i64>,
        blitzkrieg_core::probe::DEFAULT_NTP_INTERVAL_SECS,
    );
    if ntp_interval.value < 0 {
        // Parse arms already refuse a negative CLI value; a negative from the
        // env layer lands here (same fail-loud posture, never a silent default).
        eprintln!(
            "blitzkrieg-core: BK_NTP_INTERVAL_SECS={} is negative — the NTP probe \
             interval cannot be negative (0 = off)",
            ntp_interval.value
        );
        std::process::exit(2);
    }
    let ntp_echo = vec![if blitzkrieg_core::probe::should_spawn_ntp(&ntp.value)
        && ntp_interval.value > 0
    {
        format!(
            "environment probes: NTP clock-offset probe ON — every {} s against \
             {} ({}/{}); |offset| >= 50 ms warns, >= 100 ms flags probe.ntp.skewRed",
            ntp_interval.value,
            ntp.value,
            ntp.source.as_str(),
            ntp_interval.source.as_str()
        )
    } else {
        format!(
            "environment probes: NTP clock-offset probe OFF (host={}, interval={}s) — \
             zero network tasks spawned; stats probe.ntp stays cold",
            if ntp.value.trim().is_empty() {
                "(empty = off)"
            } else {
                ntp.value.as_str()
            },
            ntp_interval.value
        )
    }];

    // ── E17/#264: the engine-level exit ladder ──────────────────────────────
    // Same chain as every other file-settable knob (CLI > TOML > compiled
    // default). No env layer: `--spread-arb-*`, the sibling this pipeline
    // mirrors, has none either, and the durable layer for a ladder is the file.
    // The defaults are read off `ExitConfig::default()` rather than re-typed, so
    // "unchanged" is a property of the code, not of a comment.
    let exit_defaults = blitzkrieg_core::exit_policy::ExitConfig::default();
    let exit_knob = |report: &mut Vec<String>,
                     label: &str,
                     cli: Option<Decimal>,
                     toml: Option<Decimal>,
                     default: Decimal| {
        let s = pick(cli, None::<Decimal>, toml, default);
        if s.is_explicit() {
            report.push(format!("{label}={} ({})", s.value, s.source.as_str()));
        }
        s.value
    };
    let exit_take_profit_pct = exit_knob(
        &mut report,
        "exit.take_profit_pct",
        exit_take_profit_pct,
        file.exit.take_profit_pct,
        exit_defaults.take_profit_pct,
    );
    let exit_stop_loss_pct = exit_knob(
        &mut report,
        "exit.stop_loss_pct",
        exit_stop_loss_pct,
        file.exit.stop_loss_pct,
        exit_defaults.stop_loss_pct,
    );
    let exit_trailing_min_high_pct = exit_knob(
        &mut report,
        "exit.trailing_min_high_pct",
        exit_trailing_min_high_pct,
        file.exit.trailing_min_high_pct,
        exit_defaults.trailing_min_high_pct,
    );
    let exit_min_trail_pct = exit_knob(
        &mut report,
        "exit.min_trail_pct",
        exit_min_trail_pct,
        file.exit.min_trail_pct,
        exit_defaults.min_trail_pct,
    );
    // force_exit_sec is an INT knob (seconds), so the Decimal-typed `exit_knob`
    // closure above does not fit; the chain is the same CLI > TOML > default.
    let exit_force_sec = {
        let s = pick(
            exit_force_sec,
            None::<i64>,
            file.exit.force_exit_sec,
            exit_defaults.force_exit_sec,
        );
        if s.is_explicit() {
            report.push(format!(
                "exit.force_exit_sec={} ({})",
                s.value,
                s.source.as_str()
            ));
        }
        s.value
    };

    // ── #269: is the evolution engine running? ──────────────────────────────
    // The one shadow-evolution setting whose origin was never printed, and the
    // question the switch-conflict warning is about ("the file says off" is only
    // a kill switch if you can see whether it took). Echoed unconditionally, like
    // the breaker's budget and the freshness budget above: a boot log must answer
    // it even for a run that configured nothing.
    let se_engine = pick(
        None::<bool>,
        env.num::<bool>("BK_SHADOW_EVOLUTION")
            .or_else(|| env.num::<bool>("SHADOW_ENABLED")),
        file.shadow.enabled,
        false,
    );
    let shadow_evolution_enabled = shadow_evolution_flag || se_engine.value;
    if se_engine.is_explicit() {
        report.push(format!(
            "shadow_evolution.enabled={} ({})",
            se_engine.value,
            se_engine.source.as_str()
        ));
    }
    let shadow_evolution_echo = vec![format!(
        "shadow evolution: evaluator {} at startup ({}); the persisted runtime switch in \
         state.json wins over it, and any disagreement is reported below",
        if shadow_evolution_enabled {
            "ON"
        } else {
            "OFF"
        },
        if shadow_evolution_flag {
            "from the --shadow-evolution flag"
        } else {
            se_engine.source.as_str()
        },
    )];

    Args {
        socket,
        mode,
        readonly,
        tick_ms,
        seed_balance,
        max_order_notional,
        max_open_notional,
        min_shares,
        max_shares,
        size_pct: size_pct.value,
        max_order_notional_pct: notional_pct.value,
        sizing_echo,
        max_orderbook_stale_ms: stale.value,
        orderbook_stale_echo,
        ntp_host: ntp.value,
        ntp_interval_secs: ntp_interval.value,
        markets,
        auto_exits,
        max_positions,
        max_positions_per_asset,
        execution_policy,
        daily_loss_usd: daily_loss_usd.value,
        daily_loss_pct: daily_loss_pct.value,
        daily_loss_source: daily_loss_usd.source,
        daily_loss_echo,
        engine,
        min_round_age,
        min_time_left,
        trend_confirm,
        trend_floor_ms,
        spread_arb_entry_factor,
        spread_arb_min_obi,
        spread_arb_max_spread_pct,
        spread_arb_dip_max_pct,
        spread_arb_bounce_min_pct,
        spread_arb_bounce_window_sec,
        exit_take_profit_pct,
        exit_stop_loss_pct,
        exit_trailing_min_high_pct,
        exit_min_trail_pct,
        exit_force_sec,
        feed_ws,
        net_check,
        replay,
        replay_near_miss,
        round_sec,
        near_miss_path,
        trade_log,
        no_trade_log,
        order_log,
        no_order_log,
        position_log,
        no_position_log,
        ledger_journal_dir: if no_ledger_journal {
            None
        } else {
            Some("data/ledger".to_string())
        },
        no_intent_audit,
        allow_shared_data,
        market_plugin,
        discovery,
        // Resolved once, above, so the boot echo cannot disagree with what the
        // service is actually handed.
        shadow_evolution: shadow_evolution_enabled,
        assets,
        se_min_samples,
        se_cooldown_secs,
        se_min_obs_secs,
        se_eval_window_secs,
        se_max_gradient,
        se_variant_count,
        se_audit_dir,
        se_min_win_rate,
        se_min_profit_factor,
        se_auto_evolve,
        se_cycle_secs,
        se_ttl_secs,
        se_deep_dims,
        update_check_enabled,
        update_auto,
        update_interval_secs,
        ntp_echo,
        shadow_evolution_echo,
        strategy_limits,
        virtual_limits,
        enable_strategy,
        disable_strategy,
        allow_zero_strategies,
        strategy_state: if no_strategy_state {
            None
        } else {
            Some(strategy_state.unwrap_or_else(|| "data/strategy-state.json".to_string()))
        },
        strategy_dir: if no_strategy_dir {
            None
        } else {
            strategy_dir
                .or_else(|| env.text("BK_STRATEGY_DIR"))
                .or_else(default_strategy_dir)
        },
        lua_strategy_dir: if no_lua_strategy_dir {
            None
        } else {
            lua_strategy_dir
                .or_else(|| env.text("BK_LUA_STRATEGY_DIR"))
                .or_else(default_lua_strategy_dir)
        },
        event_archive,
        no_event_archive,
        event_archive_max_mb,
        event_archive_rotate_mb,
        event_archive_min_free_mb,
        entry_maker_timeout_ms,
        backtest,
        backtest_report,
        backtest_tick_ms,
        backtest_tail_ms,
        backtest_knobs,
        backtest_mode,
        verify_latency_ms,
        fee_model,
        maker_rebate,
        backtest_fast,
        regime_eval,
        regime_report,
        regime_token,
        regime_max_tokens,
        regime_window_sec,
        regime_min_trend_ticks,
        regime_min_efficiency,
        regime_volatile_mad_ticks,
        regime_confirmations,
        onchain_pull,
        onchain_start,
        onchain_end,
        onchain_asset,
        onchain_market,
        onchain_fills,
        onchain_out_dir,
        blueprint_compile,
        blueprint_out,
        slippage_ticks,
        latency_ms,
        taker_latency_ms,
        taker_rtt_ms,
        fill_prob_bps,
        maker_depth_share_bps,
        dry_redeem_fail,
        dry_redeem_manual,
        config_report: report,
    }
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
