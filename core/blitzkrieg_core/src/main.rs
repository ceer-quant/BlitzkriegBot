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
//!                   [--near-miss-path <path>]
//!                   [--event-archive <path>] [--no-event-archive]
//!                   [--event-archive-max-mb 0]
//!                   [--event-archive-rotate-mb 256]
//!                   [--event-archive-min-free-mb 5120]
//!                   [--entry-maker-timeout-ms 5000]
//!                   [--slippage-ticks 0] [--latency-ms 0] [--fill-prob-bps 10000]
//!
//! `--readonly` is a promise, not a convention: the venue order egress is never
//! constructed, so no code path in this process can send an order. It outranks
//! `--mode live` / `DRY_RUN=false` (giving both is not an error — the read-only
//! promise simply wins) and settles orders like dry, since a ledger waiting on a
//! venue that was never started would never move.
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
    min_shares: Option<Decimal>,
    max_shares: Option<Decimal>,
    markets: Vec<String>,
    auto_exits: bool,
    max_positions: usize,
    engine: bool,
    min_round_age: i64,
    min_time_left: i64,
    trend_confirm: i64,
    trend_floor_ms: i64,
    feed_ws: bool,
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
    market_plugin: Option<String>,
    discovery: bool,
    shadow_evolution: bool,
    assets: Option<String>,
    se_min_samples: Option<u32>,
    se_cooldown_secs: Option<i64>,
    se_min_obs_secs: Option<i64>,
    /// Per-strategy entry caps: `name:max_open_positions:max_notional_usd`
    /// (repeatable; `-` or empty = no cap on that segment).
    strategy_limits: Vec<String>,
    /// Strategies to switch ON at startup (repeatable, E4-a). The builtins default
    /// to `spread_arb` on and `trend_follow` off, so this is how a session opts
    /// into the chase leg; an unknown name is warned about, never fatal.
    enable_strategy: Vec<String>,
    /// Strategies to switch OFF at startup (repeatable). Applied after
    /// `--enable-strategy`, so an explicit "off" wins.
    disable_strategy: Vec<String>,
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
    /// Offline replay of an archive through the same core (P-1.2).
    backtest: Option<String>,
    /// Where to write the backtest report as JSON (None = stdout only).
    backtest_report: Option<String>,
    /// Virtual-clock step (ms) between maintenance cycles in a replay.
    backtest_tick_ms: i64,
    /// Keep the replay clock running this long after the last event (ms).
    backtest_tail_ms: i64,
    /// Fill model: taker slippage in ticks (0.01).
    slippage_ticks: u32,
    /// Fill model: maker latency (ms).
    latency_ms: i64,
    /// Fill model: maker fill probability (bps of 10000); None = untouched.
    fill_prob_bps: Option<u32>,
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

