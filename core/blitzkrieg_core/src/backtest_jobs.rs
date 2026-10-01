//! #353: the in-kernel backtest job registry.
//!
//! The WebUI's 傻白甜 flow — 拉数据 → 配置 → 回测 → 看结果 — runs as jobs
//! inside the core process (tokio tasks), addressed over the SAME IPC socket
//! the gateway already proxies. No subprocess, no command construction, no
//! script surface: the WebUI never touches the local filesystem (the spec's
//! hard boundary) and never names executable code — it names datasets, assets
//! and strategy names, and the registry validates every one of them
//! fail-closed before anything spawns.
//!
//! Job kinds:
//! - `pull`     — one [`crate::onchain::pull_and_convert`] (per asset filter;
//!                an empty `assets` list is one unfiltered pull). Resumable and
//!                cached exactly like the #352 CLI.
//! - `backtest` — one replay (or a #351 latency ladder for `sweep`): open the
//!                dataset, drive [`EventBacktester`], file the report JSON
//!                next to the dataset so it survives a core restart (the job
//!                table itself is in-memory and dies with the process).
//!
//! Status is polled over IPC (`backtest.status`); progress flows into the
//! registry through the same [`BacktestConfig::progress`] callback the CLI
//! prints to stderr — one producer, two consumers.

use crate::backtest::{
    BacktestConfig, Backtester, EventBacktester, latency_verdict, mode_rung_model, mode_rungs,
};
use crate::data_source::open_replay_all;
use crate::onchain::{self, HttpFetcher, PullProgress, PullRequest};
use crate::service::CoreConfig;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Finished jobs kept before the oldest is evicted. A UI mashing the button
/// must not grow the registry without bound; the durable artifacts (datasets,
/// report sidecars) live on disk regardless.
const JOB_CAP: usize = 50;

/// The datasets root every IPC pull writes into and every IPC backtest.run is
/// confined to (the CLI keeps its own default; the IPC surface is the
/// WebUI-reachable one, so it pins the root).
pub const DEFAULT_DATASET_DIR: &str = "data/onchain";

// ── wire types (the six methods' params) ────────────────────────────────────

/// `backtest.onchain.pull`: wallet + window + optional asset filters. An
/// empty `assets` list means "everything the wallet traded" (one pull);
/// named assets fan out to one resumable, cache-addressable pull each.
/// The output root is the kernel's dataset dir — not a caller parameter, so
/// the WebUI-reachable surface has no arbitrary-path write.
#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OnchainPullParams {
    pub wallet: String,
    /// `YYYY-MM-DD` (UTC, end inclusive) or epoch seconds.
    pub start: String,
    pub end: String,
    #[serde(default)]
    pub assets: Vec<String>,
    pub market: Option<String>,
}

/// `backtest.run`: dataset + the #351 mode knobs. No scripts, no code —
/// strategy NAMES only, validated by the engine install (an unknown name is a
/// failed job with that message, never a shell). `archive` must be a dataset
/// under the kernel's dataset dir.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRunParams {
    /// A dataset produced by a pull (the `eventsPath` a pull/list returned).
    pub archive: String,
    /// `mine` (default) | `verify` | `sweep` — the #351 ladder contract.
    pub mode: Option<String>,
    pub verify_latency_ms: Option<i64>,
    /// Honest-friction floor: refused below 1 (fail-closed, CLI parity).
    pub slippage_ticks: Option<i64>,
    pub tick_ms: Option<i64>,
    pub tail_ms: Option<i64>,
    /// Enable exactly these strategies (names); default = the live config's
    /// enabled set.
    pub strategies: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobIdParams {
    pub id: String,
}

/// `backtest.onchain.list` takes no fields — the root is the kernel's.
#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OnchainListParams {}

// ── the registry ────────────────────────────────────────────────────────────

/// One job's pollable status. `camelCase` straight onto the wire.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    pub job_id: String,
    /// "pull" | "backtest".
    pub kind: String,
    /// "running" | "done" | "failed".
    pub state: String,
    /// Pull: the on-chain phase ("trades"/"markets"/"prices"/"convert"/"done").
    /// Backtest: "replay".
    pub phase: String,
    pub detail: String,
    /// Pull: fetched trades. Backtest: delivered events.
    pub progress: u64,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct JobRecord {
    view: JobView,
    /// Set when Done: the pull's dataset summary or the backtest's
    /// `{ report, ladder, verdict, reportPath }`.
    result: Option<Value>,
}

