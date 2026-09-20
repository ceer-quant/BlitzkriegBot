//! MarketRegime evaluation (E16 / #98): turn an event archive into a labelled
//! regime set and measure how well the online `MarketRegime` state machine
//! reproduces the offline labels.
//!
//! Evaluation protocol (documented in `docs/MARKET_REGIME.md`):
//!
//!  - the archive's `top`/`book` events give each token a mid-price series;
//!  - the N most active tokens are evaluated independently, each on its own
//!    wall-clock window grid (`window_ms`, default 300 s) anchored at its
//!    first sample;
//!  - the **ground-truth label** of a window is the offline rule
//!    (`classify_window`) over that window's full sample list;
//!  - the **machine label** is the online `MarketRegime`'s state sampled at
//!    the window's end — an incremental ring-window estimator with a
//!    confirmation hysteresis, the same rule, so accuracy measures the
//!    online estimator, not a second opinion;
//!  - accuracy = agreement over all labelled windows of the run, plus
//!    per-ground-truth-class recall and the confusion pairs.
//!
//! This is an offline tool: it never builds a `Core`, never trades, and never
//! writes to the archive.

use crate::data_source::{DataSource, open_replay_all};
use crate::engine::DataEvent;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use strategy_logic::market_regime::{MarketRegime, MarketRegimeConfig, classify_window};

/// Knobs of one evaluation run.
#[derive(Debug, Clone)]
pub struct RegimeEvalArgs {
    /// Evaluate exactly this token instead of the most-active default.
    pub token: Option<String>,
    /// How many of the most active tokens to evaluate (default 3).
    pub max_tokens: usize,
    /// The labelling rule + machine configuration (shared).
    pub config: MarketRegimeConfig,
}

struct TokenRun {
    machine: MarketRegime,
    first_ms: Option<i64>,
    window_idx: i64,
    window_samples: Vec<(i64, Decimal)>,
    from_ms: i64,
    to_ms: i64,
    samples_total: u64,
    rows: Vec<Value>,
}

/// Mid price of one book/top event, `None` when the side is empty or stale.
fn mid_of(ev: &DataEvent) -> Option<(String, i64, Decimal)> {
    let two = Decimal::from(2);
    match ev {
        DataEvent::TopOfBook {
            token_id,
            best_bid: Some(bb),
            best_ask: Some(ba),
            now_ms,
        } => Some((token_id.clone(), *now_ms, (*bb + *ba) / two)),
        DataEvent::Book {
            token_id,
            bids,
            asks,
            now_ms,
        } => {
            // Best = most aggressive price on each side, whatever order the
            // archive stored the levels in.
            let bb = bids.iter().map(|(p, _)| p).max().copied()?;
            let ba = asks.iter().map(|(p, _)| p).min().copied()?;
            Some((token_id.clone(), *now_ms, (bb + ba) / two))
        }
        _ => None,
    }
}

