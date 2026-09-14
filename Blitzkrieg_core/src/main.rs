//! blitzkrieg-core entrypoint.
//!
//! Serves the trading core over a Unix-domain-socket JSON-RPC 2.0 channel.
//! Node spawns this process and owns its lifecycle; credentials are read from
//! the environment inside the process (never supplied by Node).
//!
//! Usage:
//!   blitzkrieg-core --socket <path> [--mode dry|live] [--tick-ms 50]
//!                   [--seed-balance 10000] [--max-order-notional 100]
//!                   [--market-plugin <name>]
//!                   [--trade-log <path>] [--no-trade-log]
//!                   [--order-log <path>] [--no-order-log]
//!                   [--near-miss-path <path>]
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
    tick_ms: u64,
    seed_balance: Decimal,
    max_order_notional: Decimal,
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
    market_plugin: Option<String>,
    discovery: bool,
    shadow_evolution: bool,
    assets: Option<String>,
    se_min_samples: Option<u32>,
    se_cooldown_secs: Option<i64>,
    se_min_obs_secs: Option<i64>,
}

fn default_socket() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "clodds".into());
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/clodds-core-{user}.sock").replace("//", "/")
}

fn parse_args() -> Args {
    let mut socket = default_socket();
    let mut mode = Mode::Dry;
    let mut tick_ms = 50u64;
    let mut seed_balance = Decimal::from(10_000);
    let mut max_order_notional = Decimal::from(100);
    let mut markets: Vec<String> = Vec::new();
    let mut auto_exits = true;
    let mut max_positions: usize = 2;
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
    let mut market_plugin: Option<String> = None;
    let mut discovery = true;
    let mut shadow_evolution = false;
    let mut se_min_samples: Option<u32> = None;
    let mut se_cooldown_secs: Option<i64> = None;
    let mut se_min_obs_secs: Option<i64> = None;
    let mut assets_arg: Option<String> = None;

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
            "--tick-ms" => tick_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(tick_ms),
            "--seed-balance" => {
                seed_balance = it.next().and_then(|v| Decimal::from_str(&v).ok()).unwrap_or(seed_balance)
            }
            "--max-order-notional" => {
                max_order_notional =
                    it.next().and_then(|v| Decimal::from_str(&v).ok()).unwrap_or(max_order_notional)
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
            "--market-plugin" => market_plugin = it.next(),
            "--no-discovery" => discovery = false,
            "--shadow-evolution" => shadow_evolution = true,
            "--se-min-samples" => se_min_samples = it.next().and_then(|v| v.parse().ok()),
            "--se-cooldown-secs" => se_cooldown_secs = it.next().and_then(|v| v.parse().ok()),
            "--se-min-obs-secs" => se_min_obs_secs = it.next().and_then(|v| v.parse().ok()),
            "--assets" => assets_arg = it.next(),
            "--round-sec" => {
                round_sec = it.next().and_then(|v| v.parse().ok()).unwrap_or(round_sec)
            }
            "--min-round-age" => {
                min_round_age = it.next().and_then(|v| v.parse().ok()).unwrap_or(min_round_age)
            }
            "--min-time-left" => {
                min_time_left = it.next().and_then(|v| v.parse().ok()).unwrap_or(min_time_left)
            }
            "--trend-confirm-sec" => {
                trend_confirm = it.next().and_then(|v| v.parse().ok()).unwrap_or(trend_confirm)
            }
            "--trend-window-floor-ms" => {
                trend_floor_ms = it.next().and_then(|v| v.parse().ok()).unwrap_or(trend_floor_ms)
            }
            "--max-positions" => {
                max_positions = it.next().and_then(|v| v.parse().ok()).unwrap_or(max_positions)
            }
            "--market" => {
                if let Some(m) = it.next() {
                    markets.push(m);
                }
            }
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }
    Args { socket, mode, tick_ms, seed_balance, max_order_notional, markets, auto_exits, max_positions, engine, min_round_age, min_time_left, trend_confirm, trend_floor_ms, feed_ws, replay, replay_near_miss, round_sec, near_miss_path, trade_log, no_trade_log, order_log, no_order_log, market_plugin, discovery, shadow_evolution, assets: assets_arg, se_min_samples, se_cooldown_secs, se_min_obs_secs }
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
    } else if std::env::var("DRY_RUN").map(|v| v == "false").unwrap_or(false) {
        Mode::Live
    } else {
        Mode::Dry
    };

    // When the self-driving engine is on, record near-misses to disk by default
    // so the entry-gate question can be evaluated offline.
    let near_miss_path = if args.engine {
        args.near_miss_path.clone().or_else(|| Some("data/shadow/near-miss.jsonl".to_string()))
    } else {
        args.near_miss_path.clone()
    };

    // Trade log destination. Defaults to the relative `data/trades/trades.jsonl`;
    // `--trade-log <path>` overrides it (used by tests to keep synthetic trades out
    // of the production ledger) and `--no-trade-log` disables persistence entirely.
    let trade_log_path = if args.no_trade_log {
        None
    } else {
        Some(args.trade_log.clone().unwrap_or_else(|| "data/trades/trades.jsonl".to_string()))
    };

    // Order log: durable tracked-order storage for crash recovery + orphan sweep.
    let order_log_path = if args.no_order_log {
        None
    } else {
        Some(args.order_log.clone().unwrap_or_else(|| "data/orders/orders.jsonl".to_string()))
    };

    let assets_override: Option<Vec<String>> = args
        .assets
        .as_ref()
        .map(|s| s.split(',').map(|a| a.trim().to_uppercase()).filter(|a| !a.is_empty()).collect());

    let config = CoreConfig {
        mode,
        default_maker_timeout_ms: 5000,
        risk: RiskConfig { max_order_notional: args.max_order_notional, ..Default::default() },
        dry_seed_balance: args.seed_balance,
        markets: args.markets,
        auto_exits_enabled: args.auto_exits,
        engine_enabled: args.engine,
        near_miss_path,
        trade_log_path,
        order_log_path,
        discovery_enabled: args.discovery,
        shadow_evolution_enabled: args.shadow_evolution,
        shadow_evolution_tuning: if args.se_min_samples.is_some() || args.se_cooldown_secs.is_some() || args.se_min_obs_secs.is_some() {
            Some(blitzkrieg_core::service::ShadowEvolutionTuning {
                min_sample_count: args.se_min_samples,
                cooldown_secs: args.se_cooldown_secs,
                min_observation_secs: args.se_min_obs_secs,
                ..Default::default()
            })
        } else { None },
        assets: assets_override.unwrap_or_else(|| vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()]),
        min_round_age_sec: args.min_round_age,
        trend_confirm_sec: args.trend_confirm,
        trend_window_floor_ms: args.trend_floor_ms,
        feed_ws_enabled: args.feed_ws,
        market_plugin: args.market_plugin,
        round_duration_sec: args.round_sec,
        positions: PositionConfig { max_positions: args.max_positions, exit: blitzkrieg_core::exit_policy::ExitConfig { min_time_left_sec: args.min_time_left, ..Default::default() }, ..Default::default() },
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

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    // Keep the sender alive for the process lifetime; shutdown is driven by
    // SIGTERM/SIGINT inside the server. The handle is available for an embedded
    // caller that wants programmatic shutdown.
    let _shutdown_tx = tx;

    server::run(args.socket.clone(), config, args.tick_ms, rx).await?;
    eprintln!("blitzkrieg-core stopped");
    Ok(())
}