/// In-memory job table. Shared as `Arc` with the spawned tasks, which update
/// their own record through the `note_*`/`finish_*` helpers.
pub struct BacktestJobs {
    inner: Mutex<JobsInner>,
    /// The dataset root: where pulls write and the only tree `backtest.run`
    /// may open. Fixed at construction (production = [`DEFAULT_DATASET_DIR`],
    /// tests = a temp dir) and NOT a caller parameter — the WebUI-reachable
    /// surface has no arbitrary-path read or write.
    root: PathBuf,
}

#[derive(Debug, Default)]
struct JobsInner {
    next: u64,
    jobs: HashMap<String, JobRecord>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lexically normalize a path for the containment check: `./x` ≡ `x` and
/// `a/b/../c` ≡ `a/c` (ParentDir pops — a plain `starts_with` on unresolved
/// components would let `data/onchain/../../etc` pose as a dataset).
fn norm(p: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in Path::new(p).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The report sidecar written next to the dataset: the FULL result document
/// (report + ladder + verdict), so a core restart loses nothing durable.
pub fn report_path_for(archive: &str) -> String {
    let stem = archive.strip_suffix(".jsonl").unwrap_or(archive);
    format!("{stem}.report.json")
}

impl BacktestJobs {
    /// The production registry: dataset root = [`DEFAULT_DATASET_DIR`]
    /// (`data/onchain`), the same tree the #352 CLI pulls into — so a dataset
    /// pulled from either surface shows up in the other's list.
    pub fn new() -> Self {
        Self::with_root(PathBuf::from(DEFAULT_DATASET_DIR))
    }

    /// Tests only: pin the dataset root to a scratch dir.
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            inner: Mutex::new(JobsInner::default()),
            root,
        }
    }

