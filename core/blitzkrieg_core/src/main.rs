//! blitzkrieg-core entrypoint.
//!
//! Serves the trading core over a Unix-domain-socket JSON-RPC 2.0 channel.
//! Node spawns this process and owns its lifecycle; credentials are read from
//! the environment inside the process (never supplied by Node).
//!
//! Usage:
//!   blitzkrieg-core --socket <path> [--mode dry|live] [--tick-ms 50]
//!                   [--readonly]
//!                   [--seed-balance 10000] [--max-order-notional 100]
//!                   [--min-shares 10] [--max-shares 10]
//!                   [--market-plugin <name>]
//!                   [--trade-log <path>] [--no-trade-log]
//!                   [--order-log <path>] [--no-order-log]
//!                   [--position-log <path>] [--no-position-log]
//!                   [--allow-shared-data]
//!                   [--near-miss-path <path>]
//!                   [--event-archive <path>] [--no-event-archive]
//!                   [--event-archive-max-mb 0]
//!                   [--event-archive-rotate-mb 256]
//!                   [--event-archive-min-free-mb 5120]
//!                   [--entry-maker-timeout-ms 5000]
//!                   [--max-orderbook-stale-ms 8000]
//!                   [--slippage-ticks 0] [--latency-ms 0] [--fill-prob-bps 10000]
//!                   [--maker-depth-share-bps 10000]
//!                   [--dry-redeem-fail 0] [--dry-redeem-manual]
//!                   [--net-check]
//!
//! `--max-orderbook-stale-ms <ms>` (env `BK_MAX_ORDERBOOK_STALE_MS`) is how old
//! an orderbook may be before the engine refuses to price off it — the knob that
//! decides how long after the feed goes quiet the bot stops taking entries.
//! Default 8000 (the value every deployment has always run with). `0` disables
//! the check entirely (any book age is accepted); a negative value, a
//! non-numeric value or anything past the 600000 ms ceiling is a startup error
//! (exit 2), never a silent fallback. Closing intents are never blocked by it.
//!
//! `--readonly` is a promise, not a convention: the venue order egress is never
//! constructed, so no code path in this process can send an order. It outranks
//! `--mode live` / `DRY_RUN=false` (giving both is not an error — the read-only
//! promise simply wins) and settles orders like dry, since a ledger waiting on a
//! venue that was never started would never move.
//!
//! #199 — the data-directory latch. The ledgers ARE the accounting truth the
//! risk limits rest on, so a second writer is not a logging accident: it breaks
//! the trade/order/position identities `max_daily_loss_usd`, the loss breakers
//! and the reconcile audit are computed from, and polluted rows are
//! indistinguishable from real ones. At startup this process therefore claims
//! the parent directory of every EFFECTIVE ledger path (`--trade-log`,
//! `--order-log`, `--position-log`) by writing `.core-lock` — `{pid,
//! started_at, socket, mode, version}` — into it, and REFUSES TO START (exit 1,
//! not one byte written anywhere) if a record is already there and its pid is
//! alive. A record whose pid is gone is taken over, and the boot banner says so.
//! The banner always prints the absolute path of every file this process may
//! write, so "where did that run write?" is answered at boot rather than
//! reconstructed from a cwd afterwards. This is the DATA conflict; #196's socket
//! probe is the separate SOCKET conflict (who is listening). `--allow-shared-data`
//! (or `BLITZKRIEG_ALLOW_SHARED_DATA=1`) is the fixture/backtest escape hatch:
//! the refusal is disabled and the banner states loudly that the directory has
//! two writers. Never use it for a deployment.
//!
//! Strategy selection (repeatable; builtins default to `spread_arb` on and
//! `trend_follow` off, so a new strategy never changes what a running session
//! trades until it is named):
//!                   [--enable-strategy <name>] [--disable-strategy <name>]
//!
//! Market-data capture is ON by default for an engine session (`--no-event-archive`
//! disables it): the events behind a past stop-out only exist if recording was
//! already running when it happened.
//!
//! Backtest (offline; forces dry mode, starts no feeds and writes no logs):
//!   blitzkrieg-core --backtest <archive.jsonl> [--backtest-report <path>]
//!                   [--backtest-tick-ms 50] [--backtest-tail-ms 0] [model flags]
//!                   [--backtest-knob <strategy>:<knob>=<value>]...
//!                   [--fee-model <legacy_quadratic|official>] [strategy knobs:
//!                    --trend-confirm-sec --spread-arb-entry-factor
//!                    --spread-arb-min-obi --spread-arb-max-spread-pct
//!                    --spread-arb-dip-max-pct --spread-arb-bounce-min-pct
//!                    --spread-arb-bounce-window-sec]
//!   The CoreConfig knobs flow through CoreConfig → engine_config, the same
//!   mapping the live server uses, so a sweep is a pure CLI variation with no
//!   rebuild (E15). `--backtest-knob` is the other half: a knob a strategy
//!   declares but CoreConfig does not carry (the cdylibs' own parameters) is
//!   handed to the replayed strategy through the Shadow Evolution hot-param
//!   registry — the same cell, the same `on_hot_params` push as a live
//!   evolution — so a counterfactual arm differs from the shipped one by exactly
//!   that value (see `scripts/mean-reversion-gate-evidence.mjs`).
//!   `--fee-model` is the fee-side counterpart (#203): the same corpus replayed
//!   under a different taker-fee schedule, so "what would the published crypto
//!   rate cost this strategy" is a measurement rather than an argument (see
//!   `scripts/fee-model-sensitivity-check.mjs`).
//!
//! Network self-check (a diagnostic, not a mode — no socket, no ledger, no
//! order, no credential, and it is answered before the #199 latch, so it is safe
//! to run against a live deployment):
//!   blitzkrieg-core --net-check
//!   Answers the question behind a bot that has gone quiet — "is it us or the
//!   venue?" — by probing the SAME hosts the live paths use (venue REST,
//!   discovery, spot stream, venue user stream) through resolver → TCP → TLS →
//!   one cheap request each, and printing ONE JSON `NetCheckReport` on stdout.
//!   The wording is deliberately absent from the core: the launcher, the TUI and
//!   the WebUI render that struct in Chinese, so the core keeps a
//!   language-neutral data contract. Exit 0 when every probe passed, 1 when any
//!   failed — and the report is printed either way, so a failure is readable
//!   rather than inferred from the code. A `CLOB_API_URL` / `POLYMARKET_WS_URL`
//!   override pointing at a loopback or private address is reported as
//!   `rejected` instead of being dialled, and proxy-related environment
//!   variables are NAMED, never printed with their values (a proxy URL may carry
//!   credentials).
//!
//! Env (live): POLYMARKET_PRIVATE_KEY, POLYMARKET_FUNDER_ADDRESS, CLOB_API_URL.

use blitzkrieg_core::ipc::server;
use blitzkrieg_core::model::Mode;
use blitzkrieg_core::position::PositionConfig;
use blitzkrieg_core::risk::RiskConfig;
use blitzkrieg_core::service::CoreConfig;
use rust_decimal::Decimal;
use std::str::FromStr;