/// Run the evaluation over `archive` (plus its rotated segments, like a
/// backtest replay does). Returns the report as JSON (also the markdown
/// renderer in [`render_markdown`]).
pub fn run_eval(archive: &Path, args: &RegimeEvalArgs) -> Result<Value, String> {
    let path_str = archive
        .to_str()
        .ok_or_else(|| "archive path is not valid UTF-8".to_string())?;
    // Pass 1: which tokens carry the most market-data events. Deterministic
    // tie-break by token id so two runs over the same corpus pick the same set.
    let mut src = open_replay_all(path_str)?;
    let mut counts: HashMap<String, u64> = HashMap::new();
    while let Some(te) = src.next_event() {
        let token = match &te.event {
            DataEvent::TopOfBook { token_id, .. } | DataEvent::Book { token_id, .. } => {
                token_id.clone()
            }
            _ => continue,
        };
        *counts.entry(token).or_default() += 1;
    }
    let targets: Vec<String> = match &args.token {
        Some(t) => vec![t.clone()],
        None => {
            let mut ranked: Vec<(String, u64)> = counts.into_iter().collect();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            ranked
                .into_iter()
                .take(args.max_tokens.max(1))
                .map(|(t, _)| t)
                .collect()
        }
    };
    if targets.is_empty() {
        return Err("archive carries no book/top events — nothing to evaluate".to_string());
    }

    // Pass 2: feed each token's mids to its own machine and cut windows.
    let mut src = open_replay_all(path_str)?;
    let mut runs: HashMap<String, TokenRun> = targets
        .iter()
        .map(|t| {
            (
                t.clone(),
                TokenRun {
                    machine: MarketRegime::new(args.config.clone()),
                    first_ms: None,
                    window_idx: 0,
                    window_samples: Vec::new(),
                    from_ms: 0,
                    to_ms: 0,
                    samples_total: 0,
                    rows: Vec::new(),
                },
            )
        })
        .collect();
    while let Some(te) = src.next_event() {
        let Some((token, now_ms, mid)) = mid_of(&te.event) else {
            continue;
        };
        let Some(run) = runs.get_mut(&token) else {
            continue;
        };
        run.samples_total += 1;
        if run.first_ms.is_none() {
            run.first_ms = Some(now_ms);
            run.from_ms = now_ms;
        }
        let first = run.first_ms.unwrap_or(now_ms);
        let idx = (now_ms - first) / args.config.window_ms.max(1);
        if idx > run.window_idx {
            // The boundary event belongs to the NEW window; the machine state
            // before it is the state at the old window's end.
            finalize_window(run, &args.config, run.window_idx);
            run.window_idx = idx;
        }
        run.to_ms = now_ms;
        run.window_samples.push((now_ms, mid));
        run.machine.on_price(now_ms, mid);
    }
    for run in runs.values_mut() {
        finalize_window(run, &args.config, run.window_idx);
    }

    // ── accuracy over the pooled labelled set ───────────────────────────────
    let mut agree = 0u64;
    let mut total = 0u64;
    let mut by_label: HashMap<String, [u64; 2]> = HashMap::new(); // [n, agree]
    let mut confusion: HashMap<String, u64> = HashMap::new();
    let mut window_rows: Vec<Value> = Vec::new();
    for (token, run) in &runs {
        for row in &run.rows {
            let mut row = row.clone();
            row["token"] = json!(token);
            let label = row["label"].as_str().unwrap_or("range").to_string();
            let machine = row["machine"].as_str().unwrap_or("range").to_string();
            window_rows.push(row);
            total += 1;
            let hit = label == machine;
            if hit {
                agree += 1;
            }
            let slot = by_label.entry(label.clone()).or_default();
            slot[0] += 1;
            if hit {
                slot[1] += 1;
            }
            *confusion.entry(format!("{label}->{machine}")).or_default() += 1;
        }
    }
    let accuracy_pct = if total > 0 {
        (agree as f64 / total as f64) * 100.0
    } else {
        0.0
    };
    let by_label_json = json!(
        by_label
            .into_iter()
            .map(|(label, [n, a])| (
                label,
                json!({
                    "windows": n,
                    "agree": a,
                    "recallPct": if n > 0 { (a as f64 / n as f64) * 100.0 } else { 0.0 },
                })
            ))
            .collect::<HashMap<String, Value>>()
    );
    window_rows.sort_by_key(|r| {
        (
            r["token"].as_str().unwrap_or("").to_string(),
            r["fromMs"].as_i64().unwrap_or(0),
        )
    });

    let mut token_summaries = Vec::new();
    for token in &targets {
        let run = &runs[token];
        let token_total = run.rows.len() as u64;
        let token_agree = run
            .rows
            .iter()
            .filter(|r| r["label"] == r["machine"])
            .count() as u64;
        token_summaries.push(json!({
            "token": token,
            "samples": run.samples_total,
            "windows": token_total,
            "fromMs": run.from_ms,
            "toMs": run.to_ms,
            "agree": token_agree,
            "accuracyPct": if token_total > 0 { (token_agree as f64 / token_total as f64) * 100.0 } else { 0.0 },
        }));
    }

    Ok(json!({
        "generatedAtMs": now_unix_ms(),
        "archive": path_str,
        "protocol": {
            "windowMs": args.config.window_ms,
            "trendNetTicks": args.config.trend_net_ticks.to_string(),
            "trendMinEfficiency": args.config.trend_min_efficiency.to_string(),
            "volatileMadTicks": args.config.volatile_mad_ticks.to_string(),
            "confirmations": args.config.confirmations,
            "note": "ground truth = offline classify_window over each full window; machine = online ring estimator with confirmation hysteresis; both share one rule",
        },
        "evaluated": token_summaries,
        "accuracy": {
            "windows": total,
            "agree": agree,
            "accuracyPct": accuracy_pct,
            "pass80": accuracy_pct >= 80.0,
            "byGtLabel": by_label_json,
            "confusion": confusion,
        },
        "windows": window_rows,
    }))
}