    /// The dataset root for this registry.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Register a job, evicting the oldest finished one when at capacity.
    fn submit(&self, kind: &str) -> Result<String, String> {
        let mut g = self.inner.lock().unwrap();
        if g.jobs.len() >= JOB_CAP {
            let oldest = g
                .jobs
                .iter()
                .filter(|(_, r)| r.view.finished_at_ms.is_some())
                .min_by_key(|(_, r)| r.view.started_at_ms)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    g.jobs.remove(&k);
                }
                None => {
                    return Err(format!(
                        "job registry is full ({JOB_CAP} running) — wait for a job to finish"
                    ));
                }
            }
        }
        g.next += 1;
        let id = format!("bt-{}", g.next);
        g.jobs.insert(
            id.clone(),
            JobRecord {
                view: JobView {
                    job_id: id.clone(),
                    kind: kind.to_string(),
                    state: "running".into(),
                    phase: "queued".into(),
                    detail: String::new(),
                    progress: 0,
                    started_at_ms: now_ms(),
                    finished_at_ms: None,
                    error: None,
                },
                result: None,
            },
        );
        Ok(id)
    }

    fn update<F: FnOnce(&mut JobView)>(&self, id: &str, f: F) {
        let mut g = self.inner.lock().unwrap();
        if let Some(r) = g.jobs.get_mut(id) {
            f(&mut r.view);
        }
    }

    fn finish(&self, id: &str, result: Result<Value, String>) {
        let mut g = self.inner.lock().unwrap();
        if let Some(r) = g.jobs.get_mut(id) {
            r.view.finished_at_ms = Some(now_ms());
            match result {
                Ok(v) => {
                    r.view.state = "done".into();
                    r.view.phase = "done".into();
                    r.result = Some(v);
                }
                Err(e) => {
                    r.view.state = "failed".into();
                    r.view.error = Some(e);
                }
            }
        }
    }

    pub(crate) fn note_pull(&self, id: &str, p: &PullProgress) {
        self.update(id, |v| {
            v.phase = p.phase.to_string();
            v.detail = p.detail.clone();
            v.progress = p.fetched;
        });
    }

    pub(crate) fn note_replay(&self, id: &str, rung: i64, delivered: u64, clock_ms: i64) {
        self.update(id, |v| {
            v.phase = "replay".into();
            v.detail = format!("latency {rung} ms · virtual {clock_ms} ms");
            v.progress = delivered;
        });
    }

    /// Start one pull per asset filter (`assets: []` = one unfiltered pull).
    /// All validation happens here, synchronously — a bad wallet/window/
    /// asset is an immediate, actionable error, never a half-started job.
    pub fn start_pull(self: &Arc<Self>, p: OnchainPullParams) -> Result<Vec<String>, String> {
        if p.wallet.len() < 40 || !p.wallet.starts_with("0x") {
            return Err(format!("`{}` is not a 0x… wallet address", p.wallet));
        }
        let start_sec = onchain::parse_time_arg(&p.start).map_err(|e| format!("start: {e}"))?;
        let end_sec = onchain::parse_time_arg(&p.end).map_err(|e| format!("end: {e}"))?;
        if end_sec <= start_sec {
            return Err(format!("empty window [{} → {}]", p.start, p.end));
        }
        // A date end covers the whole day (CLI parity); an epoch end is the
        // instant itself.
        let end_is_date = p.end.trim().parse::<i64>().is_err();
        let end_ms = (if end_is_date {
            end_sec + 86_400 - 1
        } else {
            end_sec
        }) * 1000;
        let start_ms = start_sec * 1000;
        let out_dir = self.root.clone();
        for a in &p.assets {
            if a.trim().is_empty() {
                return Err("asset filter must not be empty (omit `assets` for everything)".into());
            }
        }
        let filters: Vec<Option<String>> = if p.assets.is_empty() {
            vec![None]
        } else {
            p.assets
                .iter()
                .map(|a| Some(a.trim().to_string()))
                .collect()
        };

        let mut ids = Vec::new();
        for asset in filters {
            let id = self.submit("pull")?;
            let req = PullRequest {
                wallet: p.wallet.clone(),
                start_ms,
                end_ms,
                asset: asset.clone(),
                market: p.market.clone(),
                out_dir: out_dir.clone(),
                page_limit: 500,
            };
            let jobs = Arc::clone(self);
            let id2 = id.clone();
            tokio::spawn(async move {
                let fetcher = HttpFetcher::new(150);
                let cb = |prog: &PullProgress| jobs.note_pull(&id2, prog);
                match onchain::pull_and_convert(&fetcher, &req, &cb).await {
                    Ok(out) => {
                        let label = asset.unwrap_or_default();
                        jobs.finish(
                            &id2,
                            Ok(json!({
                                "asset": label,
                                "trades": out.trades,
                                "events": out.events,
                                "cacheHit": out.cache_hit,
                                "tradesPath": out.trades_path.display().to_string(),
                                "eventsPath": out.events_path.display().to_string(),
                                "manifestPath": out.manifest_path.display().to_string(),
                            })),
                        );
                    }
                    Err(e) => jobs.finish(&id2, Err(e)),
                }
            });
            ids.push(id);
        }
        Ok(ids)
    }

    /// Start one backtest job (a single replay, or a ladder for `sweep`).
    /// `cfg` is the LIVE core's config snapshot — the replay reuses its
    /// strategy knobs; only the mode's latency dial and the requested strategy
    /// selection differ. Validation is synchronous and fail-closed.
    pub fn start_backtest(
        self: &Arc<Self>,
        cfg: &CoreConfig,
        p: BacktestRunParams,
    ) -> Result<String, String> {
        let mode = p.mode.as_deref().unwrap_or("mine").to_string();
        let verify = p.verify_latency_ms.unwrap_or(286);
        let slippage = p.slippage_ticks.unwrap_or(1);
        if slippage < 1 {
            return Err(format!(
                "backtest refuses zero/negative taker slippage ({slippage}): the honest-friction \
                 floor is 1 tick"
            ));
        }
        let tick_ms = p.tick_ms.unwrap_or(50).max(1);
        let base_tail = p.tail_ms.unwrap_or(0).max(0);
        let rungs = mode_rungs(&mode, verify)?;
        // The dataset root confines which files a backtest may open: the WebUI
        // asks for a dataset by the path a pull/list returned — anything
        // outside the root is refused before a task exists.
        let root = self.root.to_string_lossy().into_owned();
        let archive = norm(&p.archive);
        if !archive.starts_with(norm(&root)) {
            return Err(format!(
                "`{}` is not a dataset under {root} — pick one from the dataset list",
                p.archive
            ));
        }
        if !archive.is_file() {
            return Err(format!(
                "dataset {} not found — pull it first (拉数据 → 配置 → 回测)",
                p.archive
            ));
        }
        let archive = archive.to_string_lossy().into_owned();
        let base_model = {
            let mut m = cfg.fill_model;
            m.taker_slippage_ticks = slippage as u32;
            m
        };

        let id = self.submit("backtest")?;
        // A snapshot, not a borrow: the job outlives this call.
        let cfg = cfg.clone();
        let jobs = Arc::clone(self);
        let id_task = id.clone();
        let archive_sidecar = archive.clone();
        // The replay is a CPU-bound sync loop (the same reason the CLI runs it
        // on main): it goes on the BLOCKING pool, not an async worker — the
        // IPC server must keep answering the WebUI's ~1s status polls while a
        // sweep grinds through its rungs. Progress crosses back through the
        // registry (std Mutex, held only for the note). Construction and
        // consumption of the backtester both happen inside the closure, so
        // nothing non-Send ever crosses a thread. The `backtest.run` arm
        // answers at once with the job id; the task below only shepherds the
        // blocking run to completion.
        tokio::spawn(async move {
            let jobs_run = Arc::clone(&jobs);
            let id_run = id_task.clone();
            let outcome = match tokio::task::spawn_blocking(move || -> Result<Value, String> {
                let mut rows: Vec<Value> = Vec::new();
                let mut reports: Vec<(i64, crate::backtest::BacktestReport)> = Vec::new();
                for &rung in &rungs {
                    let model = mode_rung_model(&mode, base_model, rung)?;
                    let mut rcfg = cfg.clone();
                    rcfg.fill_model = model;
                    // The user explicitly asked for a backtest: the replay runs
                    // its engine even if the live core boots with it off.
                    rcfg.engine_enabled = true;
                    if let Some(ss) = &p.strategies {
                        rcfg.enabled_strategies = ss.clone();
                        rcfg.disabled_strategies = Vec::new();
                    }
                    // A leg submitted at the last event still needs the clock to
                    // reach its report_at: tail at least the full round trip.
                    let tail = base_tail.max(rung.saturating_mul(2) + 100);
                    let src = open_replay_all(&archive)?;
                    let j2 = Arc::clone(&jobs_run);
                    let i2 = id_run.clone();
                    let progress: Arc<dyn Fn(u64, i64) + Send + Sync> =
                        Arc::new(move |d, c| j2.note_replay(&i2, rung, d, c));
                    let mut bt = EventBacktester::new(
                        BacktestConfig {
                            core: rcfg,
                            tick_ms,
                            tail_ms: tail,
                            hot_params: Vec::new(),
                            progress: Some(progress),
                        },
                        Box::new(src),
                    );
                    let report = bt.run()?;
                    rows.push(json!({
                        "latencyMs": rung,
                        "netPnlUsd": report.trades.net_pnl_usd,
                        "closed": report.trades.closed,
                        "winRatePct": report.trades.win_rate_pct,
                        "profitFactor": report.trades.profit_factor,
                    }));
                    reports.push((rung, report));
                }
                let nets: Vec<Decimal> =
                    reports.iter().map(|(_, r)| r.trades.net_pnl_usd).collect();
                let verdict = latency_verdict(&nets);
                Ok(json!({
                    "mode": mode,
                    "archive": archive,
                    "verdict": verdict,
                    "ladder": rows,
                    // Rung 0 = the no-latency baseline the ladder is read against.
                    "report": serde_json::to_value(&reports[0].1).unwrap_or(Value::Null),
                    "reportPath": report_path_for(&archive),
                }))
            })
            .await
            {
                Ok(inner) => inner,
                Err(e) => Err(format!("backtest task crashed: {e}")),
            };
            // File the result next to the dataset (durable across restarts); a
            // failed write does not fail the job — the report is already in the
            // registry and exportable over IPC.
            if let Ok(v) = &outcome {
                let text = serde_json::to_string_pretty(v).unwrap_or_default();
                let _ = std::fs::write(report_path_for(&archive_sidecar), format!("{text}\n"));
            }
            jobs.finish(&id_task, outcome);
        });
        Ok(id)
    }

    /// Poll one job: its status view, plus the full result once Done.
    pub fn status(&self, id: &str) -> Option<(JobView, Option<Value>)> {
        let g = self.inner.lock().unwrap();
        g.jobs.get(id).map(|r| (r.view.clone(), r.result.clone()))
    }

    /// `backtest.result`: the full result document of a finished job.
    pub fn result(&self, id: &str) -> Result<Value, String> {
        let g = self.inner.lock().unwrap();
        match g.jobs.get(id) {
            None => Err(format!("unknown job {id}")),
            Some(r) if r.view.state == "done" => Ok(r.result.clone().unwrap_or(Value::Null)),
            Some(r) => Err(format!(
                "job {id} is {} ({} events so far) — poll backtest.status until it is done",
                r.view.state, r.view.progress
            )),
        }
    }

    /// `backtest.export`: the report as a named JSON document the WebUI
    /// downloads (the browser saves it — the WebUI itself never touches the
    /// filesystem).
    pub fn export(&self, id: &str) -> Result<Value, String> {
        let result = self.result(id)?;
        let archive = result
            .get("archive")
            .and_then(Value::as_str)
            .unwrap_or("backtest");
        let name = Path::new(archive)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("backtest");
        let stem = name.strip_suffix(".jsonl").unwrap_or(name);
        Ok(json!({
            "fileName": format!("{stem}.report.json"),
            "content": serde_json::to_string_pretty(&result).unwrap_or_default(),
        }))
    }

    /// `backtest.onchain.list`: every dataset in this registry's dataset root
    /// (CLI pulls included — they write the same manifest layout), oldest
    /// first. The root is NOT a caller parameter: the WebUI-reachable surface
    /// has no arbitrary-path read.
    pub fn list_datasets(&self) -> Result<Value, String> {
        Self::list_datasets_in(self.root.to_string_lossy().as_ref())
    }

    /// The actual scan, against an explicit root (tests use a temp dir).
    fn list_datasets_in(out_dir: &str) -> Result<Value, String> {
        let dir = Path::new(out_dir);
        let mut manifests: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|s| s.to_str())
                        .map(|s| s.ends_with(".manifest.json"))
                        .unwrap_or(false)
                })
                .collect(),
            Err(e) => {
                return Err(format!(
                    "read {}: {e} — pull a dataset first (先拉一次数据)",
                    dir.display()
                ));
            }
        };
        manifests.sort();
        let mut out = Vec::new();
        for mp in manifests {
            let name = mp.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let dataset = name.strip_suffix(".manifest.json").unwrap_or(name);
            let manifest: Value = match std::fs::read_to_string(&mp)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
            {
                Some(v) => v,
                None => continue, // half-written manifest: not a dataset yet
            };
            out.push(json!({
                "dataset": dataset,
                "eventsPath": dir.join(format!("{dataset}.jsonl")).display().to_string(),
                "manifest": manifest,
            }));
        }
        Ok(json!({ "outDir": out_dir, "datasets": out }))
    }
}