struct Args {
    socket: String,
    mode: Mode,
    /// `--readonly`: refuse order egress entirely. Recorded separately from
    /// `mode` because the flag is parsed before the mode precedence is settled —
    /// and it must win over `--mode live` / `DRY_RUN=false`, not be overwritten
    /// by whichever of those was read last.
    readonly: bool,
    tick_ms: u64,
    seed_balance: Decimal,
    max_order_notional: Decimal,
    max_open_notional: Decimal,
    min_shares: Option<Decimal>,
    max_shares: Option<Decimal>,
    /// #202 — the sizing knobs that are relative to the account: `size_pct` is
    /// the per-entry budget as a percentage of cash equity (0 = off, the
    /// absolute `size_usd` path), `max_order_notional_pct` the per-order
    /// notional bound in the same unit (0 = off). Both default to off, so an
    /// unconfigured deployment keeps its exact order sizes.
    size_pct: Decimal,
    max_order_notional_pct: Decimal,
    /// The startup echo for the sizing/notional knobs, printed unconditionally
    /// (same rationale as `daily_loss_echo`: "what bounds one order here?"
    /// must be answerable from the boot log of a run that set no flags).
    sizing_echo: Vec<String>,
    markets: Vec<String>,
    auto_exits: bool,
    max_positions: usize,
    /// #173 — the daily-loss breaker's budget. `daily_loss_usd` is the absolute
    /// cap (0 = none) and `daily_loss_pct` the cap as a percentage of the day's
    /// opening cash equity (0 = off). The EFFECTIVE cap is the tighter of the
    /// two, and it is echoed at startup whether or not anything configured it:
    /// "the breaker was armed, against what?" must be answerable from the boot
    /// log of a run that set no flags (that silence was half of P0-2).
    daily_loss_usd: Decimal,
    daily_loss_pct: Decimal,
    /// The startup echo lines for the daily-loss budget, printed unconditionally
    /// (kept out of `config_report`, which lists only settings moved away from
    /// their compiled default).
    daily_loss_echo: Vec<String>,
    engine: bool,
    min_round_age: i64,
    min_time_left: i64,
    trend_confirm: i64,
    trend_floor_ms: i64,
    spread_arb_entry_factor: Option<Decimal>,
    spread_arb_min_obi: Option<Decimal>,
    spread_arb_max_spread_pct: Option<Decimal>,
    spread_arb_dip_max_pct: Option<Decimal>,
    spread_arb_bounce_min_pct: Option<Decimal>,
    spread_arb_bounce_window_sec: Option<i64>,
    feed_ws: bool,
    /// `--net-check`: probe the venue's network paths, print one JSON report and
    /// exit (0 all passed / 1 any failed). Answered before any service starts,
    /// so it needs no socket, writes no ledger and places no order.
    net_check: bool,
    replay: Option<String>,
    replay_near_miss: Option<String>,
    round_sec: i64,
    near_miss_path: Option<String>,
    trade_log: Option<String>,
    no_trade_log: bool,
    order_log: Option<String>,
    no_order_log: bool,
    position_log: Option<String>,
    no_position_log: bool,
    /// #199: acknowledge that this process may write a data directory another
    /// core is already writing (fixtures, backtests). Only disables the
    /// live-owner refusal — the boot banner still says so, loudly.
    allow_shared_data: bool,
    market_plugin: Option<String>,
    discovery: bool,
    shadow_evolution: bool,
    /// Resolved asset list (CLI `--assets` > `BK_ASSETS` > `[engine].assets` >
    /// the built-in list).
    assets: Vec<String>,
    se_min_samples: Option<u32>,
    se_cooldown_secs: Option<i64>,
    se_min_obs_secs: Option<i64>,
    /// Shadow-evolution metrics window (secs), resolved through the same chain.
    se_eval_window_secs: Option<i64>,
    /// Per-evolution step ceiling (Lock 1), resolved through the same chain and
    /// clamped to the built-in ceiling — a file may tighten it, never widen it.
    se_max_gradient: Option<Decimal>,
    se_variant_count: Option<usize>,
    se_audit_dir: Option<String>,
    se_min_win_rate: Option<Decimal>,
    se_min_profit_factor: Option<Decimal>,
    /// E13 proposal workflow (all resolved through the same chain).
    se_auto_evolve: Option<bool>,
    se_cycle_secs: Option<i64>,
    se_ttl_secs: Option<i64>,
    se_deep_dims: Option<usize>,
    /// Per-strategy entry caps: `name:max_open_positions:max_notional_usd`
    /// (repeatable; `-` or empty = no cap on that segment).
    strategy_limits: Vec<String>,
    /// Strategies to switch ON at startup (repeatable, E4-a). The kernel ships
    /// ZERO enabled strategies — what starts enabled is the operator's persisted
    /// intent (`--strategy-state` file, replayed and rewritten) plus these flags;
    /// an unknown name is warned about, never fatal.
    enable_strategy: Vec<String>,
    /// Strategies to switch OFF at startup (repeatable). Applied after
    /// `--enable-strategy`, so an explicit "off" wins.
    disable_strategy: Vec<String>,
    /// Where the effective enabled-set is recorded so toggles and boot flags
    /// survive a restart. Default `data/strategy-state.json`; `none`/empty = no
    /// persistence (a backtest or a hermetic harness wants this).
    strategy_state: Option<String>,
    /// Directory scanned at startup for user-layer strategy libraries
    /// (`*.dylib`/`*.so`): every library found is loaded and enabled, so
    /// dropping a file in the folder is the whole install. `none`/empty = off.
    /// Default `user_layer/strategies` under the repo; overridden by
    /// `BK_STRATEGY_DIR` (a file-config key would be dead weight: this belongs
    /// to "where is the checkout", not to strategy parameters).
    strategy_dir: Option<String>,
    /// Mirror every market-data event into this JSONL archive (P-1.3).
    /// None = use the always-on default for an engine session (see `--no-event-archive`).
    event_archive: Option<String>,
    /// Disable market-data capture entirely. Required for a harness that spawns
    /// the engine: it would otherwise record into the default archive path.
    no_event_archive: bool,
    /// Stop recording at this archive size (MiB); 0 = unlimited (the default).
    /// None = not given; the always-on default then supplies its own value.
    event_archive_max_mb: Option<u64>,
    /// Rotate into a new UTC-stamped segment every this many MiB (0 = never).
    /// Required for a 24/7 capture: an un-rotated archive hits the cap and goes
    /// dark. Rotation only renames; nothing is ever deleted.
    event_archive_rotate_mb: Option<u64>,
    /// Stop recording before the volume has less than this many MiB free
    /// (0 = no guard). Checked once per rotation.
    event_archive_min_free_mb: Option<u64>,
    /// Maker→taker escalation deadline for engine entries (ms).
    entry_maker_timeout_ms: i64,
    /// #205 — how old an orderbook may be before the engine refuses to price off
    /// it (ms). 0 = the freshness check is OFF (any age accepted). This is the
    /// value in force after CLI/env resolution and validation; the compiled
    /// default is `engine::DEFAULT_MAX_ORDERBOOK_STALE_MS`.
    max_orderbook_stale_ms: i64,
    /// The startup echo for the freshness budget, printed unconditionally: the
    /// answer to "how long after the feed goes quiet does this bot stop
    /// trading?" must not depend on someone having thought to configure it.
    orderbook_stale_echo: Vec<String>,
    /// Offline replay of an archive through the same core (P-1.2).
    backtest: Option<String>,
    /// Where to write the backtest report as JSON (None = stdout only).
    backtest_report: Option<String>,
    /// Virtual-clock step (ms) between maintenance cycles in a replay.
    backtest_tick_ms: i64,
    /// Keep the replay clock running this long after the last event (ms).
    backtest_tail_ms: i64,
    /// Counterfactual `--backtest-knob <strategy>:<knob>=<value>` overrides,
    /// handed to the replayed strategies through the Shadow Evolution hot-param
    /// path (so a replay can A/B a knob value on the SAME frozen corpus).
    backtest_knobs: Vec<(String, String, Decimal)>,
    /// Counterfactual taker-fee schedule for a replay (#203): `--fee-model
    /// <name>`. None = the shipped schedule. Replay-only, exactly like
    /// `--backtest-knob`: what a live run charges is not a per-invocation choice,
    /// so asking for this without `--backtest` is refused rather than ignored.
    fee_model: Option<blitzkrieg_core::exit_policy::FeeSchedule>,
    /// MarketRegime evaluation over an archive (E16 / #98): label windows and
    /// score the online state machine against the offline labels.
    regime_eval: Option<String>,
    /// Where to write the regime report (<path>.json + .md; None = stdout).
    regime_report: Option<String>,
    /// Evaluate exactly this token instead of the most-active defaults.
    regime_token: Option<String>,
    /// How many of the most active tokens to evaluate (default 3).
    regime_max_tokens: usize,
    /// Regime window in seconds — the label rule and the machine share it.
    regime_window_sec: i64,
    /// Trend bar: |net move| ≥ this many ticks with the efficiency ≥ the bar.
    regime_min_trend_ticks: Decimal,
    /// Trend efficiency bar (|net| / path).
    regime_min_efficiency: Decimal,
    /// Volatile bar: mean |per-step move| ≥ this many ticks.
    regime_volatile_mad_ticks: Decimal,
    /// Confirmation hysteresis before the machine switches state.
    regime_confirmations: u32,
    /// Fill model: taker slippage in ticks (0.01).
    slippage_ticks: u32,
    /// Fill model: maker latency (ms).
    latency_ms: i64,
    /// Fill model: maker fill probability (bps of 10000); None = untouched.
    fill_prob_bps: Option<u32>,
    /// Fill model: share of the crossing depth a resting maker takes, in bps of
    /// 10000 (queue position); None = untouched.
    maker_depth_share_bps: Option<u32>,
    /// Dry-mode test hook: the first N simulated redemption attempts of each
    /// claim fail before one lands (0 = every attempt lands).
    dry_redeem_fail: u32,
    /// Dry-mode test hook: report those injected failures as `manual`, i.e. stop
    /// the automatic retries for good.
    dry_redeem_manual: bool,
    /// One line per file-settable setting, `key=value (source)`, for the startup
    /// report. Only settings resolved from above the compiled default appear, so
    /// "where did this number come from" is answerable from the log alone.
    config_report: Vec<String>,
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

/// Where the config file comes from, settled BEFORE argument parsing (the file
/// has to be loaded to resolve the arguments against it).
enum ConfigChoice {
    /// No file: every setting comes from CLI/env/default.
    Off,
    Path(String),
}

/// The environment layer of the precedence chain, wrapped in a struct so a test
/// can supply its own map. Mutating the process environment to test precedence
/// would be global state that parallel tests race on.
#[derive(Debug, Clone, Default)]
struct EnvVars {
    vars: std::collections::HashMap<String, String>,
}

impl EnvVars {
    /// Every `BK_*` variable in the process environment. Only the kernel's own
    /// namespace is copied: `HFT_*` belongs to the UI shell and `DRY_RUN` is
    /// read at its own call site (its meaning predates this layer).
    fn from_process() -> Self {
        let mut vars = std::collections::HashMap::new();
        for (k, v) in std::env::vars() {
            if k.starts_with("BK_") {
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
fn prescan_config(argv: &[String]) -> Option<ConfigChoice> {
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
fn config_choice(argv: &[String], env: &EnvVars) -> ConfigChoice {
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

/// Why `--max-orderbook-stale-ms` / `BK_MAX_ORDERBOOK_STALE_MS` cannot be used as
/// given, or `None` when it can (#205).
///
/// Pure so the rule is unit-testable while the caller keeps the loud failure
/// (stderr + exit 2): a budget that silently falls back to the compiled default
/// is the exact failure mode this knob exists to remove. `0` is legal and means
/// "freshness check OFF" — a supported setting, not an invalid one — so it is
/// deliberately not rejected.
fn stale_budget_rejection(ms: i64) -> Option<String> {
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

fn parse_args(file: &blitzkrieg_core::config::FileConfig, argv: &[String], env: &EnvVars) -> Args {
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
    // #173: the daily-loss budget, tracked as Option so CLI > env > default
    // resolution can tell "the operator spoke" from "nobody did".
    let mut max_daily_loss: Option<Decimal> = None;
    let mut max_daily_loss_pct: Option<Decimal> = None;
    let mut strategy_limits: Vec<String> = Vec::new();
    let mut enable_strategy: Vec<String> = Vec::new();
    let mut disable_strategy: Vec<String> = Vec::new();
    let mut strategy_state: Option<String> = None;
    let mut no_strategy_state = false;
    let mut strategy_dir: Option<String> = None;
    let mut no_strategy_dir = false;
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
    let mut allow_shared_data = false;
    let mut market_plugin: Option<String> = None;
    let mut discovery = true;
    let mut shadow_evolution_flag = false;
    let mut se_min_samples: Option<u32> = None;
    let mut se_cooldown_secs: Option<i64> = None;
    let mut se_min_obs_secs: Option<i64> = None;
    let mut se_auto_evolve: Option<bool> = None;
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
    // #203: replay-only fee schedule (see `Args::fee_model`).
    let mut fee_model: Option<blitzkrieg_core::exit_policy::FeeSchedule> = None;
    let mut regime_eval: Option<String> = None;
    let mut regime_report: Option<String> = None;
    let mut regime_token: Option<String> = None;
    let mut regime_max_tokens: usize = 3;
    let mut regime_window_sec: i64 = 300;
    let mut regime_min_trend_ticks = Decimal::new(3, 0);
    let mut regime_min_efficiency = Decimal::new(5, 1);
    let mut regime_volatile_mad_ticks = Decimal::new(15, 1);
    let mut regime_confirmations: u32 = 2;
    let mut slippage_ticks: u32 = 0;
    let mut latency_ms: i64 = 0;
    let mut fill_prob_bps: Option<u32> = None;
    let mut maker_depth_share_bps: Option<u32> = None;
    // #205: the orderbook freshness budget, tracked as Option so CLI > env >
    // default resolution can tell "the operator spoke" from "nobody did".
    let mut max_orderbook_stale_ms: Option<i64> = None;
    // Dry-mode redemption test hooks (#175 gate): fail the first N simulated
    // redemption attempts of each claim, optionally as a `manual` failure that
    // stops the retries for good. 0 = production behaviour.
    let mut dry_redeem_fail: u32 = 0;
    let mut dry_redeem_manual = false;

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
            "--socket" => socket = it.next().unwrap_or(socket),
            "--mode" => {
                mode = match it.next().as_deref() {
                    Some("live") => Mode::Live,
                    _ => Mode::Dry,
                }
            }
            "--readonly" => readonly = true,
            "--tick-ms" => tick_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(tick_ms),
            "--seed-balance" => {
                seed_balance = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .unwrap_or(seed_balance)
            }
            "--max-order-notional" => {
                max_order_notional = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .unwrap_or(max_order_notional)
            }
            "--max-open-notional-usd" => {
                max_open_notional = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .unwrap_or(max_open_notional)
            }
            // #202: per-entry budget as a share of the account, and the
            // per-order notional bound in the same unit. Both are percentages
            // (20 = 20%), both default to 0 = off.
            "--size-pct" => {
                size_pct = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .or(size_pct)
            }
            "--max-order-notional-pct" => {
                max_order_notional_pct = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .or(max_order_notional_pct)
            }
            "--min-shares" => {
                min_shares = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .or(min_shares)
            }
            "--max-shares" => {
                max_shares = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .or(max_shares)
            }
            "--no-auto-exits" => auto_exits = false,
            "--engine" => engine = true,
            "--feed-ws" => feed_ws = true,
            "--replay" => replay = it.next(),
            "--replay-near-miss" => replay_near_miss = it.next(),
            "--net-check" => net_check = true,
            "--near-miss-path" => near_miss_path = it.next(),
            "--trade-log" => trade_log = it.next(),
            "--no-trade-log" => no_trade_log = true,
            "--order-log" => order_log = it.next(),
            "--no-order-log" => no_order_log = true,
            "--position-log" => position_log = it.next(),
            "--no-position-log" => no_position_log = true,
            "--allow-shared-data" => allow_shared_data = true,
            "--market-plugin" => market_plugin = it.next(),
            "--no-discovery" => discovery = false,
            "--shadow-evolution" => shadow_evolution_flag = true,
            "--se-min-samples" => se_min_samples = it.next().and_then(|v| v.parse().ok()),
            "--se-cooldown-secs" => se_cooldown_secs = it.next().and_then(|v| v.parse().ok()),
            "--se-min-obs-secs" => se_min_obs_secs = it.next().and_then(|v| v.parse().ok()),
            "--se-auto-evolve" => {
                se_auto_evolve = it.next().and_then(|v| match v.as_str() {
                    "true" | "1" | "on" => Some(true),
                    "false" | "0" | "off" => Some(false),
                    _ => None,
                })
            }
            "--se-cycle-secs" => se_cycle_secs = it.next().and_then(|v| v.parse().ok()),
            "--se-ttl-secs" => se_ttl_secs = it.next().and_then(|v| v.parse().ok()),
            "--se-deep-dims" => se_deep_dims = it.next().and_then(|v| v.parse().ok()),
            "--strategy-limit" => {
                if let Some(v) = it.next() {
                    strategy_limits.push(v);
                }
            }
            "--enable-strategy" => {
                if let Some(v) = it.next().filter(|v| !v.trim().is_empty()) {
                    enable_strategy.push(v);
                }
            }
            "--disable-strategy" => {
                if let Some(v) = it.next().filter(|v| !v.trim().is_empty()) {
                    disable_strategy.push(v);
                }
            }
            "--strategy-state" => {
                strategy_state = it.next().filter(|v| !v.trim().is_empty());
            }
            "--no-strategy-state" => no_strategy_state = true,
            "--strategy-dir" => strategy_dir = it.next().filter(|v| !v.trim().is_empty()),
            "--no-strategy-dir" => no_strategy_dir = true,
            "--assets" => assets_arg = it.next(),
            "--round-sec" => round_sec = it.next().and_then(|v| v.parse().ok()).or(round_sec),
            "--min-round-age" => {
                min_round_age = it.next().and_then(|v| v.parse().ok()).or(min_round_age)
            }
            "--min-time-left" => {
                min_time_left = it.next().and_then(|v| v.parse().ok()).or(min_time_left)
            }
            "--trend-confirm-sec" => {
                trend_confirm = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(trend_confirm)
            }
            "--trend-window-floor-ms" => {
                trend_floor_ms = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(trend_floor_ms)
            }
            "--spread-arb-entry-factor" => {
                spread_arb_entry_factor = it.next().and_then(|v| Decimal::from_str(&v).ok())
            }
            "--spread-arb-min-obi" => {
                spread_arb_min_obi = it.next().and_then(|v| Decimal::from_str(&v).ok())
            }
            "--spread-arb-max-spread-pct" => {
                spread_arb_max_spread_pct = it.next().and_then(|v| Decimal::from_str(&v).ok())
            }
            "--spread-arb-dip-max-pct" => {
                spread_arb_dip_max_pct = it.next().and_then(|v| Decimal::from_str(&v).ok())
            }
            "--spread-arb-bounce-min-pct" => {
                spread_arb_bounce_min_pct = it.next().and_then(|v| Decimal::from_str(&v).ok())
            }
            "--spread-arb-bounce-window-sec" => {
                spread_arb_bounce_window_sec = it.next().and_then(|v| v.parse().ok())
            }
            "--regime-eval" => regime_eval = it.next(),
            "--regime-report" => regime_report = it.next(),
            "--regime-token" => regime_token = it.next(),
            "--regime-max-tokens" => {
                regime_max_tokens = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(regime_max_tokens)
            }
            "--regime-window-sec" => {
                regime_window_sec = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(regime_window_sec)
            }
            "--regime-min-trend-ticks" => {
                regime_min_trend_ticks = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .unwrap_or(regime_min_trend_ticks)
            }
            "--regime-min-efficiency" => {
                regime_min_efficiency = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .unwrap_or(regime_min_efficiency)
            }
            "--regime-volatile-mad-ticks" => {
                regime_volatile_mad_ticks = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .unwrap_or(regime_volatile_mad_ticks)
            }
            "--regime-confirmations" => {
                regime_confirmations = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(regime_confirmations)
            }
            "--max-positions" => {
                max_positions = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(max_positions)
            }
            // #173: absolute USD cap for the day's realized loss (0 = none).
            "--max-daily-loss" => {
                max_daily_loss = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .or(max_daily_loss)
            }
            // #173: cap as a percentage of the day's opening cash equity.
            "--max-daily-loss-pct" => {
                max_daily_loss_pct = it
                    .next()
                    .and_then(|v| Decimal::from_str(&v).ok())
                    .or(max_daily_loss_pct)
            }
            "--market" => {
                if let Some(m) = it.next() {
                    markets.push(m);
                }
            }
            "--event-archive" => event_archive = it.next(),
            "--no-event-archive" => no_event_archive = true,
            "--event-archive-max-mb" => {
                event_archive_max_mb = it.next().and_then(|v| v.parse().ok())
            }
            "--event-archive-rotate-mb" => {
                event_archive_rotate_mb = it.next().and_then(|v| v.parse().ok())
            }
            "--event-archive-min-free-mb" => {
                event_archive_min_free_mb = it.next().and_then(|v| v.parse().ok())
            }
            "--entry-maker-timeout-ms" => {
                entry_maker_timeout_ms = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(entry_maker_timeout_ms)
            }
            // #205: how stale an orderbook may be before the engine refuses to
            // price off it. Parsed strictly — a value the engine cannot honour is
            // a startup error, never a silent fallback: an operator who believes
            // they set a budget must not be running a different one.
            "--max-orderbook-stale-ms" => {
                let raw = it.next().unwrap_or_default();
                match raw.trim().parse::<i64>() {
                    Ok(v) => max_orderbook_stale_ms = Some(v),
                    Err(_) => {
                        eprintln!(
                            "blitzkrieg-core: --max-orderbook-stale-ms '{raw}': want a whole number of milliseconds (0 disables the freshness check)"
                        );
                        std::process::exit(2);
                    }
                }
            }
            "--backtest" => backtest = it.next(),
            "--backtest-report" => backtest_report = it.next(),
            "--backtest-tick-ms" => {
                backtest_tick_ms = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(backtest_tick_ms)
            }
            "--backtest-tail-ms" => {
                backtest_tail_ms = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(backtest_tail_ms)
            }
            // `<strategy>:<knob>=<value>`; repeatable. A replay-only counterfactual.
            "--backtest-knob" => {
                if let Some(spec) = it.next() {
                    match parse_backtest_knob(&spec) {
                        Ok(k) => backtest_knobs.push(k),
                        Err(e) => {
                            eprintln!("blitzkrieg-core: --backtest-knob {spec}: {e}");
                            std::process::exit(2);
                        }
                    }
                }
            }
            // #203: which taker-fee schedule a REPLAY charges. Not a live knob —
            // see `Args::fee_model`.
            "--fee-model" => {
                if let Some(name) = it.next() {
                    match blitzkrieg_core::exit_policy::fee_schedule_by_name(&name) {
                        Some(s) => fee_model = Some(s),
                        None => {
                            eprintln!(
                                "blitzkrieg-core: --fee-model {name}: unknown schedule; known: {}",
                                blitzkrieg_core::exit_policy::FEE_SCHEDULE_NAMES.join(", ")
                            );
                            std::process::exit(2);
                        }
                    }
                }
            }
            "--slippage-ticks" => {
                slippage_ticks = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(slippage_ticks)
            }
            "--latency-ms" => {
                latency_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(latency_ms)
            }
            "--fill-prob-bps" => fill_prob_bps = it.next().and_then(|v| v.parse().ok()),
            "--maker-depth-share-bps" => {
                maker_depth_share_bps = it.next().and_then(|v| v.parse().ok())
            }
            // Dry-mode test hooks (#175 gate). Both are inert unless a mode that
            // simulates the redemption is running, so a live deployment can pass
            // them and see no change at all.
            "--dry-redeem-fail" => {
                dry_redeem_fail = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(dry_redeem_fail)
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
    // Lock 1 is a safety lock, not a tuning surface: a file may tighten the
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
                 (Lock 1 is not widenable)",
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
        env.text("BK_SE_AUTO_EVOLVE").and_then(|v| match v.trim() {
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" => Some(false),
            _ => None,
        }),
        file.shadow.auto_evolve
    );
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
        None,
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
        markets,
        auto_exits,
        max_positions,
        daily_loss_usd: daily_loss_usd.value,
        daily_loss_pct: daily_loss_pct.value,
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
        allow_shared_data,
        market_plugin,
        discovery,
        shadow_evolution: shadow_evolution_flag
            || pick(
                None::<bool>,
                env.num::<bool>("BK_SHADOW_EVOLUTION"),
                file.shadow.enabled,
                false,
            )
            .value,
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
        strategy_limits,
        enable_strategy,
        disable_strategy,
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
        fee_model,
        regime_eval,
        regime_report,
        regime_token,
        regime_max_tokens,
        regime_window_sec,
        regime_min_trend_ticks,
        regime_min_efficiency,
        regime_volatile_mad_ticks,
        regime_confirmations,
        slippage_ticks,
        latency_ms,
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
const DEFAULT_ROUND_SEC: i64 = 900;
const DEFAULT_MIN_ROUND_AGE_SEC: i64 = 30;
const DEFAULT_MIN_TIME_LEFT_SEC: i64 = 180;
const DEFAULT_ASSETS: [&str; 4] = ["BTC", "ETH", "SOL", "XRP"];

/// Default archive path, relative to the core's working directory (the repo root
/// for the production shell).
const DEFAULT_EVENT_ARCHIVE: &str = "data/archive/events.jsonl";

/// Resolve the market-data capture settings.
///
/// Capture is ON by default for an engine session (see the rationale at the call
/// site). `--no-event-archive` disables it; an explicit `--event-archive <path>`
/// wins over the default path. Unset tuning gets the 24/7-safe values: no session
/// cap (`0`), 256 MiB segments, and a 5 GiB free-space floor — a cap with no
/// rotation would silently go dark, which is the failure this default exists to
/// avoid.
fn resolve_event_archive(
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
fn parse_strategy_limits(
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

/// #173: where the day's realized-loss budget is persisted. A sibling of the
/// position log, derived the same way the `.recon` watermark is
/// (`path.with_extension(..)`), so `data/positions/positions.jsonl` gets
/// `data/positions/positions.daily-loss.json` beside it: the two files describe
/// the same book and belong in the same place (both under the git-ignored
/// `data/`).
fn daily_loss_path_for(position_log: &str) -> String {
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
async fn run_net_check(plugin: Option<&str>) -> i32 {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // #184: INFO by default, not ERROR — `EnvFilter::from_default_env()` with no
    // RUST_LOG kept only ERROR, so a deployed run's warn/error breadcrumbs never
    // reached the run log while the process looked healthy. An explicit RUST_LOG
    // still wins outright. `logging` is the one spelling of that rule; the
    // effective level is echoed below so the run log states it.
    tracing_subscriber::fmt()
        .with_env_filter(blitzkrieg_core::logging::log_filter())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    eprintln!(
        "blitzkrieg-core: log level {} (source: {})",
        blitzkrieg_core::logging::effective_level(),
        blitzkrieg_core::logging::level_source()
    );

    // Configuration is loaded BEFORE the arguments, because the arguments are
    // resolved against it (CLI > env > TOML > default).
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // `--version` is answered before anything else is touched: it must work with
    // no config file, no socket, no data/ and no git, and must not boot a core.
    // The string carries the built revision (`<semver>+g<sha>`, #179), which is
    // what lets a gate compare the binary under test against the commit it is
    // checking instead of trusting whatever is on disk (#172).
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", blitzkrieg_core::ipc::build_info::version_string());
        return Ok(());
    }

    // #228: `--help` is answered here, beside `--version` and for the same
    // reason — it must work with no config file, no socket, no data/ and no git,
    // and it must NEVER boot a core. It reads the accepted set out of
    // `blitzkrieg_core::cli::FLAGS`, the same table the parser's fallback arm
    // consults, so the list an operator checks a spelling against is the list the
    // parser enforces (a test in that module pins the two together). `--version`
    // is checked first, so a command with both keeps the pre-#228 answer.
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", blitzkrieg_core::cli::help_text());
        return Ok(());
    }

    let env = EnvVars::from_process();
    let choice = config_choice(&argv, &env);
    let file = match &choice {
        ConfigChoice::Off => blitzkrieg_core::config::FileConfig::default(),
        ConfigChoice::Path(p) => {
            blitzkrieg_core::config::FileConfig::load(Some(std::path::Path::new(p)))
        }
    };
    for w in &file.warnings {
        eprintln!("blitzkrieg-core: config warning: {w}");
    }
    // An unknown key is the exact failure mode this layer exists to prevent
    // (KI-11), so it is loud — but never fatal.
    for k in &file.unknown_keys {
        eprintln!("blitzkrieg-core: config key not understood (ignored): {k}");
    }

    let args = parse_args(&file, &argv, &env);

    // Say where the non-default settings came from. A declarative config layer is
    // only auditable if the effective value and its origin are both observable.
    if let Some(p) = &file.path {
        eprintln!("blitzkrieg-core: config file {}", p.display());
    }
    for line in &args.config_report {
        eprintln!("blitzkrieg-core: config {line}");
    }
    // #173: the daily-loss breaker's effective budget, echoed whether or not any
    // layer configured it — a run that set nothing must still say what protects
    // it (the old default was silently uncapped for a small book).
    for line in &args.daily_loss_echo {
        eprintln!("blitzkrieg-core: {line}");
    }
    // #202: what one order may commit on THIS account. Printed unconditionally
    // for the same reason as the breaker's budget above: the answer must not
    // depend on someone having thought to configure (or grep) it.
    for line in &args.sizing_echo {
        eprintln!("blitzkrieg-core: {line}");
    }
    // #205: the freshness budget in force, printed unconditionally for the same
    // reason: "how long after the feed goes quiet does this bot stop trading?"
    // must be answerable from the boot log of a run that set no flags.
    for line in &args.orderbook_stale_echo {
        eprintln!("blitzkrieg-core: {line}");
    }

    // Which code this process is (#179). On stderr and deliberately NOT through
    // `tracing`: the filter is configurable (`RUST_LOG=error` is a legitimate
    // operator choice, #184), so a provenance line sent to the log layer could be
    // absent from exactly the runs later asked "which build was that?".
    eprintln!(
        "blitzkrieg-core: {}",
        blitzkrieg_core::ipc::build_info::provenance_line()
    );

    // A diagnostic, not a mode: answered before the mode is decided, before any
    // data directory is claimed (it writes nothing), and before the replay /
    // backtest runners — so it needs no socket, no ledger and no `data/`, and is
    // safe to run against a live deployment. It reads the parsed args only, which
    // is why it sits here and not beside the other one-shot runners: those are
    // reached after `CoreConfig` is built, and `CoreConfig` consumes the very
    // field (`market_plugin`) this needs.
    if args.net_check {
        std::process::exit(run_net_check(args.market_plugin.as_deref()).await);
    }

    // DRY_RUN env honours the existing convention when --mode is not explicit.
    let mode = if std::env::args().any(|a| a == "--mode") {
        args.mode
    } else if std::env::var("DRY_RUN")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        Mode::Live
    } else {
        Mode::Dry
    };

    // `--readonly` is decided LAST and unconditionally, so nothing above can
    // outrank it. Deciding it in the same chain would make the promise depend on
    // clause order — exactly the kind of thing that reads correct and is not.
    let mode = if args.readonly {
        if mode == Mode::Live {
            eprintln!(
                "blitzkrieg-core: --readonly given with a live mode request; \
                 refusing egress (orders will settle locally, no venue is started)"
            );
        }
        Mode::ReadOnly
    } else {
        mode
    };

    // When the self-driving engine is on, record near-misses to disk by default
    // so the entry-gate question can be evaluated offline.
    let near_miss_path = if args.engine {
        args.near_miss_path
            .clone()
            .or_else(|| Some("data/shadow/near-miss.jsonl".to_string()))
    } else {
        args.near_miss_path.clone()
    };

    // Trade log destination. Defaults to the relative `data/trades/trades.jsonl`;
    // `--trade-log <path>` overrides it (used by tests to keep synthetic trades out
    // of the production ledger) and `--no-trade-log` disables persistence entirely.
    let trade_log_path = if args.no_trade_log {
        None
    } else {
        Some(
            args.trade_log
                .clone()
                .unwrap_or_else(|| "data/trades/trades.jsonl".to_string()),
        )
    };

    // Order log: durable tracked-order storage for crash recovery + orphan sweep.
    let order_log_path = if args.no_order_log {
        None
    } else {
        Some(
            args.order_log
                .clone()
                .unwrap_or_else(|| "data/orders/orders.jsonl".to_string()),
        )
    };

    // Position log: durable snapshot of the OPEN position book (crash recovery).
    let position_log_path = if args.no_position_log {
        None
    } else {
        Some(
            args.position_log
                .clone()
                .unwrap_or_else(|| "data/positions/positions.jsonl".to_string()),
        )
    };

    // Per-order share sizing. Both default to 10 (fixed lot) when neither flag is
    // given. Clamp min<=max so a stray flag order cannot invert the range.
    let (min_shares, max_shares) = {
        let mn = args.min_shares.unwrap_or_else(|| Decimal::from(10));
        let mx = args.max_shares.unwrap_or_else(|| Decimal::from(10));
        if mn > mx {
            eprintln!(
                "blitzkrieg-core: --min-shares {mn} > --max-shares {mx}; clamping min to max"
            );
            (mx, mx)
        } else {
            (mn, mx)
        }
    };

    // Market-data capture is ON by default for an engine session. The data for a
    // past stop-out only exists if recording was already running when it happened,
    // which makes "opt in" the wrong default for the one thing that cannot be
    // reconstructed after the fact. Rotation + a free-space floor are what make
    // leaving it on safe, so the default supplies those too.
    let (
        event_archive_path,
        event_archive_max_mb,
        event_archive_rotate_mb,
        event_archive_min_free_mb,
    ) = resolve_event_archive(
        args.engine,
        args.no_event_archive,
        args.event_archive.as_deref(),
        args.event_archive_max_mb,
        args.event_archive_rotate_mb,
        args.event_archive_min_free_mb,
    );

    // What starts enabled is the OPERATOR'S intent, never a hardcoded name:
    // the persisted set (runtime toggles + earlier boot flags) first, then the
    // explicit CLI adds on top. `--disable-strategy` still wins over both — it
    // is applied after them in `install_engine`. A fresh checkout (no state
    // file) boots with ZERO strategies enabled; the panel's toggles then build
    // the set and it persists from the first change on.
    let mut enabled_strategies = args
        .strategy_state
        .as_deref()
        .map(|p| blitzkrieg_core::strategy_state::load(std::path::Path::new(p)))
        .unwrap_or_default();
    for s in args.enable_strategy {
        if !enabled_strategies.contains(&s) {
            enabled_strategies.push(s);
        }
    }

    // #173: the daily-loss budget's durable side file follows the position log
    // (the struct literal below consumes `position_log_path`).
    let daily_pnl_path = position_log_path.as_deref().map(daily_loss_path_for);

    let config = CoreConfig {
        mode,
        default_maker_timeout_ms: 5000,
        risk: RiskConfig {
            max_order_notional: args.max_order_notional,
            // #202: the same bound as a share of the account (0 = off).
            max_order_notional_pct: args.max_order_notional_pct,
            max_open_notional_usd: args.max_open_notional,
            ..Default::default()
        },
        dry_seed_balance: args.seed_balance,
        // #205: the validated freshness budget (0 = check off).
        max_orderbook_stale_ms: args.max_orderbook_stale_ms,
        strategy_limits: parse_strategy_limits(&args.strategy_limits),
        enabled_strategies,
        disabled_strategies: args.disable_strategy,
        strategy_state_path: args.strategy_state,
        strategy_dir: args.strategy_dir,
        markets: args.markets,
        auto_exits_enabled: args.auto_exits,
        engine_enabled: args.engine,
        min_shares,
        max_shares,
        // #202: equity-relative per-entry budget (0 = the absolute size_usd path,
        // i.e. every deployment's current order sizes, unchanged).
        size_pct: args.size_pct,
        near_miss_path,
        trade_log_path,
        order_log_path,
        position_log_path,
        discovery_enabled: args.discovery,
        shadow_evolution_enabled: args.shadow_evolution,
        shadow_evolution_tuning: if args.se_min_samples.is_some()
            || args.se_cooldown_secs.is_some()
            || args.se_min_obs_secs.is_some()
            || args.se_eval_window_secs.is_some()
            || args.se_max_gradient.is_some()
            || args.se_variant_count.is_some()
            || args.se_audit_dir.is_some()
            || args.se_min_win_rate.is_some()
            || args.se_min_profit_factor.is_some()
            || args.se_auto_evolve.is_some()
            || args.se_cycle_secs.is_some()
            || args.se_ttl_secs.is_some()
            || args.se_deep_dims.is_some()
        {
            Some(blitzkrieg_core::service::ShadowEvolutionTuning {
                min_sample_count: args.se_min_samples,
                cooldown_secs: args.se_cooldown_secs,
                min_observation_secs: args.se_min_obs_secs,
                evaluation_window_secs: args.se_eval_window_secs,
                max_gradient: args.se_max_gradient,
                variant_count: args.se_variant_count,
                min_win_rate_improvement: args.se_min_win_rate,
                min_profit_factor_improvement: args.se_min_profit_factor,
                audit_dir: args.se_audit_dir.clone(),
                auto_evolve: args.se_auto_evolve,
                evolution_cycle_secs: args.se_cycle_secs,
                proposal_ttl_secs: args.se_ttl_secs,
                deep_dims: args.se_deep_dims,
            })
        } else {
            None
        },
        assets: args.assets.clone(),
        min_round_age_sec: args.min_round_age,
        trend_confirm_sec: args.trend_confirm,
        trend_window_floor_ms: args.trend_floor_ms,
        spread_arb_trend_entry_factor: args.spread_arb_entry_factor,
        spread_arb_entry_min_obi: args.spread_arb_min_obi,
        spread_arb_entry_max_spread_pct: args.spread_arb_max_spread_pct,
        spread_arb_entry_dip_max_pct: args.spread_arb_dip_max_pct,
        spread_arb_entry_bounce_min_pct: args.spread_arb_bounce_min_pct,
        spread_arb_entry_bounce_window_sec: args.spread_arb_bounce_window_sec,
        feed_ws_enabled: args.feed_ws,
        market_plugin: args.market_plugin,
        round_duration_sec: args.round_sec,
        positions: PositionConfig {
            max_positions: args.max_positions,
            // #173: the budget comes from the CLI/env, and its durable side file
            // is derived from the position log (the same convention as the
            // `.recon` watermark beside it) so a restart resumes the SAME day's
            // realized loss instead of laundering it.
            max_daily_loss_usd: args.daily_loss_usd,
            max_daily_loss_equity_pct: args.daily_loss_pct,
            daily_pnl_path: daily_pnl_path.clone(),
            exit: blitzkrieg_core::exit_policy::ExitConfig {
                min_time_left_sec: args.min_time_left,
                ..Default::default()
            },
            ..Default::default()
        },
        entry_maker_timeout_ms: args.entry_maker_timeout_ms,
        fill_model: blitzkrieg_core::sim::FillModel {
            taker_slippage_ticks: args.slippage_ticks,
            maker_latency_ms: args.latency_ms,
            maker_fill_prob_bps: args.fill_prob_bps.unwrap_or(10_000),
            maker_depth_share_bps: args.maker_depth_share_bps.unwrap_or(10_000),
        },
        event_archive_path,
        event_archive_max_mb,
        event_archive_rotate_mb,
        event_archive_min_free_mb,
        // #175 gate hooks: dry-mode redemption failure injection, off unless the
        // flag is given (see `CoreConfig::dry_redeem_fail`).
        dry_redeem_fail: args.dry_redeem_fail,
        dry_redeem_manual: args.dry_redeem_manual,
        ..Default::default()
    };

    if let Some(path) = &args.replay {
        run_replay(std::path::Path::new(path));
        return Ok(());
    }
    if let Some(path) = &args.replay_near_miss {
        run_replay_near_miss(std::path::Path::new(path));
        return Ok(());
    }
    if let Some(path) = &args.regime_eval {
        // Offline MarketRegime evaluation: independent of the engine — it
        // needs no mode, no feeds and never trades.
        run_regime_eval(
            path,
            args.regime_report.as_deref(),
            blitzkrieg_core::regime_eval::RegimeEvalArgs {
                token: args.regime_token.clone(),
                max_tokens: args.regime_max_tokens,
                config: strategy_logic::MarketRegimeConfig {
                    window_ms: args.regime_window_sec * 1_000,
                    trend_net_ticks: args.regime_min_trend_ticks,
                    trend_min_efficiency: args.regime_min_efficiency,
                    volatile_mad_ticks: args.regime_volatile_mad_ticks,
                    confirmations: args.regime_confirmations,
                    ..strategy_logic::MarketRegimeConfig::default()
                },
            },
        );
        return Ok(());
    }

    if let Some(path) = &args.backtest {
        // #203: install the counterfactual fee schedule BEFORE the core is built,
        // so every fill the replay charges is priced under it. Set-once: a failed
        // install means a schedule was already chosen, which cannot happen here
        // (this branch runs once) and must not be silent if it somehow does.
        if let Some(schedule) = args.fee_model
            && let Err(e) = blitzkrieg_core::exit_policy::set_fee_schedule(schedule)
        {
            eprintln!("blitzkrieg-core: --fee-model {}: {e}", schedule.name);
            std::process::exit(2);
        }
        // Offline replay: never re-record into (or read from) the file being
        // replayed — an appended-to-archive would feed the source its own tail.
        let mut cfg = config.clone();
        cfg.event_archive_path = None;
        cfg.event_archive_max_mb = 0;
        cfg.event_archive_rotate_mb = 0;
        cfg.event_archive_min_free_mb = 0;
        run_backtest(
            path,
            args.backtest_report.as_deref(),
            cfg,
            args.backtest_tick_ms,
            args.backtest_tail_ms,
            args.backtest_knobs,
        );
        return Ok(());
    }
    if !args.backtest_knobs.is_empty() {
        eprintln!(
            "blitzkrieg-core: --backtest-knob only applies to a replay; pass --backtest <archive.jsonl>"
        );
        std::process::exit(2);
    }
    if let Some(schedule) = args.fee_model {
        eprintln!(
            "blitzkrieg-core: --fee-model {} only applies to a replay; pass --backtest <archive.jsonl> \
             (the schedule a live run charges is not a per-invocation choice, #203)",
            schedule.name
        );
        std::process::exit(2);
    }

    // ───────────────────────── #199: the data-directory latch ────────────────
    // Everything above this line is read-only or in-memory, and every offline
    // mode (--replay, --regime-eval, --backtest) has already returned. The
    // serving path is where the ledgers are first touched — `server::run` →
    // `Core::new` reads them, restore and the first append write them — so the
    // latch is taken HERE. That placement is the whole reason a refused boot
    // writes nothing: no lock file, no ledger line, no side file, anywhere.
    let mode_str = match mode {
        Mode::Dry => "dry",
        Mode::Live => "live",
        Mode::ReadOnly => "readonly",
    };
    // The escape hatch. `BLITZKRIEG_ALLOW_SHARED_DATA` is spelled with the
    // issue's prefix rather than the `BK_*` namespace `EnvVars` collects,
    // because it is the one knob a fixture author copies out of the issue text.
    // An unparseable value is treated as UNSET (fail closed): a typo must never
    // be what silently disables the guard.
    let allow_shared_data = args.allow_shared_data
        || match std::env::var("BLITZKRIEG_ALLOW_SHARED_DATA") {
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" | "" => false,
                other => {
                    eprintln!(
                        "blitzkrieg-core: BLITZKRIEG_ALLOW_SHARED_DATA={other:?} is not a \
                         boolean; treating it as UNSET (the data directories stay guarded)"
                    );
                    false
                }
            },
            Err(_) => false,
        };

    // The latch goes on the parent directory of each EFFECTIVE ledger path, not
    // on a hardcoded `data/`: `data/` is only this process's cwd-relative
    // default, so a deployment with `--trade-log /mnt/ledger/trades.jsonl` would
    // be guarded in the wrong place by a fixed path. Three directories under the
    // defaults, one when a harness points all three logs at one temp directory
    // (and two fixtures in two temp directories never see each other), none when
    // every ledger is disabled. `data_lock`'s module docs carry the full
    // argument, including why #196's socket probe is not a substitute.
    let ledger_logs: [(&str, Option<&str>); 3] = [
        ("trade-log", config.trade_log_path.as_deref()),
        ("order-log", config.order_log_path.as_deref()),
        ("position-log", config.position_log_path.as_deref()),
    ];
    let anchors = blitzkrieg_core::data_lock::ledger_anchors(&ledger_logs);

    // "Where did this process write?" answered at boot, in absolute terms,
    // without knowing the cwd it was started from — the incident behind #199 was
    // found by hand, days later, by comparing files nobody had been told were
    // being written. Absolute but deliberately NOT canonicalized: the files may
    // not exist yet, and on macOS canonicalizing `/tmp` would print a path the
    // operator never typed.
    for (label, path) in [
        ("trade-log", config.trade_log_path.as_deref()),
        ("order-log", config.order_log_path.as_deref()),
        ("position-log", config.position_log_path.as_deref()),
        ("near-miss-log", config.near_miss_path.as_deref()),
        ("event-archive", config.event_archive_path.as_deref()),
        ("strategy-state", config.strategy_state_path.as_deref()),
        ("shadow-evolution-audit", args.se_audit_dir.as_deref()),
    ] {
        match path {
            Some(p) => {
                let abs = blitzkrieg_core::data_lock::absolute(std::path::Path::new(p));
                if abs.as_path() == std::path::Path::new(p) {
                    eprintln!("blitzkrieg-core: write path {label}: {}", abs.display());
                } else {
                    eprintln!(
                        "blitzkrieg-core: write path {label}: {} (from {p})",
                        abs.display()
                    );
                }
            }
            None => eprintln!("blitzkrieg-core: write path {label}: (disabled)"),
        }
    }
    // The side files that follow those paths are where a "the core only writes
    // trades.jsonl" assumption goes wrong: the trade summary, the fill/audit/
    // settlement journals and the daily-loss state all live in the same
    // directories, so a latch on the log file alone would not describe the
    // footprint. Naming them here keeps the banner honest about the set.
    eprintln!(
        "blitzkrieg-core: side files: summary.json beside the trade log; applied-fills.jsonl / \
         reconcile.jsonl / settlements.jsonl beside the order log; <position-log>.daily-loss.json \
         beside the position log"
    );

    let lock_now = server::now_ms();
    let record =
        blitzkrieg_core::data_lock::LockRecord::for_this_process(&args.socket, mode_str, lock_now);
    // A refusal propagates out of `main` as a non-zero exit with the operator
    // text `DataDirsBusy` renders (occupant pid / socket / mode / start time,
    // per busy directory, plus the remedy). The anchors were all decided before
    // this call and the call writes only what it decided it owns, so a refusal
    // leaves the filesystem exactly as it found it.
    let data_lock =
        blitzkrieg_core::data_lock::DataLock::acquire(&anchors, &record, allow_shared_data)?;
    if anchors.is_empty() {
        eprintln!(
            "blitzkrieg-core: data lock: nothing to claim (every ledger is disabled by \
             --no-trade-log / --no-order-log / --no-position-log)"
        );
    } else {
        let dirs = data_lock
            .held()
            .iter()
            .map(|h| h.dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "blitzkrieg-core: data lock: pid {} claimed {} data director{}: {}",
            record.pid,
            anchors.len(),
            if anchors.len() == 1 { "y" } else { "ies" },
            dirs
        );
    }
    if allow_shared_data {
        // Printed even when no live owner was found: the flag is the promise
        // that this run may share, and the log reader (not the operator who
        // typed it) is who needs to know a guard was switched off.
        eprintln!(
            "blitzkrieg-core: WARNING SHARED DATA MODE: --allow-shared-data \
             (BLITZKRIEG_ALLOW_SHARED_DATA=1) is set, so a LIVE core writing the same data \
             directories will NOT refuse this one. Two writers break the trade/order/position \
             identities the daily-loss breaker and the reconcile audit rest on. Fixtures and \
             backtests only — never a deployment."
        );
    }
    for line in data_lock.banner(lock_now) {
        eprintln!("blitzkrieg-core: {line}");
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    // Keep the sender alive for the process lifetime; shutdown is driven by
    // SIGTERM/SIGINT inside the server. The handle is available for an embedded
    // caller that wants programmatic shutdown.
    let _shutdown_tx = tx;

    server::run(args.socket.clone(), config, args.tick_ms, rx).await?;
    eprintln!("blitzkrieg-core stopped");
    Ok(())
}

/// Offline MarketRegime evaluation (E16 / #98): label the archive's windows
/// with the offline rule and score the online state machine against those
/// labels. Builds no Core, never trades, never writes to the archive.
fn run_regime_eval(
    archive: &str,
    report_path: Option<&str>,
    args: blitzkrieg_core::regime_eval::RegimeEvalArgs,
) {
    use blitzkrieg_core::regime_eval::{render_markdown, run_eval};
    eprintln!("blitzkrieg-core: regime eval over {archive}");
    let report = match run_eval(std::path::Path::new(archive), &args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("regime-eval: {e}");
            std::process::exit(2);
        }
    };
    let verdict = &report["accuracy"];
    eprintln!(
        "blitzkrieg-core: regime eval — windows {}, agree {}, accuracy {:.2}% (pass ≥80: {})",
        verdict["windows"],
        verdict["agree"],
        verdict["accuracyPct"].as_f64().unwrap_or(0.0),
        verdict["pass80"].as_bool().unwrap_or(false),
    );
    if let Some(p) = report_path {
        if let Some(dir) = std::path::Path::new(p).parent()
            && !dir.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                let _ = std::fs::write(p, json + "\n");
            }
            Err(e) => eprintln!("regime-eval: report json failed: {e}"),
        }
        let md_path = if let Some(base) = p.strip_suffix(".json") {
            format!("{base}.md")
        } else {
            format!("{p}.md")
        };
        let _ = std::fs::write(&md_path, render_markdown(&report));
        eprintln!("blitzkrieg-core: regime report {p} (+ .md)");
    } else if let Ok(json) = serde_json::to_string_pretty(&report) {
        println!("{json}");
    }
}

/// Offline event-driven backtest (P-1.2): replay a market-data archive through
/// the same core the live path runs. Always dry: the backtester forces dry mode,
/// starts no feeds and writes no trade/order/position logs.
fn run_backtest(
    archive: &str,
    report_path: Option<&str>,
    cfg: CoreConfig,
    tick_ms: i64,
    tail_ms: i64,
    knobs: Vec<(String, String, Decimal)>,
) {
    use blitzkrieg_core::backtest::{BacktestConfig, Backtester, EventBacktester};
    use blitzkrieg_core::data_source::open_replay_all;

    if !cfg.engine_enabled {
        eprintln!(
            "blitzkrieg-core: --backtest without --engine: no strategy will run (pass --engine)"
        );
    }
    // Reads the archive plus any rotated segments beside it, so replaying a 24/7
    // capture (which rotates) needs no manual concatenation.
    let src = match open_replay_all(archive) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("backtest: {e}");
            std::process::exit(2);
        }
    };
    let core_cfg = cfg.clone();
    for (strategy, knob, value) in &knobs {
        eprintln!("blitzkrieg-core: counterfactual {strategy}.{knob} = {value}");
    }
    let mut bt = EventBacktester::new(
        BacktestConfig {
            core: cfg,
            tick_ms,
            tail_ms,
            hot_params: knobs,
        },
        Box::new(src),
    );
    eprintln!("blitzkrieg-core: {}", bt.describe());
    eprintln!(
        "blitzkrieg-core: replay is always dry (tick {tick_ms} ms, tail {tail_ms} ms, seed ${})",
        core_cfg.dry_seed_balance
    );
    match bt.run() {
        Ok(report) => {
            print!("{}", report.render());
            if let Some(p) = report_path {
                match serde_json::to_string_pretty(&report) {
                    Ok(json) => {
                        if let Some(dir) = std::path::Path::new(p).parent()
                            && !dir.as_os_str().is_empty()
                        {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        match std::fs::write(p, format!("{json}\n")) {
                            Ok(()) => println!("backtest report: {p}"),
                            Err(e) => eprintln!("backtest: cannot write report {p}: {e}"),
                        }
                    }
                    Err(e) => eprintln!("backtest: cannot encode report: {e}"),
                }
            }
        }
        Err(e) => {
            eprintln!("backtest failed: {e}");
            std::process::exit(2);
        }
    }
}

/// Offline walk-forward over a shadow JSONL file, using the SAME exit policy the
/// live core trades with. Prints an in-sample grid and the out-of-sample result.
fn run_replay(path: &std::path::Path) {
    use blitzkrieg_core::exit_policy::ExitConfig;
    use blitzkrieg_core::shadow::{
        bucket_by_entry, default_grid, frozen_holdout, walk_forward_file,
    };
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    let base_exit = ExitConfig::default();
    let grid = default_grid();
    match walk_forward_file(path, &base_exit, &grid, 6) {
        Ok(r) => {
            println!("shadow replay: {}", path.display());
            println!("\nIn-sample grid (optimistic):");
            let mut rows = r.in_sample.clone();
            rows.sort_by_key(|r| std::cmp::Reverse(r.1));
            for (name, pnl) in &rows {
                println!("  {:<20} {:>8.2}", name, pnl);
            }
            println!(
                "\nWalk-forward out-of-sample: {} records, PnL {:.2} (train from {})",
                r.oos_count, r.oos_pnl, r.min_train
            );
            if let Some((name, best)) = rows.first() {
                println!(
                    "In-sample best: {} {:.2} — compare with OOS to judge overfitting",
                    name, best
                );
            }

            // Strict frozen holdout: choose once on the early window, apply frozen
            // to the later window. The shipped cell must win on BOTH to be trusted;
            // an adaptive walk-forward alone can flatter a single era.
            if let Ok(text) = std::fs::read_to_string(path) {
                let mut recs: Vec<_> = text
                    .lines()
                    .filter_map(blitzkrieg_core::shadow::parse_shadow_line)
                    .collect();
                recs.sort_by_key(|rec| rec.entered_at_ms);
                for frac in [0.5_f64, 0.6] {
                    let h = frozen_holdout(&recs, &base_exit, &grid, frac);
                    if h.test_n == 0 {
                        continue;
                    }
                    println!(
                        "\nFrozen holdout (train first {:.0}% = {} trades, test last = {} trades):",
                        frac * 100.0,
                        h.train_n,
                        h.test_n
                    );
                    println!(
                        "  {:<20} {:>9} {:>6} | {:>9} {:>6}",
                        "cell", "train$", "win%", "test$", "win%"
                    );
                    for row in &h.rows {
                        let tw = if h.train_n > 0 {
                            row.train_wins as f64 / h.train_n as f64 * 100.0
                        } else {
                            0.0
                        };
                        let ew = if h.test_n > 0 {
                            row.test_wins as f64 / h.test_n as f64 * 100.0
                        } else {
                            0.0
                        };
                        let mark = if row.name == h.train_best {
                            " <-train-best"
                        } else {
                            ""
                        };
                        println!(
                            "  {:<20} {:>9.2} {:>5.0}% | {:>9.2} {:>5.0}%{}",
                            row.name, row.train_pnl, tw, row.test_pnl, ew, mark
                        );
                    }
                    println!(
                        "  frozen forward: {} -> test PnL {:.2}",
                        h.train_best, h.frozen_test_pnl
                    );
                }
            }

            // Entry-side analysis over the same records (tests the entry cap and
            // the timing gate on trades that actually got in).
            if let Ok(text) = std::fs::read_to_string(path) {
                let recs: Vec<_> = text
                    .lines()
                    .filter_map(blitzkrieg_core::shadow::parse_shadow_line)
                    .collect();
                let (by_price, by_time) = bucket_by_entry(
                    &recs,
                    &[dec!(0.40), dec!(0.43), dec!(0.45)],
                    &[dec!(420), dec!(600), dec!(720)],
                );
                println!("\nBy entry price (entry-cap question):");
                println!(
                    "  {:<12} {:>4} {:>9} {:>9} {:>7}",
                    "bucket", "n", "sum$", "med$", "win%"
                );
                for b in &by_price {
                    if b.n > 0 {
                        println!(
                            "  {:<12} {:>4} {:>9.2} {:>9.3} {:>6.0}%",
                            b.label, b.n, b.sum, b.median, b.win_rate
                        );
                    }
                }
                println!("\nBy time-left at entry (timing-gate question):");
                println!(
                    "  {:<12} {:>4} {:>9} {:>9} {:>7}",
                    "bucket", "n", "sum$", "med$", "win%"
                );
                for b in &by_time {
                    if b.n > 0 {
                        println!(
                            "  {:<12} {:>4} {:>9.2} {:>9.3} {:>6.0}%",
                            b.label, b.n, b.sum, b.median, b.win_rate
                        );
                    }
                }
                println!(
                    "\nNOTE: shadow data only holds trades that ALREADY passed the entry gate;"
                );
                println!("it cannot see opportunities the gate rejected (never recorded).");
                let _ = Decimal::ZERO;
            }
        }
        Err(e) => eprintln!("replay failed: {e}"),
    }
}

/// Offline evaluation of blocked near-misses: replays the exit policy over each
/// blocked signal's recorded path and reports what taking those trades would
/// have earned/cost — the data needed to judge relaxing the entry gate.
fn run_replay_near_miss(path: &std::path::Path) {
    use blitzkrieg_core::exit_policy::ExitConfig;
    use blitzkrieg_core::shadow::{evaluate_near_misses, parse_near_miss_line};
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read {}: {e}", path.display());
            return;
        }
    };
    let recs: Vec<_> = text.lines().filter_map(parse_near_miss_line).collect();
    if recs.is_empty() {
        println!("no near-miss records in {}", path.display());
        return;
    }
    let base = ExitConfig::default();
    let s_same = evaluate_near_misses(&recs, &base);

    // Apples-to-apples: relaxing the entry gate is meaningless unless the EXIT
    // time gate is relaxed too (a signal blocked at tLeft < min_time_left would
    // otherwise be time-exited instantly). Compare both.
    let relaxed = ExitConfig {
        min_time_left_sec: 0,
        force_exit_sec: 0,
        ..Default::default()
    };
    let s_relaxed = evaluate_near_misses(&recs, &relaxed);

    println!("near-miss replay: {}", path.display());
    println!(
        "  blocked signals   : {} (timing {}, momentum {})",
        s_same.n, s_same.timing_n, s_same.momentum_n
    );
    println!(
        "  notional tradable : ${:.0}",
        s_same.total_feeable_notional
    );
    println!();
    println!("  A) same exit gate (entry relaxed only):");
    println!(
        "     PnL ${:.2}  avg ${:.3}  win {}%",
        s_same.total_pnl,
        s_same.avg_pnl,
        pct(s_same.wins, s_same.n)
    );
    println!("  B) entry AND exit time gate relaxed   :");
    println!(
        "     PnL ${:.2}  avg ${:.3}  win {}%",
        s_relaxed.total_pnl,
        s_relaxed.avg_pnl,
        pct(s_relaxed.wins, s_relaxed.n)
    );
    println!();
    println!("  (A) is near-zero because blocked signals have tLeft < min_time_left and");
    println!("  are time-exited immediately — this is the entry/exit timing COUPLING.");
    println!("  (B) is the real test of relaxing the timing gate.");
    if s_relaxed.total_pnl > rust_decimal::Decimal::ZERO {
        println!("  => On this sample, relaxing the timing gate would have HELPED (B > 0),");
        println!(
            "     but n={} is small and regime-dependent — gather more before acting.",
            s_relaxed.n
        );
    } else {
        println!(
            "  => On this sample, blocked trades would have LOST even with relaxed timing (B <= 0):"
        );
        println!("     the gate is doing its job; do not relax it on this evidence.");
    }
}

fn pct(wins: usize, n: usize) -> usize {
    (wins * 100).checked_div(n).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    // ── E2-a: the --strategy-limit grammar ──────────────────────────────────

    fn parse_one(flag: &str) -> Option<blitzkrieg_core::service::StrategyLimit> {
        parse_strategy_limits(&[flag.to_string()])
            .into_iter()
            .next()
            .map(|(_, l)| l)
    }

    #[test]
    fn three_segment_form_still_parses_as_caps_only() {
        let l = parse_one("spread_arb:0:-").expect("legacy form must still parse");
        assert_eq!(l.max_open_positions, Some(0));
        assert_eq!(l.max_open_notional_usd, None);
        assert!(l.size_usd.is_none() && l.min_shares.is_none() && l.max_shares.is_none());
        assert!(
            !l.sizing().overrides_anything(),
            "caps-only must not masquerade as a sizing override"
        );

        let l2 = parse_one("dip_buyer:-:12.5").expect("blank cap segment = uncapped");
        assert_eq!(l2.max_open_positions, None);
        assert_eq!(l2.max_open_notional_usd, Some(dec!(12.5)));
    }

    #[test]
    fn six_segment_form_parses_per_strategy_sizing() {
        let l = parse_one("dip_buyer:2:20:1.5:4:8").expect("extended form must parse");
        assert_eq!(l.max_open_positions, Some(2));
        assert_eq!(l.max_open_notional_usd, Some(dec!(20)));
        assert_eq!(l.size_usd, Some(dec!(1.5)));
        assert_eq!(l.min_shares, Some(dec!(4)));
        assert_eq!(l.max_shares, Some(dec!(8)));
        assert!(l.sizing().overrides_anything());

        // "-" inherits the global per dimension, independently of the others.
        let l2 = parse_one("x:-:-:-:3:-").expect("blank sizing segments inherit the globals");
        assert_eq!(l2.size_usd, None);
        assert_eq!(l2.min_shares, Some(dec!(3)));
        assert_eq!(l2.max_shares, None);
        assert!(
            l2.sizing().overrides_anything(),
            "one set dimension is enough"
        );
    }

    #[test]
    fn seven_segment_form_parses_allocation_weight() {
        // E16/#98: the 7th segment is the leg's allocation weight.
        let l = parse_one("spread_arb:2:20:1.5:4:8:0.5").expect("weight form must parse");
        assert_eq!(l.size_weight, Some(dec!(0.5)));
        assert!(l.sizing().overrides_anything());
        // "-" = unweighted; an absent 7th segment is the 6-segment form.
        let l2 = parse_one("spread_arb:2:20:1.5:4:8:-").expect("blank weight = unweighted");
        assert_eq!(l2.size_weight, None);
        // Bad weight drops the whole flag.
        assert!(parse_one("spread_arb:0:-:1:2:3:x").is_none());
    }

    #[test]
    fn malformed_strategy_limits_are_dropped_not_half_applied() {
        for bad in [
            "spread_arb:0",               // too few segments
            "spread_arb:0:-:1:2:3:4:5:6", // too many
            ":0:-",                       // no name
            "spread_arb:x:-",             // bad position cap
            "spread_arb:0:abc",           // bad notional
            "spread_arb:0:-:abc:1:2",     // bad size_usd
            "spread_arb:0:-:1:abc:2",     // bad min_shares
            "spread_arb:0:-:1:2:abc",     // bad max_shares
            "spread_arb:0:-:1:2:3:4:x",   // bad size_pct (#202)
        ] {
            assert!(parse_one(bad).is_none(), "must be ignored: {bad}");
        }
        assert!(parse_strategy_limits(&[]).is_empty());
    }

    #[test]
    fn repeated_flags_keep_the_last_entry_per_name() {
        let m = parse_strategy_limits(&[
            "a:1:-".to_string(),
            "b:2:-:1:1:1".to_string(),
            "a:5:-".to_string(),
        ]);
        assert_eq!(m.len(), 2);
        assert_eq!(m["a"].max_open_positions, Some(5), "last flag wins");
        assert_eq!(m["b"].min_shares, Some(dec!(1)));
    }

    #[test]
    fn engine_session_records_by_default() {
        let (path, max, rotate, min_free) =
            resolve_event_archive(true, false, None, None, None, None);
        assert_eq!(path.as_deref(), Some(DEFAULT_EVENT_ARCHIVE));
        assert_eq!(
            max, 0,
            "no session cap: rotation + the space floor bound the volume"
        );
        assert_eq!(rotate, 256);
        assert_eq!(min_free, 5120);
    }

    #[test]
    fn non_engine_session_does_not_record() {
        // The plain trading core (no --engine) has no market-data consumers, so
        // there is nothing to archive; the default must not leave an empty file.
        let (path, _, rotate, _) = resolve_event_archive(false, false, None, None, None, None);
        assert_eq!(path, None);
        assert_eq!(rotate, 0);
    }

    #[test]
    fn explicit_path_opts_in_without_engine() {
        let (path, ..) =
            resolve_event_archive(false, false, Some("/tmp/x.jsonl"), None, None, None);
        assert_eq!(path.as_deref(), Some("/tmp/x.jsonl"));
    }

    #[test]
    fn explicit_tuning_beats_the_default() {
        let (path, max, rotate, min_free) =
            resolve_event_archive(true, false, None, Some(64), Some(8), Some(100));
        assert_eq!(path.as_deref(), Some(DEFAULT_EVENT_ARCHIVE));
        assert_eq!((max, rotate, min_free), (64, 8, 100));
    }

    #[test]
    fn explicit_zero_tuning_is_honoured_not_defaulted() {
        // 0 is a meaningful request (no cap / never rotate / no space guard) and
        // must survive; only an ABSENT flag falls back to the default.
        let (_, max, rotate, min_free) =
            resolve_event_archive(true, false, None, Some(0), Some(0), Some(0));
        assert_eq!((max, rotate, min_free), (0, 0, 0));
    }

    #[test]
    fn no_event_archive_disables_capture_even_with_a_path() {
        let (path, max, rotate, min_free) = resolve_event_archive(
            true,
            true,
            Some("/tmp/x.jsonl"),
            Some(64),
            Some(8),
            Some(100),
        );
        assert_eq!((path, max, rotate, min_free), (None, 0, 0, 0));
    }

    // ── KI-11: the file-config source and its precedence ────────────────────

    /// Parse an argv with no config file and no env overrides in play.
    fn args_from(flags: &[&str]) -> Args {
        let argv: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &argv,
            &EnvVars::default(),
        )
    }

    /// An env layer built from literal pairs, so the env tier of the precedence
    /// chain is testable without touching the process environment.
    fn env_of(pairs: &[(&str, &str)]) -> EnvVars {
        EnvVars {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// Write `text` to a scratch `default.toml` and load it the way the kernel
    /// does (the loader consults the file system, so a temp file is the honest
    /// way to exercise it).
    fn file_with(text: &str) -> blitzkrieg_core::config::FileConfig {
        let dir = std::env::temp_dir().join(format!(
            "bk-main-cfg-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("default.toml");
        std::fs::write(&path, text).unwrap();
        let cfg = blitzkrieg_core::config::FileConfig::load(Some(&path));
        std::fs::remove_dir_all(&dir).ok();
        cfg
    }

    #[test]
    fn defaults_hold_without_a_file_or_flags() {
        let a = args_from(&[]);
        assert_eq!(a.round_sec, 900);
        assert_eq!(a.min_round_age, 30);
        assert_eq!(a.min_time_left, 180);
        assert_eq!(a.assets, vec!["BTC", "ETH", "SOL", "XRP"]);
        assert!(
            a.config_report.is_empty(),
            "nothing is explicit, so nothing is reported: {:?}",
            a.config_report
        );
    }

    // ── #173: the daily-loss breaker's CLI/env surface ─────────────────────

    /// The compiled default is RELATIVE: an absolute default is meaningless
    /// across account sizes (the old 200 USD was no protection at all on a
    /// 4.8 USDC book), so the percentage carries the default protection.
    #[test]
    fn the_daily_loss_budget_defaults_to_a_relative_cap_and_is_always_echoed() {
        let a = args_from(&[]);
        assert_eq!(
            a.daily_loss_usd,
            Decimal::ZERO,
            "no absolute cap by default"
        );
        assert_eq!(a.daily_loss_pct, dec!(20));
        assert!(
            a.config_report.is_empty(),
            "the echo is not part of config_report: {:?}",
            a.config_report
        );
        let echo = a.daily_loss_echo.join("\n");
        assert!(echo.contains("relative cap 20%"), "{echo}");
        assert!(echo.contains("default"), "{echo}");
    }

    #[test]
    fn a_max_daily_loss_flag_overrides_the_budget_and_is_reported() {
        let a = args_from(&["--max-daily-loss", "5"]);
        assert_eq!(a.daily_loss_usd, dec!(5));
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "positions.max_daily_loss_usd=5 (cli)"),
            "{:?}",
            a.config_report
        );
        // The echo reports the effective configuration whether or not it moved.
        assert!(
            a.daily_loss_echo
                .join("\n")
                .contains("absolute cap $5 (cli)")
        );
    }

    #[test]
    fn the_daily_loss_budget_is_settable_from_the_environment() {
        let args = ["--no-config".to_string()];
        let env = env_of(&[
            ("BK_MAX_DAILY_LOSS", "3.5"),
            ("BK_MAX_DAILY_LOSS_PCT", "10"),
        ]);
        let a = parse_args(&blitzkrieg_core::config::FileConfig::default(), &args, &env);
        assert_eq!(a.daily_loss_usd, dec!(3.5));
        assert_eq!(a.daily_loss_pct, dec!(10));
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "positions.max_daily_loss_usd=3.5 (env)"),
            "{:?}",
            a.config_report
        );
        // CLI outranks env, like every other setting in the chain.
        let a = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--max-daily-loss".into(), "9".into()],
            &env,
        );
        assert_eq!(a.daily_loss_usd, dec!(9));
    }