/// Offline walk-forward over a shadow JSONL file, using the SAME exit policy the
/// live core trades with. Prints an in-sample grid and the out-of-sample result.
fn run_replay(path: &std::path::Path) {
    use blitzkrieg_core::exit_policy::ExitConfig;
    use blitzkrieg_core::shadow::{bucket_by_entry, default_grid, walk_forward_file};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    match walk_forward_file(path, &ExitConfig::default(), &default_grid(), 6) {
        Ok(r) => {
            println!("shadow replay: {}", path.display());
            println!("\nIn-sample grid (optimistic):");
            let mut rows = r.in_sample.clone();
            rows.sort_by(|a, b| b.1.cmp(&a.1));
            for (name, pnl) in &rows {
                println!("  {:<20} {:>8.2}", name, pnl);
            }
            println!("\nWalk-forward out-of-sample: {} records, PnL {:.2} (train from {})", r.oos_count, r.oos_pnl, r.min_train);
            if let Some((name, best)) = rows.first() {
                println!("In-sample best: {} {:.2} — compare with OOS to judge overfitting", name, best);
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
                println!("  {:<12} {:>4} {:>9} {:>9} {:>7}", "bucket", "n", "sum$", "med$", "win%");
                for b in &by_price {
                    if b.n > 0 {
                        println!("  {:<12} {:>4} {:>9.2} {:>9.3} {:>6.0}%", b.label, b.n, b.sum, b.median, b.win_rate);
                    }
                }
                println!("\nBy time-left at entry (timing-gate question):");
                println!("  {:<12} {:>4} {:>9} {:>9} {:>7}", "bucket", "n", "sum$", "med$", "win%");
                for b in &by_time {
                    if b.n > 0 {
                        println!("  {:<12} {:>4} {:>9.2} {:>9.3} {:>6.0}%", b.label, b.n, b.sum, b.median, b.win_rate);
                    }
                }
                println!("\nNOTE: shadow data only holds trades that ALREADY passed the entry gate;");
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
    println!("  blocked signals   : {} (timing {}, momentum {})", s_same.n, s_same.timing_n, s_same.momentum_n);
    println!("  notional tradable : ${:.0}", s_same.total_feeable_notional);
    println!();
    println!("  A) same exit gate (entry relaxed only):");
    println!("     PnL ${:.2}  avg ${:.3}  win {}%", s_same.total_pnl, s_same.avg_pnl, pct(s_same.wins, s_same.n));
    println!("  B) entry AND exit time gate relaxed   :");
    println!("     PnL ${:.2}  avg ${:.3}  win {}%", s_relaxed.total_pnl, s_relaxed.avg_pnl, pct(s_relaxed.wins, s_relaxed.n));
    println!();
    println!("  (A) is near-zero because blocked signals have tLeft < min_time_left and");
    println!("  are time-exited immediately — this is the entry/exit timing COUPLING.");
    println!("  (B) is the real test of relaxing the timing gate.");
    if s_relaxed.total_pnl > rust_decimal::Decimal::ZERO {
        println!("  => On this sample, relaxing the timing gate would have HELPED (B > 0),");
        println!("     but n={} is small and regime-dependent — gather more before acting.", s_relaxed.n);
    } else {
        println!("  => On this sample, blocked trades would have LOST even with relaxed timing (B <= 0):");
        println!("     the gate is doing its job; do not relax it on this evidence.");
    }
}

fn pct(wins: usize, n: usize) -> usize {
    if n > 0 { (wins * 100) / n } else { 0 }
}