/// Cut the accumulated samples into one labelled window row and reset.
fn finalize_window(run: &mut TokenRun, config: &MarketRegimeConfig, idx: i64) {
    if run.window_samples.is_empty() {
        return;
    }
    let (label, stats) = classify_window(&run.window_samples, config);
    let from_ms = run.window_samples[0].0;
    let to_ms = run.window_samples[run.window_samples.len() - 1].0;
    run.rows.push(json!({
        "idx": idx,
        "fromMs": from_ms,
        "toMs": to_ms,
        "samples": stats.samples,
        "netTicks": stats.net_ticks.to_string(),
        "efficiency": stats.efficiency.to_string(),
        "madTicks": stats.mad_ticks.to_string(),
        "label": label.as_str(),
        "machine": run.machine.state().as_str(),
    }));
    run.window_samples.clear();
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Human-readable verdict for the operator (stdout + .md file).
pub fn render_markdown(report: &Value) -> String {
    let acc = &report["accuracy"];
    let mut md = String::new();
    md.push_str("# MarketRegime evaluation\n\n");
    md.push_str(&format!(
        "Archive: `{}` · windows {} · agree {} · **accuracy {:.2}%** (pass ≥80: {})\n\n",
        report["archive"].as_str().unwrap_or("?"),
        acc["windows"],
        acc["agree"],
        acc["accuracyPct"].as_f64().unwrap_or(0.0),
        acc["pass80"].as_bool().unwrap_or(false),
    ));
    md.push_str(
        "## Evaluated tokens\n\n| token | samples | windows | accuracy % |\n|---|---|---|---|\n",
    );
    if let Some(rows) = report["evaluated"].as_array() {
        for r in rows {
            md.push_str(&format!(
                "| `{}` | {} | {} | {:.2} |\n",
                r["token"].as_str().unwrap_or("?"),
                r["samples"],
                r["windows"],
                r["accuracyPct"].as_f64().unwrap_or(0.0),
            ));
        }
    }
    md.push_str("\n## Recall by ground-truth label\n\n| gt label | windows | agree | recall % |\n|---|---|---|---|\n");
    if let Some(obj) = acc["byGtLabel"].as_object() {
        for (label, v) in obj {
            md.push_str(&format!(
                "| {} | {} | {} | {:.2} |\n",
                label,
                v["windows"],
                v["agree"],
                v["recallPct"].as_f64().unwrap_or(0.0),
            ));
        }
    }
    md.push_str("\n## Confusion (gt → machine)\n\n");
    if let Some(obj) = acc["confusion"].as_object() {
        let mut pairs: Vec<(&String, &Value)> = obj.iter().collect();
        pairs.sort_by_key(|(k, _)| (*k).clone());
        for (pair, n) in pairs {
            md.push_str(&format!("- `{pair}`: {}\n", n.as_u64().unwrap_or(0)));
        }
    }
    md.push_str("\nData boundary: the labelled set is this archive only (the frozen corpus is 0.75 days — the 30-day premise of #97 does not hold), and dry-mode economics do not enter regime labels. Accuracy here is online-vs-offline agreement, not a live forecast.\n");
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::io::Write;

    /// A synthetic archive with three clean phases (rise → flat → ±2-tick
    /// chop) must be labelled exactly by the machine once its confirmation
    /// hysteresis settles.
    #[test]
    fn synthetic_archive_labels_clean_phases() {
        let dir = std::env::temp_dir().join(format!("bkregime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let t0 = 1_700_000_000_000i64;
        // 60 s of rise (1 tick/s), 60 s flat, 60 s of ±2-tick chop — all in
        // one 300 s window each is too little; use window 60 s so each phase
        // is three windows.
        for i in 0..60 {
            let line = json!({"at": t0 + i * 1_000, "k": "top", "t": "tok",
                "bb": (dec!(0.40) + Decimal::from(i) * strategy_logic::market_regime::PRICE_TICK).to_string(),
                "ba": (dec!(0.41) + Decimal::from(i) * strategy_logic::market_regime::PRICE_TICK).to_string()});
            writeln!(f, "{line}").unwrap();
        }
        for i in 0..60 {
            let line = json!({"at": t0 + 60_000 + i * 1_000, "k": "top", "t": "tok",
                "bb": "0.99", "ba": "1.00"});
            writeln!(f, "{line}").unwrap();
        }
        for i in 0..60 {
            let p = if i % 2 == 0 { dec!(0.40) } else { dec!(0.42) };
            let line = json!({"at": t0 + 120_000 + i * 1_000, "k": "top", "t": "tok",
                "bb": p.to_string(), "ba": (p + dec!(0.01)).to_string()});
            writeln!(f, "{line}").unwrap();
        }
        drop(f);
        let args = RegimeEvalArgs {
            token: Some("tok".into()),
            max_tokens: 1,
            config: MarketRegimeConfig {
                window_ms: 60_000,
                confirmations: 1,
                ..MarketRegimeConfig::default()
            },
        };
        let report = run_eval(&path, &args).unwrap();
        let windows = report["windows"].as_array().unwrap();
        assert_eq!(windows.len(), 3, "one window per phase: {report}");
        assert_eq!(windows[0]["label"], "trendUp");
        assert_eq!(windows[1]["label"], "range");
        assert_eq!(windows[2]["label"], "volatile");
        let acc = report["accuracy"]["accuracyPct"].as_f64().unwrap();
        assert!(acc >= 80.0, "clean phases must agree, got {acc}");
        assert_eq!(report["accuracy"]["pass80"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