    /// 0/0 is the one configuration that protects nothing, so it says so out loud.
    #[test]
    fn a_disabled_daily_loss_budget_is_announced() {
        let a = args_from(&["--max-daily-loss", "0", "--max-daily-loss-pct", "0"]);
        assert!(a.daily_loss_echo.join("\n").contains("DISABLED"));
    }

    /// The budget's side file is a sibling of the position log, the way the
    /// `.recon` watermark is: same directory, derived name.
    #[test]
    fn the_daily_loss_file_follows_the_position_log() {
        assert_eq!(
            daily_loss_path_for("data/positions/positions.jsonl"),
            "data/positions/positions.daily-loss.json"
        );
        assert_eq!(
            daily_loss_path_for("/tmp/x/pos.jsonl"),
            "/tmp/x/pos.daily-loss.json"
        );
    }

    // ── #202: the equity-relative sizing / per-order bound ──────────────────

    /// Both knobs default to OFF — the shipped absolute path, order sizes
    /// unchanged — and the boot log says so, including the number the operator
    /// would otherwise have to derive: the widest ticket the defaults allow as a
    /// share of the live 4.8 USDC book.
    #[test]
    fn the_equity_knobs_default_to_off_and_the_echo_states_the_worst_case() {
        let a = args_from(&[]);
        assert_eq!(a.size_pct, Decimal::ZERO);
        assert_eq!(a.max_order_notional_pct, Decimal::ZERO);
        assert!(
            !a.config_report
                .iter()
                .any(|l| l.contains("size_pct") || l.contains("notional_pct")),
            "nothing explicit, nothing reported: {:?}",
            a.config_report
        );
        let echo = a.sizing_echo.join("\n");
        assert!(echo.contains("absolute size_usd"), "{echo}");
        assert!(
            echo.contains("(default; --size-pct/BK_SIZE_PCT off)"),
            "{echo}"
        );
        assert!(echo.contains("NO equity-relative cap"), "{echo}");
        assert!(echo.contains("208% of a 4.8 USDC book"), "{echo}");
        assert!(echo.contains("scripts/risk-sizing-check.mjs"), "{echo}");
    }