fn parse_args() -> Args {
    let mut socket = default_socket();
    let mut mode = Mode::Dry;
    let mut readonly = false;
    let mut tick_ms = 50u64;
    let mut seed_balance = Decimal::from(10_000);
    let mut max_order_notional = Decimal::from(100);
    let mut min_shares: Option<Decimal> = None;
    let mut max_shares: Option<Decimal> = None;
    let mut markets: Vec<String> = Vec::new();
    let mut auto_exits = true;
    let mut max_positions: usize = 2;
    let mut strategy_limits: Vec<String> = Vec::new();
    let mut enable_strategy: Vec<String> = Vec::new();
    let mut disable_strategy: Vec<String> = Vec::new();
    let mut engine = false;
    let mut min_round_age: i64 = 30;
    let mut min_time_left: i64 = 180;
    let mut trend_confirm: i64 = 60;
    let mut trend_floor_ms: i64 = 10_000;
    let mut feed_ws = false;
    let mut replay: Option<String> = None;
    let mut replay_near_miss: Option<String> = None;
    let mut round_sec: i64 = 900;
    let mut near_miss_path: Option<String> = None;
    let mut trade_log: Option<String> = None;
    let mut no_trade_log = false;
    let mut order_log: Option<String> = None;
    let mut no_order_log = false;
    let mut position_log: Option<String> = None;
    let mut no_position_log = false;
    let mut market_plugin: Option<String> = None;
    let mut discovery = true;
    let mut shadow_evolution = false;
    let mut se_min_samples: Option<u32> = None;
    let mut se_cooldown_secs: Option<i64> = None;
    let mut se_min_obs_secs: Option<i64> = None;
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
    let mut slippage_ticks: u32 = 0;
    let mut latency_ms: i64 = 0;
    let mut fill_prob_bps: Option<u32> = None;

    let mut it = std::env::args().skip(1);
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
            "--near-miss-path" => near_miss_path = it.next(),
            "--trade-log" => trade_log = it.next(),
            "--no-trade-log" => no_trade_log = true,
            "--order-log" => order_log = it.next(),
            "--no-order-log" => no_order_log = true,
            "--position-log" => position_log = it.next(),
            "--no-position-log" => no_position_log = true,
            "--market-plugin" => market_plugin = it.next(),
            "--no-discovery" => discovery = false,
            "--shadow-evolution" => shadow_evolution = true,
            "--se-min-samples" => se_min_samples = it.next().and_then(|v| v.parse().ok()),
            "--se-cooldown-secs" => se_cooldown_secs = it.next().and_then(|v| v.parse().ok()),
            "--se-min-obs-secs" => se_min_obs_secs = it.next().and_then(|v| v.parse().ok()),
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
            "--assets" => assets_arg = it.next(),
            "--round-sec" => {
                round_sec = it.next().and_then(|v| v.parse().ok()).unwrap_or(round_sec)
            }
            "--min-round-age" => {
                min_round_age = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(min_round_age)
            }
            "--min-time-left" => {
                min_time_left = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(min_time_left)
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
            "--max-positions" => {
                max_positions = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(max_positions)
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
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }
    Args {
        socket,
        mode,
        readonly,
        tick_ms,
        seed_balance,
        max_order_notional,
        min_shares,
        max_shares,
        markets,
        auto_exits,
        max_positions,
        engine,
        min_round_age,
        min_time_left,
        trend_confirm,
        trend_floor_ms,
        feed_ws,
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
        market_plugin,
        discovery,
        shadow_evolution,
        assets: assets_arg,
        se_min_samples,
        se_cooldown_secs,
        se_min_obs_secs,
        strategy_limits,
        enable_strategy,
        disable_strategy,
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
        slippage_ticks,
        latency_ms,
        fill_prob_bps,
    }
}

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
/// the global value there). Two shapes are accepted, both colon-separated:
///
/// * `name:max_open_positions:max_notional_usd` (P-1.1, unchanged)
/// * `name:max_open_positions:max_notional_usd:size_usd:min_shares:max_shares`
///   (E2-a: per-strategy sizing, clamped by the global risk band)
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
        if (parts.len() != 3 && parts.len() != 6) || parts[0].trim().is_empty() {
            eprintln!(
                "blitzkrieg-core: ignoring malformed --strategy-limit '{raw}' \
                 (want name:max_open:max_notional[:size_usd:min_shares:max_shares])"
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
        let (size_usd, min_shares, max_shares) = if parts.len() == 6 {
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
        out.insert(
            parts[0].trim().to_string(),
            blitzkrieg_core::service::StrategyLimit {
                max_open_positions,
                max_open_notional_usd,
                size_usd,
                min_shares,
                max_shares,
            },
        );
    }
    out
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let args = parse_args();

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

    let assets_override: Option<Vec<String>> = args.assets.as_ref().map(|s| {
        s.split(',')
            .map(|a| a.trim().to_uppercase())
            .filter(|a| !a.is_empty())
            .collect()
    });

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

    let config = CoreConfig {
        mode,
        default_maker_timeout_ms: 5000,
        risk: RiskConfig {
            max_order_notional: args.max_order_notional,
            ..Default::default()
        },
        dry_seed_balance: args.seed_balance,
        strategy_limits: parse_strategy_limits(&args.strategy_limits),
        enabled_strategies: args.enable_strategy,
        disabled_strategies: args.disable_strategy,
        markets: args.markets,
        auto_exits_enabled: args.auto_exits,
        engine_enabled: args.engine,
        min_shares,
        max_shares,
        near_miss_path,
        trade_log_path,
        order_log_path,
        position_log_path,
        discovery_enabled: args.discovery,
        shadow_evolution_enabled: args.shadow_evolution,
        shadow_evolution_tuning: if args.se_min_samples.is_some()
            || args.se_cooldown_secs.is_some()
            || args.se_min_obs_secs.is_some()
        {
            Some(blitzkrieg_core::service::ShadowEvolutionTuning {
                min_sample_count: args.se_min_samples,
                cooldown_secs: args.se_cooldown_secs,
                min_observation_secs: args.se_min_obs_secs,
                ..Default::default()
            })
        } else {
            None
        },
        assets: assets_override
            .unwrap_or_else(|| vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()]),
        min_round_age_sec: args.min_round_age,
        trend_confirm_sec: args.trend_confirm,
        trend_window_floor_ms: args.trend_floor_ms,
        feed_ws_enabled: args.feed_ws,
        market_plugin: args.market_plugin,
        round_duration_sec: args.round_sec,
        positions: PositionConfig {
            max_positions: args.max_positions,
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
        },
        event_archive_path,
        event_archive_max_mb,
        event_archive_rotate_mb,
        event_archive_min_free_mb,
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
    if let Some(path) = &args.backtest {
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
        );
        return Ok(());
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

/// Offline event-driven backtest (P-1.2): replay a market-data archive through
/// the same core the live path runs. Always dry: the backtester forces dry mode,
/// starts no feeds and writes no trade/order/position logs.
fn run_backtest(
    archive: &str,
    report_path: Option<&str>,
    cfg: CoreConfig,
    tick_ms: i64,
    tail_ms: i64,
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
    let mut bt = EventBacktester::new(
        BacktestConfig {
            core: cfg,
            tick_ms,
            tail_ms,
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
                        if let Some(dir) = std::path::Path::new(p).parent() {
                            if !dir.as_os_str().is_empty() {
                                let _ = std::fs::create_dir_all(dir);
                            }
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
            rows.sort_by(|a, b| b.1.cmp(&a.1));
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
    let mut relaxed = ExitConfig::default();
    relaxed.min_time_left_sec = 0;
    relaxed.force_exit_sec = 0;
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
    if n > 0 { (wins * 100) / n } else { 0 }
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
    fn malformed_strategy_limits_are_dropped_not_half_applied() {
        for bad in [
            "spread_arb:0",           // too few segments
            "spread_arb:0:-:1:2:3:4", // too many
            ":0:-",                   // no name
            "spread_arb:x:-",         // bad position cap
            "spread_arb:0:abc",       // bad notional
            "spread_arb:0:-:abc:1:2", // bad size_usd
            "spread_arb:0:-:1:abc:2", // bad min_shares
            "spread_arb:0:-:1:2:abc", // bad max_shares
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
}