impl Default for BacktestJobs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch registry: dataset root = a fresh temp dir per test.
    fn scratch(tag: &str) -> (Arc<BacktestJobs>, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("bk-jobs-{tag}-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&dir).expect("create scratch root");
        (Arc::new(BacktestJobs::with_root(dir.clone())), dir)
    }

    fn run_params(archive: &str) -> BacktestRunParams {
        BacktestRunParams {
            archive: archive.into(),
            mode: Some("mine".into()),
            verify_latency_ms: None,
            slippage_ticks: Some(1),
            tick_ms: None,
            tail_ms: None,
            strategies: None,
        }
    }

    /// #353 反向验收：WebUI 面上没有任意路径读。`backtest.run` 只认数据集根
    /// 里的数据集 —— `..` 逃逸、绝对路径、根外文件全部拒绝，且拒绝发生在
    /// 任务创建之前（提交即失败，不存在半个任务）。`#[tokio::test]`：诚实
    /// 路径会 spawn 任务，需要 runtime。
    #[tokio::test]
    async fn backtest_run_refuses_everything_outside_the_dataset_root() {
        let (jobs, dir) = scratch("escape");
        // A real dataset inside the root, so the only thing being tested is
        // the containment check, not a missing file.
        let inside = dir.join("w_s_e.jsonl");
        std::fs::write(&inside, b"{}\n").unwrap();
        let inside_s = inside.to_str().unwrap().to_string();

        for archive in [
            // `..` normalization escape (a plain starts_with would admit this).
            format!("{}/w_s_e.jsonl/../../elsewhere.jsonl", dir.display()),
            // An absolute path outside the root.
            "/etc/passwd".to_string(),
            // A sibling tree that merely shares a prefix.
            format!("{}.bak", dir.display()),
        ] {
            let err = jobs
                .start_backtest(&crate::service::CoreConfig::default(), run_params(&archive))
                .expect_err(&format!("`{archive}` must be refused"));
            assert!(
                err.contains("not a dataset"),
                "the refusal names the boundary: {err}"
            );
        }
        // The honest path passes the boundary check (it then fails on the
        // malformed dataset INSIDE the job — not here).
        assert!(
            jobs.start_backtest(
                &crate::service::CoreConfig::default(),
                run_params(&inside_s)
            )
            .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The honest-friction floor holds on the IPC surface too: the CLI refuses
    /// zero slippage, so the WebUI must not become the way around it.
    #[test]
    fn backtest_run_refuses_zero_slippage() {
        let (jobs, dir) = scratch("slippage");
        let inside = dir.join("w_s_e.jsonl");
        std::fs::write(&inside, b"{}\n").unwrap();
        let mut p = run_params(inside.to_str().unwrap());
        p.slippage_ticks = Some(0);
        let err = jobs
            .start_backtest(&crate::service::CoreConfig::default(), p)
            .expect_err("zero slippage must die");
        assert!(
            err.contains("honest-friction"),
            "the refusal names the floor: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mode knob is the CLI's enum, not free text: an unknown mode is an
    /// immediate, actionable refusal (single job submitted; the message names
    /// the allowed set).
    #[test]
    fn backtest_run_refuses_an_unknown_mode() {
        let (jobs, dir) = scratch("mode");
        let inside = dir.join("w_s_e.jsonl");
        std::fs::write(&inside, b"{}\n").unwrap();
        let mut p = run_params(inside.to_str().unwrap());
        p.mode = Some("yolo".into());
        let err = jobs
            .start_backtest(&crate::service::CoreConfig::default(), p)
            .expect_err("an unknown mode must be refused");
        assert!(err.contains("mine|verify|sweep"), "names the set: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Poll/exit/report on a job id that never existed: a named, immediate
    /// error — never a fabricated status.
    #[test]
    fn unknown_job_ids_are_named_errors_not_fabrications() {
        let jobs = BacktestJobs::new();
        assert!(jobs.status("bt-9999").is_none());
        assert!(jobs.result("bt-9999").is_err());
        assert!(jobs.export("bt-9999").is_err());
    }

    /// `backtest.onchain.list` reports every COMPLETED dataset manifest in the
    /// root and skips half-written ones (a torn manifest is not a dataset);
    /// each entry carries the eventsPath `backtest.run` accepts.
    #[test]
    fn dataset_list_reports_complete_manifests_only() {
        let (jobs, dir) = scratch("list");
        // Complete manifest (parseable JSON).
        std::fs::write(
            dir.join("a_b_c.manifest.json"),
            br#"{"state":{"complete":true},"counts":{"trades":1,"events":2}}"#,
        )
        .unwrap();
        // Torn manifest (its last write died mid-JSON).
        std::fs::write(dir.join("d_e_f.manifest.json"), b"{\"state\": {").unwrap();
        // A dataset FILE without a manifest is not listed (a backtest cannot
        // verify it either).
        std::fs::write(dir.join("g_h_i.jsonl"), b"{}\n").unwrap();

        let doc = jobs.list_datasets().expect("the scan itself succeeds");
        let sets = doc["datasets"].as_array().unwrap();
        assert_eq!(sets.len(), 1, "only the complete manifest lists: {doc}");
        assert_eq!(sets[0]["dataset"], "a_b_c");
        assert!(
            sets[0]["eventsPath"]
                .as_str()
                .unwrap()
                .ends_with("a_b_c.jsonl"),
            "the entry names the file backtest.run opens: {doc}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `start_pull` validates at submission: a malformed wallet or an empty
    /// window never becomes a half-started job — the error is the WebUI's
    /// actionable message (傻白甜 contract). `#[tokio::test]`: the passing
    /// cases would spawn tasks.
    #[tokio::test]
    async fn pull_validation_is_immediate_and_actionable() {
        let (jobs, dir) = scratch("pull");
        let good_wallet = "0x0000000000000000000000000000000000000001";
        let base = OnchainPullParams {
            wallet: "0xabc".into(),
            start: "2026-01-01".into(),
            end: "2026-01-02".into(),
            assets: vec![],
            market: None,
        };
        let err = jobs.start_pull(base.clone()).expect_err("bad wallet");
        assert!(err.contains("0x"), "names the wallet shape: {err}");

        let mut flipped = base.clone();
        flipped.wallet = good_wallet.into();
        flipped.start = "2026-01-02".into();
        flipped.end = "2026-01-01".into();
        let err = jobs.start_pull(flipped).expect_err("empty window");
        assert!(err.contains("empty window"), "names the window: {err}");

        // A BLANK asset filter on an otherwise-valid request (the wallet must
        // be good here or the wallet check fires first and the filter check
        // never runs).
        let mut blank = base;
        blank.wallet = good_wallet.into();
        blank.assets = vec![" ".into()];
        let err = jobs.start_pull(blank).expect_err("blank asset filter");
        assert!(err.contains("asset"), "names the filter: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