    #[test]
    fn the_equity_knobs_are_settable_by_flag_and_by_env() {
        let a = args_from(&["--size-pct", "20", "--max-order-notional-pct", "20"]);
        assert_eq!(a.size_pct, dec!(20));
        assert_eq!(a.max_order_notional_pct, dec!(20));
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.size_pct=20 (cli)"),
            "{:?}",
            a.config_report
        );
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "risk.max_order_notional_pct=20 (cli)"),
            "{:?}",
            a.config_report
        );
        let echo = a.sizing_echo.join("\n");
        assert!(
            echo.contains("20% of cash equity (cli; replaces size_usd)"),
            "{echo}"
        );
        assert!(echo.contains("REJECTED, never truncated"), "{echo}");

        // Same chain as every other knob: env supplies, CLI outranks.
        let env = env_of(&[("BK_SIZE_PCT", "7.5"), ("BK_MAX_ORDER_NOTIONAL_PCT", "15")]);
        let b = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--no-config".to_string()],
            &env,
        );
        assert_eq!(b.size_pct, dec!(7.5));
        assert_eq!(b.max_order_notional_pct, dec!(15));
        assert!(
            b.config_report
                .iter()
                .any(|l| l == "engine.size_pct=7.5 (env)"),
            "{:?}",
            b.config_report
        );
        let c = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--size-pct".to_string(), "9".to_string()],
            &env,
        );
        assert_eq!(c.size_pct, dec!(9), "CLI must outrank the environment");
        assert_eq!(
            c.max_order_notional_pct,
            dec!(15),
            "untouched env knob stays"
        );
    }

    #[test]
    fn eight_segment_form_parses_the_legs_own_equity_pct() {
        // #202: the 8th segment is the leg's own share of the account.
        let l = parse_one("dip_buyer:2:20:1.5:4:8:0.5:10").expect("8-segment form must parse");
        assert_eq!(l.size_weight, Some(dec!(0.5)));
        assert_eq!(l.size_pct, Some(dec!(10)));
        assert!(l.sizing().overrides_anything());
        // The weight slot stays mandatory, so a percentage can never be read as
        // a weight ("-" skips the weight, the same convention as every segment).
        let l2 = parse_one("dip_buyer:2:20:1.5:4:8:-:10").expect("blank weight = unweighted");
        assert_eq!(l2.size_weight, None);
        assert_eq!(l2.size_pct, Some(dec!(10)));
        // The 7- and 6-segment forms keep their own meaning: no percentage.
        assert_eq!(
            parse_one("dip_buyer:2:20:1.5:4:8:0.5").unwrap().size_pct,
            None
        );
        assert_eq!(parse_one("dip_buyer:2:20:1.5:4:8").unwrap().size_pct, None);
    }

    #[test]
    fn the_file_supplies_a_value_when_no_flag_does() {
        let file = file_with(
            r#"
            [engine]
            assets = ["DOGE"]
            round_sec = 300
            min_round_age_sec = 5
            min_time_left_sec = 7
            "#,
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(a.round_sec, 300);
        assert_eq!(a.min_round_age, 5);
        assert_eq!(a.min_time_left, 7);
        assert_eq!(a.assets, vec!["DOGE"]);
        // Each resolved value names its origin, so the log can answer "why 300?".
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.round_sec=300 (toml)")
        );
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.min_time_left_sec=7 (toml)")
        );
    }

    #[test]
    fn a_flag_beats_the_file() {
        let file = file_with(
            r#"
            [engine]
            round_sec = 300
            min_time_left_sec = 7
            "#,
        );
        let a = parse_args(
            &file,
            &["--round-sec".into(), "60".into()],
            &EnvVars::default(),
        );
        assert_eq!(a.round_sec, 60, "CLI outranks TOML");
        assert_eq!(
            a.min_time_left, 7,
            "and the untouched key still comes from TOML"
        );
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.round_sec=60 (cli)")
        );
    }

    #[test]
    fn the_file_can_turn_shadow_evolution_on_and_the_flag_is_a_no_op() {
        // The documented way to enable the feature: edit the file. Before KI-11
        // this had no effect at all.
        let file = file_with(
            r#"
            [shadow_evolution]
            enabled = true
            min_sample_count = 12
            cooldown_minutes = 2
            evaluation_window_minutes = 5
            min_observation_minutes = 1
            variant_count = 4
            audit_dir = "data/evo-scratch"
            "#,
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert!(a.shadow_evolution);
        assert_eq!(a.se_min_samples, Some(12));
        // Minutes in the file, seconds in the kernel.
        assert_eq!(a.se_cooldown_secs, Some(120));
        assert_eq!(a.se_eval_window_secs, Some(300));
        assert_eq!(a.se_min_obs_secs, Some(60));
        assert_eq!(a.se_variant_count, Some(4));
        assert_eq!(a.se_audit_dir.as_deref(), Some("data/evo-scratch"));
    }

    #[test]
    fn a_strategy_section_is_reported_as_dead_config_not_silently_ignored() {
        // PR-B removed `[strategy] active`: strategies are never named in kernel
        // config any more (they are auto-loaded from strategy_dir and enabled on
        // load). An old file must not silently look like it did something.
        let file = file_with(
            r#"
            [strategy]
            active = ["spread_arb", "trend_follow"]
            "#,
        );
        assert!(
            file.unknown_keys.iter().any(|k| k == "strategy"),
            "the dead section must be reported: {:?}",
            file.unknown_keys
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert!(
            a.enable_strategy.is_empty(),
            "the kernel must not invent an enable list from the file: {:?}",
            a.enable_strategy
        );
    }

    #[test]
    fn strategy_selection_comes_from_cli_flags_only() {
        let a = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &[
                "--enable-strategy".into(),
                "dog_strategy".into(),
                "--disable-strategy".into(),
                "spread_arb".into(),
            ],
            &EnvVars::default(),
        );
        assert_eq!(a.enable_strategy, vec!["dog_strategy"]);
        assert_eq!(a.disable_strategy, vec!["spread_arb"]);
    }

    #[test]
    fn a_gradient_above_lock_1_is_refused_not_silently_applied() {
        let file = file_with(
            r#"
            [shadow_evolution]
            max_gradient = 0.25
            "#,
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(
            a.se_max_gradient, None,
            "a request to widen a safety lock must not take effect"
        );

        // Tightening it is allowed.
        let file = file_with(
            r#"
            [shadow_evolution]
            max_gradient = 0.01
            "#,
        );
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(a.se_max_gradient, Some(dec!(0.01)));
    }

    #[test]
    fn a_broken_file_does_not_stop_startup() {
        // The parse_args half: a FileConfig that failed to load is all-defaults,
        // and the kernel still comes up on CLI + built-in values.
        let a = args_from(&["--round-sec", "120"]);
        assert_eq!(a.round_sec, 120);
        assert_eq!(a.assets, vec!["BTC", "ETH", "SOL", "XRP"]);
    }

    #[test]
    fn prescan_finds_the_config_flag_in_every_documented_spelling() {
        let v = |s: &[&str]| -> Vec<String> { s.iter().map(|x| x.to_string()).collect() };
        match prescan_config(&v(&["--config", "a.toml"])) {
            Some(ConfigChoice::Path(p)) => assert_eq!(p, "a.toml"),
            _ => panic!("--config <path> must parse"),
        }
        match prescan_config(&v(&["--config=b.toml"])) {
            Some(ConfigChoice::Path(p)) => assert_eq!(p, "b.toml"),
            _ => panic!("--config=<path> must parse"),
        }
        assert!(matches!(
            prescan_config(&v(&["--no-config"])),
            Some(ConfigChoice::Off)
        ));
        // Bare `--config` with nothing usable after it means "no file", not "the
        // next flag is my filename".
        assert!(matches!(
            prescan_config(&v(&["--config="])),
            Some(ConfigChoice::Off)
        ));
        assert!(matches!(
            prescan_config(&v(&["--config", ""])),
            Some(ConfigChoice::Off)
        ));
        assert!(prescan_config(&v(&["--engine"])).is_none());
    }

    #[test]
    fn the_env_layer_sits_between_the_flag_and_the_file() {
        let file = file_with(
            r#"
            [engine]
            round_sec = 300
            min_time_left_sec = 7
            "#,
        );
        let env = env_of(&[("BK_ROUND_SEC", "600"), ("BK_MIN_TIME_LEFT_SEC", "9")]);

        // CLI > env.
        let a = parse_args(&file, &["--round-sec".into(), "60".into()], &env);
        assert_eq!(a.round_sec, 60);
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.round_sec=60 (cli)")
        );
        // env > TOML.
        assert_eq!(
            a.min_time_left, 9,
            "the env var outranks the file for a key no flag named"
        );
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.min_time_left_sec=9 (env)")
        );
    }

    #[test]
    fn the_env_layer_alone_can_supply_a_value() {
        let env = env_of(&[
            ("BK_MIN_ROUND_AGE_SEC", "3"),
            ("BK_ASSETS", "btc, eth"),
            ("BK_SHADOW_EVOLUTION", "true"),
            ("BK_SE_COOLDOWN_SECS", "45"),
        ]);
        let a = parse_args(&blitzkrieg_core::config::FileConfig::default(), &[], &env);
        assert_eq!(a.min_round_age, 3);
        // Asset lists are normalised wherever they come from.
        assert_eq!(a.assets, vec!["BTC", "ETH"]);
        assert!(a.shadow_evolution);
        assert_eq!(a.se_cooldown_secs, Some(45));
    }

    #[test]
    fn bk_config_none_turns_the_file_off() {
        let env = env_of(&[("BK_CONFIG", "none")]);
        assert!(matches!(config_choice(&[], &env), ConfigChoice::Off));
        // Without the variable the shipped default is used.
        assert!(matches!(
            config_choice(&[], &EnvVars::default()),
            ConfigChoice::Path(p) if p == blitzkrieg_core::config::DEFAULT_CONFIG_PATH
        ));
    }

    #[test]
    fn a_config_path_that_does_not_exist_is_a_silent_no_op() {
        let file = blitzkrieg_core::config::FileConfig::load(Some(std::path::Path::new(
            "/nonexistent/bk.toml",
        )));
        let a = parse_args(&file, &[], &EnvVars::default());
        assert_eq!(a.round_sec, 900);
        assert!(a.config_report.is_empty());
    }

    // ── #205: the orderbook freshness budget is a runtime knob ──────────────

    /// The default must be the value the kernel has always hardcoded: making the
    /// knob configurable must not move it.
    #[test]
    fn the_orderbook_staleness_default_is_the_historic_eight_seconds() {
        let a = args_from(&[]);
        assert_eq!(a.max_orderbook_stale_ms, 8_000);
        assert_eq!(
            a.max_orderbook_stale_ms,
            blitzkrieg_core::engine::DEFAULT_MAX_ORDERBOOK_STALE_MS
        );
        assert!(
            a.config_report.is_empty(),
            "an unset knob is not a configured value: {:?}",
            a.config_report
        );
        assert!(
            a.orderbook_stale_echo.join(" ").contains("8000 ms"),
            "the boot line must state the effective budget: {:?}",
            a.orderbook_stale_echo
        );
        assert!(a.orderbook_stale_echo.join(" ").contains("default"));
    }

    /// CLI and env both reach it, CLI wins, and the boot line names the tier that
    /// supplied the value — the same shape as `--size-pct` / `BK_SIZE_PCT`.
    #[test]
    fn the_orderbook_staleness_budget_is_configurable_from_cli_and_env() {
        let a = args_from(&["--max-orderbook-stale-ms", "12000"]);
        assert_eq!(a.max_orderbook_stale_ms, 12_000);
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.max_orderbook_stale_ms=12000 (cli)"),
            "{:?}",
            a.config_report
        );
        assert!(a.orderbook_stale_echo.join(" ").contains("12000 ms (cli)"));

        let env = env_of(&[("BK_MAX_ORDERBOOK_STALE_MS", "15000")]);
        let a = parse_args(&blitzkrieg_core::config::FileConfig::default(), &[], &env);
        assert_eq!(a.max_orderbook_stale_ms, 15_000);
        assert!(
            a.config_report
                .iter()
                .any(|l| l == "engine.max_orderbook_stale_ms=15000 (env)"),
            "{:?}",
            a.config_report
        );
        // CLI outranks env, like every other setting in the chain.
        let a = parse_args(
            &blitzkrieg_core::config::FileConfig::default(),
            &["--max-orderbook-stale-ms".into(), "9000".into()],
            &env,
        );
        assert_eq!(a.max_orderbook_stale_ms, 9_000);
    }

    /// `0` is the supported OFF switch and must be announced as such rather than
    /// rendered as "a 0 ms budget" (which would read as "refuse everything").
    #[test]
    fn a_disabled_orderbook_staleness_check_is_announced() {
        let a = args_from(&["--max-orderbook-stale-ms", "0"]);
        assert_eq!(a.max_orderbook_stale_ms, 0);
        let echo = a.orderbook_stale_echo.join(" ");
        assert!(echo.contains("CHECK DISABLED"), "{echo}");
        assert!(echo.contains("cli"), "the tier is named: {echo}");
        assert!(
            stale_budget_rejection(0).is_none(),
            "0 is legal, not invalid"
        );
    }

    /// Invalid values are rejected by a rule the boot path enforces with exit 2;
    /// this pins the rule itself (the exit is exercised end-to-end from the
    /// shell). A negative budget has no meaning, and anything past the ceiling is
    /// a unit mistake (`8` for 8 seconds, `8_000_000` for 8000 ms).
    #[test]
    fn an_out_of_band_orderbook_staleness_budget_is_rejected() {
        for bad in [-1i64, -8_000, 600_001, 8_000_000] {
            assert!(
                stale_budget_rejection(bad).is_some(),
                "{bad} must be rejected"
            );
        }
        for ok in [0i64, 1, 8_000, 60_000, 600_000] {
            assert!(
                stale_budget_rejection(ok).is_none(),
                "{ok} must be accepted"
            );
        }
        let why = stale_budget_rejection(-5).unwrap();
        assert!(why.contains("0"), "the fix is named: {why}");
        let why = stale_budget_rejection(600_001).unwrap();
        assert!(why.contains("600000"), "the ceiling is named: {why}");
    }

    /// A value that is not a number at all fails the boot the same way, rather
    /// than being silently dropped: the flag parser rejects it outright.
    #[test]
    fn a_non_numeric_orderbook_staleness_budget_is_rejected_at_parse() {
        // `parse_args` exits the process on a bad value, so this asserts the
        // parse rule through the helper the arm uses.
        assert!("8s".trim().parse::<i64>().is_err());
        assert!("".trim().parse::<i64>().is_err());
        assert!(" 8000 ".trim().parse::<i64>().is_ok());
    }
}
