// #699 — the bin's offline subcommand surface, moved here verbatim from
// `main.rs` (blueprint compile / regime eval / backtest / replay + `pct`).
// Declared only from `main.rs`, so these stay private to the bin target.

use blitzkrieg_core::service::CoreConfig;
use rust_decimal::Decimal;

/// Offline MarketRegime evaluation (E16 / #98): label the archive's windows
/// with the offline rule and score the online state machine against those
/// labels. Builds no Core, never trades, never writes to the archive.
/// #361: compile one blueprint JSON document to a Lua strategy body. The
/// default output is `strategy.lua` beside the blueprint (the package layout
/// §6.4 expects); `--blueprint-out` moves it. Errors exit 2 with the node id
/// the compiler named — a refused blueprint must be fixable from the message
/// alone.
pub(crate) fn run_blueprint_compile(blueprint_path: &str, out_path: Option<&str>) {
    eprintln!("blitzkrieg-core: blueprint compile {blueprint_path}");
    let json = match std::fs::read_to_string(blueprint_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("blitzkrieg-core: --blueprint-compile {blueprint_path}: {e}");
            std::process::exit(2);
        }
    };
    match blitzkrieg_core::blueprint::compile(&json) {
        Ok(lua) => {
            let out = out_path.unwrap_or("strategy.lua");
            let out = if std::path::Path::new(out).is_absolute() {
                out.to_string()
            } else {
                std::path::Path::new(blueprint_path)
                    .parent()
                    .map(|dir| dir.join(out).display().to_string())
                    .unwrap_or_else(|| out.to_string())
            };
            if let Some(dir) = std::path::Path::new(&out).parent()
                && !dir.as_os_str().is_empty()
            {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(&out, &lua) {
                Ok(()) => {
                    println!("blueprint: wrote {out} ({} bytes)", lua.len());
                }
                Err(e) => {
                    eprintln!("blitzkrieg-core: --blueprint-out {out}: {e}");
                    std::process::exit(2);
                }
            }
        }
        Err(e) => {
            eprintln!("blitzkrieg-core: blueprint compile failed: {e}");
            std::process::exit(2);
        }
    }
}

pub(crate) fn run_regime_eval(
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

// #351: `mode_rung_model` / `mode_rungs` / `latency_verdict` moved into the
// lib (`backtest`) so the #353 IPC job registry composes the SAME contract
// the CLI does.
/// One replay: open the archive, drive the SAME core, print and (optionally)
/// write the report. The single-rung primitive every mode composes. Returns
/// the report so a mode runner can build its ladder table without a second
/// replay.
fn backtest_once(
    archive: &str,
    cfg: CoreConfig,
    tick_ms: i64,
    tail_ms: i64,
    knobs: &[(String, String, Decimal)],
    report_path: Option<&str>,
) -> blitzkrieg_core::backtest::BacktestReport {
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
    for (strategy, knob, value) in knobs {
        eprintln!("blitzkrieg-core: counterfactual {strategy}.{knob} = {value}");
    }
    let core_cfg = cfg.clone();
    let mut bt = EventBacktester::new(
        BacktestConfig {
            core: cfg,
            tick_ms,
            tail_ms,
            hot_params: knobs.to_vec(),
            progress: None,
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
            report
        }
        Err(e) => {
            // The reason goes to the structured channel (tracing writes to
            // stderr in production); the bare stream write stays a static
            // literal — kernel-derived text reaching the stream sink is the
            // exact shape CodeQL's cleartext-logging check flags here (#693's
            // fix, same family; the two-step format! variant still carried the
            // taint edge to the sink, alert 203).
            tracing::error!(reason = %e, "backtest failed");
            eprintln!(
                "blitzkrieg-core: backtest failed — the reason is in the error log line above (issue 699)"
            );
            std::process::exit(2);
        }
    }
}

/// Historic single-run path (no `--backtest-mode`): byte-identical CLI surface.
pub(crate) fn run_backtest(
    archive: &str,
    report_path: Option<&str>,
    cfg: CoreConfig,
    tick_ms: i64,
    tail_ms: i64,
    knobs: Vec<(String, String, Decimal)>,
) {
    let _ = backtest_once(archive, cfg, tick_ms, tail_ms, &knobs, report_path);
}

/// #351: the mode runner — `mine` (zero latency, friction as configured),
/// `verify` (one real-latency run), `sweep` (the latency ladder with the
/// verdict table). Latency is the ONLY dial the mode moves: slippage and the
/// fee schedule stay as configured, so the rungs are comparable and any
/// expectation change is attributable to latency alone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_mode_backtest(
    archive: &str,
    report_path: Option<&str>,
    mut cfg: CoreConfig,
    tick_ms: i64,
    tail_ms: i64,
    knobs: Vec<(String, String, Decimal)>,
    mode: &str,
    verify_latency_ms: i64,
) {
    use rust_decimal::Decimal;
    let base_model = cfg.fill_model;
    let rungs = match blitzkrieg_core::backtest::mode_rungs(mode, verify_latency_ms) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("blitzkrieg-core: {e}");
            std::process::exit(2);
        }
    };
    let mut rows: Vec<(i64, blitzkrieg_core::backtest::BacktestReport)> = Vec::new();
    for rung in rungs {
        let model = match blitzkrieg_core::backtest::mode_rung_model(mode, base_model, rung) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("blitzkrieg-core: {e}");
                std::process::exit(2);
            }
        };
        cfg.fill_model = model;
        // A leg submitted at the last event still needs the clock to reach
        // its report_at: give the tail at least the full round trip.
        let tail = tail_ms.max(rung.saturating_mul(2) + 100);
        let rp: Option<String> = report_path.map(|p| {
            if mode == "sweep" {
                let stem = p.strip_suffix(".json").unwrap_or(p);
                format!("{stem}-lat{rung}.json")
            } else {
                p.to_string()
            }
        });
        eprintln!("=== backtest-mode {mode} @ latency {rung} ms ===");
        let report = backtest_once(archive, cfg.clone(), tick_ms, tail, &knobs, rp.as_deref());
        rows.push((rung, report));
    }
    // The ladder table.
    let width = 10;
    print!("backtest-mode {mode} — latency ladder:\n  latency (ms) :");
    for (rung, _) in &rows {
        print!("{rung:>width$}");
    }
    print!("\n  net PnL ($)  :");
    for (_, r) in &rows {
        print!("{:>width$}", format!("{:.2}", r.trades.net_pnl_usd));
    }
    print!("\n  closed       :");
    for (_, r) in &rows {
        print!("{:>width$}", r.trades.closed);
    }
    print!("\n  win rate (%) :");
    for (_, r) in &rows {
        print!("{:>width$}", format!("{:.2}", r.trades.win_rate_pct));
    }
    print!("\n  profit factor:");
    for (_, r) in &rows {
        let pf = r
            .trades
            .profit_factor
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "n/a".into());
        print!("{:>width$}", pf);
    }
    println!();
    let nets: Vec<Decimal> = rows.iter().map(|(_, r)| r.trades.net_pnl_usd).collect();
    let verdict = blitzkrieg_core::backtest::latency_verdict(&nets);
    println!("verdict: {verdict}");
}

/// Offline walk-forward over a shadow JSONL file, using the SAME exit policy the
/// live core trades with. Prints an in-sample grid and the out-of-sample result.
pub(crate) fn run_replay(path: &std::path::Path) {
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
pub(crate) fn run_replay_near_miss(path: &std::path::Path) {
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

pub(crate) fn pct(wins: usize, n: usize) -> usize {
    (wins * 100).checked_div(n).unwrap_or(0)
}
