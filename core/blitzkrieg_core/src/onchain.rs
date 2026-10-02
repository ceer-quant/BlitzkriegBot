//! #352 — on-chain data pull + conversion: Polymarket → backtest JSONL.
//!
//! The frozen corpus covers a few windows; a strategy's behaviour under the
//! REAL market distribution needs a wallet's actual history. Polymarket puts
//! every fill on chain and publishes it through three read APIs:
//!
//! * Data API  `data-api.polymarket.com/trades?user=<addr>` — the wallet's
//!   fills, newest first, offset-paged (`limit` ≤ 500).
//! * Gamma API `gamma-api.polymarket.com/markets?condition_ids=<id>` — market
//!   metadata; `clobTokenIds[0]`=Up / `[1]`=Down, `endDate` = round close.
//! * CLOB API  `clob.polymarket.com/prices-history?market=<token>&startTs=&endTs=&fidelity=1`
//!   — the published historical price series (~1-min points). No public L2
//!   history exists, so the honest book reconstruction is a top-of-book print
//!   at each point (`k=top`, nominal 1.0 depth — the same convention
//!   [`crate::marketdata::LocalBook::update_top`] uses for unknown sizes).
//!   No depth is invented: nothing in a replay of this stream can mint
//!   fillability that the venue did not publish.
//!
//! Layout under `data/onchain/`:
//!
//! ```text
//! <wallet>_<start>_<end>.trades.jsonl    # canonical raw fills (deduped, sorted)
//! <wallet>_<start>_<end>.jsonl           # converted event stream (--backtest input)
//! <wallet>_<start>_<end>.manifest.json   # range/counts/state + SHA256 of both files
//! .cache/conditions/<conditionId>.json   # Gamma metadata (immutable once closed)
//! .cache/prices/<token>_<start>_<end>.json
//! ```
//!
//! Resumability: the pull appends pages to a `.raw.jsonl` side file and
//! records `state.tradesOffset` in the manifest after every page. An
//! interrupted pull resumes from that offset; overlapping or shifted pages
//! are harmless because the finalize step dedupes by canonical fill key.
//! Cache: a completed manifest whose SHA256 verifies answers the same
//! request instantly (`cache_hit`), no network.
//!
//! Everything here is `async` and reports through a progress callback per
//! page/phase — the pull is never a blocking loop without feedback (the
//! #352 reverse gate), and the IPC layer (#353) wraps the same functions.

use crate::engine::DataEvent;
use crate::model::CryptoMarket;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

pub const DATA_API_BASE: &str = "https://data-api.polymarket.com";
pub const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";
pub const CLOB_API_BASE: &str = "https://clob.polymarket.com";
/// Runaway guard on total fills appended across every segment: 100k is far
/// past any single-wallet window we replay; past it the pull refuses rather
/// than spin forever against a paging bug.
pub const MAX_FILLS: i64 = 100_000;
/// The venue's own `offset` ceiling on `/trades` (probed live 2026-10-02:
/// `offset=10000` still answers, `offset=10500` errors with "max historical
/// trades offset of 10000 exceeded"). A segment never requests past it —
/// when a full page lands at the ceiling, the walk recurses on the window's
/// older half (`start → min_ts`), where offsets restart from 0.
pub const TRADES_OFFSET_CAP: i64 = 10_000;

// ── fetcher ─────────────────────────────────────────────────────────────────

/// Object-safe JSON GET. The indirection exists so the pull logic is tested
/// against a routing mock — the tests pin behaviour, never the network.
pub trait JsonFetcher: Send + Sync {
    fn get_json<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;
}

/// The real fetcher: rustls reqwest, polite inter-request delay, bounded
/// retries on 429/5xx (these APIs rate-limit; a pull of a big wallet is a
/// marathon, not a sprint).
pub struct HttpFetcher {
    client: reqwest::Client,
    delay_ms: u64,
    seq: AtomicU64,
}

impl HttpFetcher {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            client: reqwest::Client::new(),
            delay_ms,
            seq: AtomicU64::new(0),
        }
    }
}

impl JsonFetcher for HttpFetcher {
    fn get_json<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
        Box::pin(async move {
            let n = self.seq.fetch_add(1, Ordering::Relaxed);
            if n > 0 && self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            let mut last = String::new();
            for attempt in 0..3 {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1000 << attempt)).await;
                }
                let resp = match self.client.get(url).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        last = format!("GET {url}: {e}");
                        continue;
                    }
                };
                let status = resp.status();
                let body = resp.text().await.map_err(|e| format!("read {url}: {e}"))?;
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    last = format!("GET {url}: HTTP {status}");
                    continue;
                }
                if !status.is_success() {
                    let head: String = body.chars().take(200).collect();
                    return Err(format!("GET {url}: HTTP {status}: {head}"));
                }
                return serde_json::from_str(&body).map_err(|e| format!("decode {url}: {e}"));
            }
            Err(last)
        })
    }
}

// ── request / progress / outcome ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PullRequest {
    /// 0x… wallet address (the Data API's `user`).
    pub wallet: String,
    /// Inclusive window, MILLISECONDS.
    pub start_ms: i64,
    pub end_ms: i64,
    /// `Some("BTC")` filters to one asset's updown markets (slug prefix).
    pub asset: Option<String>,
    /// `Some("15m")` filters to one round duration (slug duration).
    pub market: Option<String>,
    /// Output root; `data/onchain` in production.
    pub out_dir: PathBuf,
    pub page_limit: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullPhase {
    Trades,
    Markets,
    Prices,
    Convert,
    Done,
}

impl std::fmt::Display for PullPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PullPhase::Trades => "trades",
            PullPhase::Markets => "markets",
            PullPhase::Prices => "prices",
            PullPhase::Convert => "convert",
            PullPhase::Done => "done",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct PullProgress {
    pub phase: PullPhase,
    pub fetched: u64,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct PullOutcome {
    pub trades_path: PathBuf,
    pub events_path: PathBuf,
    pub manifest_path: PathBuf,
    pub trades: u64,
    pub events: u64,
    pub cache_hit: bool,
}

pub type ProgressCb<'a> = dyn Fn(&PullProgress) + Send + Sync + 'a;

fn report(cb: &ProgressCb<'_>, phase: PullPhase, fetched: u64, detail: String) {
    cb(&PullProgress {
        phase,
        fetched,
        detail,
    });
}

// ── paths / naming ──────────────────────────────────────────────────────────

fn stem(req: &PullRequest) -> String {
    // The filters are part of the request identity: an asset=BTC pull and an
    // unfiltered pull of the same window are different datasets, so they must
    // not answer each other's cache.
    let mut s = format!(
        "{}_{}_{}",
        req.wallet.trim_start_matches("0x").to_lowercase(),
        req.start_ms / 1000,
        req.end_ms / 1000
    );
    if let Some(a) = &req.asset {
        s.push_str(&format!("-{}", a.to_lowercase()));
    }
    if let Some(m) = &req.market {
        s.push_str(&format!("-{}", m.to_lowercase()));
    }
    s
}

pub fn trades_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.trades.jsonl", stem(req)))
}
pub fn events_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.jsonl", stem(req)))
}
pub fn manifest_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.manifest.json", stem(req)))
}
fn raw_path(req: &PullRequest) -> PathBuf {
    req.out_dir.join(format!("{}.raw.jsonl", stem(req)))
}
fn condition_cache_path(out_dir: &Path, condition_id: &str) -> PathBuf {
    out_dir
        .join(".cache")
        .join("conditions")
        .join(format!("{condition_id}.json"))
}
fn price_cache_path(out_dir: &Path, token: &str, start_sec: i64, end_sec: i64) -> PathBuf {
    out_dir
        .join(".cache")
        .join("prices")
        .join(format!("{token}_{start_sec}_{end_sec}.json"))
}

// ── sha256 ──────────────────────────────────────────────────────────────────

/// Hex SHA256 of a file's bytes. The manifest carries one per dataset; a
/// file that no longer hashes to its pin is corruption, detected on read
/// (fail-closed, the same contract as the frozen corpus).
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    let mut out = String::new();
    for b in h.finalize() {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

/// Verify a dataset against its manifest. A manifest without sha256 entries
/// is itself a refusal — the digest is the corruption detector, and a pull
/// that skipped it would be an unverifiable dataset.
pub fn verify_dataset(manifest: &Path) -> Result<(), String> {
    let v: Value = read_json(manifest)?;
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let pins = [
        ("trades", trades_file_from_manifest(&v, manifest)),
        ("events", events_file_from_manifest(&v, manifest)),
    ];
    for (name, path) in pins {
        let path = path?;
        let pin = v
            .pointer(&format!("/sha256/{name}"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!(
                    "manifest {} has no sha256 for {name}: an un-hashed dataset is not a dataset",
                    manifest.display()
                )
            })?;
        let actual = sha256_file(&path).map_err(|e| format!("{name}: {e}"))?;
        if actual != pin {
            return Err(format!(
                "{name} sha256 mismatch: file {actual} != pinned {pin} — the dataset at {} is corrupted",
                dir.display()
            ));
        }
    }
    Ok(())
}

fn trades_file_from_manifest(v: &Value, manifest: &Path) -> Result<PathBuf, String> {
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let name = v
        .pointer("/files/trades")
        .and_then(Value::as_str)
        .ok_or_else(|| "manifest missing files.trades".to_string())?;
    Ok(dir.join(name))
}
fn events_file_from_manifest(v: &Value, manifest: &Path) -> Result<PathBuf, String> {
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let name = v
        .pointer("/files/events")
        .and_then(Value::as_str)
        .ok_or_else(|| "manifest missing files.events".to_string())?;
    Ok(dir.join(name))
}

fn read_json(path: &Path) -> Result<Value, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Atomic-ish JSON write: temp file + rename, so a crash never leaves a
/// half-written manifest (the resume state is the one thing that must not
/// corrupt).
fn write_json_atomic(path: &Path, v: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, s).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

fn append_line(path: &Path, line: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("append {}: {e}", path.display()))
}

// ── slug / time ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct RoundSlug {
    pub asset: String,
    pub duration_sec: i64,
    pub start_sec: i64,
}

/// Parse an updown round slug: `btc-updown-15m-1790784900` →
/// asset BTC, 900 s round starting at that epoch second. Anything that does
/// not match the shape (non-updown markets a wallet also touched) is None —
/// the converter skips it and counts it.
pub fn parse_round_slug(slug: &str) -> Option<RoundSlug> {
    let parts: Vec<&str> = slug.split('-').collect();
    if parts.len() < 4 || parts[1] != "updown" {
        return None;
    }
    let asset = parts[0].to_uppercase();
    let dur = parts[parts.len() - 2];
    let epoch = parts[parts.len() - 1];
    let mult = match dur.chars().last()? {
        'm' => 60,
        'h' => 3600,
        _ => return None,
    };
    let n: i64 = dur[..dur.len() - 1].parse().ok()?;
    let start_sec: i64 = epoch.parse().ok()?;
    Some(RoundSlug {
        asset,
        duration_sec: n * mult,
        start_sec,
    })
}

/// `YYYY-MM-DD` (UTC) or bare epoch seconds → epoch seconds. The CLI-facing
/// parser; the library only deals in epochs.
pub fn parse_time_arg(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("expected YYYY-MM-DD or epoch seconds, got `{s}`"));
    }
    let y: i64 = parts[0].parse().map_err(|_| format!("bad year in `{s}`"))?;
    let m: i64 = parts[1]
        .parse()
        .map_err(|_| format!("bad month in `{s}`"))?;
    let d: i64 = parts[2].parse().map_err(|_| format!("bad day in `{s}`"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(format!("bad date `{s}`"));
    }
    // Howard Hinnant's days_from_civil — UTC, no leap-second fantasies.
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86_400)
}

// ── canonical trades ────────────────────────────────────────────────────────

/// One fill's dedupe key: the venue may shift pages between requests or a
/// crash may double-append a page — the identity of a fill is its content,
/// not its position.
fn fill_key(row: &Value) -> String {
    let s = |p: &str| row.get(p).and_then(Value::as_str).unwrap_or("");
    let n = |p: &str| row.get(p).map(|v| v.to_string()).unwrap_or_default();
    format!(
        "{}|{}|{}|{}|{}|{}",
        s("transactionHash"),
        s("asset"),
        s("side"),
        n("price"),
        n("size"),
        row.get("timestamp").and_then(Value::as_i64).unwrap_or(0)
    )
}

/// Read every appended raw line, dedupe by fill key, sort by (timestamp,
/// key) and write the canonical trades file. Returns the row count.
fn finalize_trades(raw: &Path, out: &Path) -> Result<u64, String> {
    let mut seen = std::collections::HashSet::new();
    let mut rows: BTreeMap<(i64, String), Value> = BTreeMap::new();
    let content =
        std::fs::read_to_string(raw).map_err(|e| format!("read {}: {e}", raw.display()))?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // a torn tail line from a crash — the resume state owns the truth
        };
        let key = fill_key(&v);
        if !seen.insert(key.clone()) {
            continue;
        }
        let ts = v.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
        rows.insert((ts, key), v);
    }
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(out).map_err(|e| format!("create {}: {e}", out.display()))?,
    );
    let n = rows.len() as u64;
    for (_, v) in rows {
        writeln!(f, "{v}").map_err(|e| format!("write {}: {e}", out.display()))?;
    }
    f.flush()
        .map_err(|e| format!("flush {}: {e}", out.display()))?;
    Ok(n)
}

// ── the pull itself ─────────────────────────────────────────────────────────

/// Pull a wallet's fills for the window, convert to the backtest event
/// stream, write the manifest. Resumable and cached (see module docs).
pub async fn pull_and_convert(
    fetch: &dyn JsonFetcher,
    req: &PullRequest,
    progress: &ProgressCb<'_>,
) -> Result<PullOutcome, String> {
    if req.wallet.len() < 40 || !req.wallet.starts_with("0x") {
        return Err(format!("`{}` is not a 0x… wallet address", req.wallet));
    }
    if req.end_ms <= req.start_ms {
        return Err(format!("empty window [{}, {}]", req.start_ms, req.end_ms));
    }

    let tp = trades_path(req);
    let ep = events_path(req);
    let mp = manifest_path(req);

    // Cache: a completed, hash-verified manifest answers the same request
    // with zero network — the "second pull is instant" gate.
    if mp.exists() {
        let v: Value = read_json(&mp)?;
        let complete = v
            .pointer("/state/complete")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if complete && tp.exists() && ep.exists() {
            verify_dataset(&mp)?;
            let trades = v
                .pointer("/counts/trades")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let events = v
                .pointer("/counts/events")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            report(
                progress,
                PullPhase::Done,
                trades,
                "cache hit (sha256 verified)".into(),
            );
            return Ok(PullOutcome {
                trades_path: tp,
                events_path: ep,
                manifest_path: mp,
                trades,
                events,
                cache_hit: true,
            });
        }
    }

    // ── Trades ────────────────────────────────────────────────────────────
    let rp = raw_path(req);
    // The walk restarts from the window start on every call — a stored
    // offset would lie (the venue's filtered list can shift between
    // processes), and every re-served page lands as a duplicate the
    // fill-key dedupe removes at finalize. The expensive phases (gamma,
    // prices) resume through their per-item caches instead.
    let mut offset: i64 = 0;
    let mut fetched: u64 = 0;
    let start_sec = req.start_ms / 1000;
    let end_sec = req.end_ms / 1000;
    report(
        progress,
        PullPhase::Trades,
        0,
        format!("window [{start_sec}, {end_sec}]"),
    );
    // The walk: server-side time filter (`start`/`end`, both inclusive —
    // probed live 2026-10-02: the pair returns exactly that window, rows
    // newest-first), offset-paged. The venue caps the OFFSET ITSELF at
    // TRADES_OFFSET_CAP (10000 answers, 10500 errors with "max historical
    // trades offset of 10000 exceeded"), so when the walk has run past the
    // cap it reacts by page shape: a FULL page means there is likely more
    // depth below — split the window at the oldest in-window timestamp
    // seen and restart at offset 0 on `[start, min_ts]`; a SHORT page is
    // the exhaustion signal (a frozen historical list cannot grow at its
    // tail) — done, because the usual empty-page confirmation would die on
    // the venue's offset error instead of answering empty. Below the cap a
    // short page still gets its confirmation request. Boundary rows
    // re-served across a split land as duplicates the fill-key dedupe
    // removes at finalize; overlaps are always harmless.
    let (seg_start, mut seg_end) = (start_sec, end_sec);
    loop {
        let url = format!(
            "{DATA_API_BASE}/trades?user={}&limit={}&offset={offset}&start={seg_start}&end={seg_end}",
            req.wallet, req.page_limit
        );
        let page = fetch
            .get_json(&url)
            .await
            .map_err(|e| format!("trades offset {offset}: {e}"))?;
        let rows = page
            .as_array()
            .ok_or_else(|| format!("trades offset {offset}: expected an array"))?;
        if rows.is_empty() {
            break;
        }
        let mut min_ts = i64::MAX;
        for row in rows {
            let ts = row.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
            // The server already filtered; this check is the belt-and-braces
            // against a venue quirk leaking out-of-window rows into the corpus
            // — and a leaked row must not steer the split point either.
            if ts > seg_end || ts < seg_start {
                continue;
            }
            min_ts = min_ts.min(ts);
            let line = row.to_string();
            append_line(&rp, &line)?;
            fetched += 1;
        }
        offset += rows.len() as i64;
        // Persist the resume state after EVERY page: a crash loses nothing
        // that the dedupe cannot absorb on the restart.
        write_json_atomic(
            &mp,
            &json!({
                "wallet": req.wallet,
                "startMs": req.start_ms, "endMs": req.end_ms,
                "asset": req.asset, "market": req.market,
                "state": { "tradesOffset": offset, "fetched": fetched, "complete": false },
            }),
        )?;
        report(
            progress,
            PullPhase::Trades,
            fetched,
            format!("offset {offset} (+{})", rows.len()),
        );
        if fetched as i64 > MAX_FILLS {
            return Err(format!(
                "the window holds more than {MAX_FILLS} fills — narrow the time window"
            ));
        }
        // The venue refuses any request past TRADES_OFFSET_CAP. Once the
        // offset has run past it: a SHORT page is exhaustion — the usual
        // empty-page confirmation would die on the venue's offset error
        // instead of answering empty — and a FULL page means un-fetched
        // depth below, so the window splits at the oldest in-window
        // timestamp seen and the segment restarts at offset 0. The guard
        // keeps the split honest: no time progress would mean an infinite
        // walk.
        let full = rows.len() as i64 == req.page_limit;
        if offset > TRADES_OFFSET_CAP {
            if !full {
                break;
            }
            if min_ts >= seg_end {
                return Err(format!(
                    "trades walk stalled at the offset cap: segment [{seg_start}, {seg_end}] \
                     oldest ts {min_ts} makes no time progress"
                ));
            }
            seg_end = min_ts;
            offset = 0;
        }
    }

    // ── Finalize: canonical, deduped, sorted raw ─────────────────────────
    let trades_n = finalize_trades(&rp, &tp)?;
    let _ = std::fs::remove_file(&rp);

    // ── Markets + Prices (per condition, both cached) ─────────────────────
    let content =
        std::fs::read_to_string(&tp).map_err(|e| format!("read {}: {e}", tp.display()))?;
    let mut conditions: Vec<String> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let v: Value =
            serde_json::from_str(line).map_err(|e| format!("canonical trades re-parse: {e}"))?;
        let cid = v
            .get("conditionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !cid.is_empty() && !conditions.contains(&cid) {
            conditions.push(cid.clone());
        }
        rows.push(v);
    }
    report(
        progress,
        PullPhase::Markets,
        0,
        format!("{} conditions", conditions.len()),
    );
    let mut gamma: BTreeMap<String, Value> = BTreeMap::new();
    let mut no_meta: u64 = 0;
    for cid in &conditions {
        let cp = condition_cache_path(&req.out_dir, cid);
        let cached = if cp.exists() {
            Some(read_json(&cp)?)
        } else {
            None
        };
        // A cached `null` is a MISS, not a fact: Gamma prunes closed
        // short-cycle updown markets (it answered `[]` for every September
        // 2026 condition — probed live), and the CLOB fallback below recovers
        // them, so a re-pull self-heals instead of replaying the hole.
        let v = match cached {
            Some(v) if !v.is_null() && v.get("clobTokenIds").is_some() => v,
            _ => {
                let url = format!("{GAMMA_API_BASE}/markets?condition_ids={cid}");
                let resp = fetch
                    .get_json(&url)
                    .await
                    .map_err(|e| format!("gamma {cid}: {e}"))?;
                let arr = resp.as_array().cloned().unwrap_or_default();
                let mut v = arr.into_iter().next().unwrap_or(Value::Null);
                // Gamma-prune fallback: CLOB `/markets/{condition_id}` keeps
                // the full record for closed markets (tokens with outcome
                // Up/Down, question_id, neg_risk). Adapt it into the gamma
                // shape the converter consumes — `clobTokenIds` as the
                // JSON-encoded string array, Up first, Down second.
                if v.is_null() || v.get("clobTokenIds").is_none() {
                    let url = format!("{CLOB_API_BASE}/markets/{cid}");
                    match fetch.get_json(&url).await {
                        Ok(c) => {
                            let toks = c
                                .get("tokens")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            let token_of = |t: &Value| {
                                t.get("token_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string()
                            };
                            let mut up = String::new();
                            let mut down = String::new();
                            for t in &toks {
                                match t.get("outcome").and_then(Value::as_str) {
                                    Some("Up") if up.is_empty() => up = token_of(t),
                                    Some("Down") if down.is_empty() => down = token_of(t),
                                    _ => {}
                                }
                            }
                            if up.is_empty() {
                                up = toks.first().map(&token_of).unwrap_or_default();
                            }
                            if down.is_empty() {
                                down = toks.get(1).map(token_of).unwrap_or_default();
                            }
                            if !up.is_empty() || !down.is_empty() {
                                v = json!({
                                    "clobTokenIds": serde_json::to_string(&[up, down])
                                        .unwrap_or_default(),
                                    "questionID": c
                                        .get("question_id")
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                    "negRisk": c.get("neg_risk").cloned().unwrap_or(json!(false)),
                                    "closed": c.get("closed").cloned().unwrap_or(json!(true)),
                                    "source": "clob-fallback",
                                });
                            }
                        }
                        Err(e) => {
                            return Err(format!("clob {cid}: {e}"));
                        }
                    }
                }
                write_json_atomic(&cp, &v)?;
                if v.is_null() {
                    no_meta += 1;
                }
                v
            }
        };
        gamma.insert(cid.clone(), v);
        report(
            progress,
            PullPhase::Markets,
            gamma.len() as u64,
            cid.clone(),
        );
    }
    // Fail-closed against a systemic metadata outage: scattered dead
    // conditions (a wallet also touched non-round markets) are fine — the
    // converter skips them — but if MOST of the window has no record the
    // event stream would be a half-blind fiction. Refuse before converting;
    // per-condition caches persist, so a re-run when the APIs answer is
    // cheap.
    if no_meta * 2 > conditions.len() as u64 {
        return Err(format!(
            "metadata coverage collapsed: {no_meta} of {} conditions have no \
             gamma/clob record — refusing to convert a half-blind stream",
            conditions.len()
        ));
    }

    // Per condition: slug → timing; gamma → token ids; prices-history → the
    // published price series (the honest top-of-book reconstruction).
    let mut price_series: BTreeMap<(String, i64, i64), Vec<(i64, Decimal)>> = BTreeMap::new();
    for cid in &conditions {
        let g = gamma.get(cid).cloned().unwrap_or(Value::Null);
        let slug = rows
            .iter()
            .find(|r| r.get("conditionId").and_then(Value::as_str) == Some(cid.as_str()))
            .and_then(|r| r.get("slug"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(rs) = parse_round_slug(slug) else {
            continue;
        };
        let end_sec = rs.start_sec + rs.duration_sec;
        let tokens = gamma_tokens(&g);
        for token in tokens {
            if token.is_empty() {
                continue;
            }
            let key = (token.clone(), rs.start_sec, end_sec);
            if price_series.contains_key(&key) {
                continue;
            }
            let pp = price_cache_path(&req.out_dir, &token, rs.start_sec, end_sec);
            let hist = if pp.exists() {
                read_json(&pp)?
            } else {
                let url = format!(
                    "{CLOB_API_BASE}/prices-history?market={token}&startTs={}&endTs={}&fidelity=1",
                    rs.start_sec, end_sec
                );
                let v = fetch
                    .get_json(&url)
                    .await
                    .map_err(|e| format!("prices {token}: {e}"))?;
                write_json_atomic(&pp, &v)?;
                v
            };
            let mut pts = Vec::new();
            for p in hist
                .pointer("/history")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                let t = p.get("t").and_then(Value::as_i64).unwrap_or(0);
                let raw = p.get("p").map(|x| x.to_string()).unwrap_or_default();
                if let Ok(d) = Decimal::from_str_exact(raw.trim_matches('"')) {
                    pts.push((t, d));
                }
            }
            price_series.insert(key, pts);
            report(
                progress,
                PullPhase::Prices,
                price_series.len() as u64,
                token,
            );
        }
    }

    // ── Convert ───────────────────────────────────────────────────────────
    let (events, skipped) = convert(&rows, &gamma, &price_series, req)?;
    write_events(&ep, &events)?;
    report(
        progress,
        PullPhase::Convert,
        events.len() as u64,
        String::new(),
    );

    let trades_sha = sha256_file(&tp)?;
    let events_sha = sha256_file(&ep)?;
    let manifest = json!({
        "wallet": req.wallet,
        "startMs": req.start_ms, "endMs": req.end_ms,
        "asset": req.asset, "market": req.market,
        "pulledAtMs": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "files": {
            "trades": tp.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            "events": ep.file_name().and_then(|s| s.to_str()).unwrap_or(""),
        },
        "state": { "tradesOffset": offset, "fetched": fetched, "complete": true },
        "counts": { "trades": trades_n, "conditions": conditions.len(), "events": events.len(), "skippedRows": skipped },
        "sha256": { "trades": trades_sha, "events": events_sha },
    });
    write_json_atomic(&mp, &manifest)?;
    report(progress, PullPhase::Done, trades_n, String::new());

    Ok(PullOutcome {
        trades_path: tp,
        events_path: ep,
        manifest_path: mp,
        trades: trades_n,
        events: events.len() as u64,
        cache_hit: false,
    })
}

/// Gamma's `clobTokenIds` (a JSON-encoded string array) + `outcomes` order:
/// index 0 = Up, 1 = Down.
fn gamma_tokens(g: &Value) -> Vec<String> {
    let raw = g
        .get("clobTokenIds")
        .and_then(Value::as_str)
        .unwrap_or("[]");
    let ids: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
    ids
}

/// The deterministic converter: canonical fills + gamma metadata + price
/// series → the archive event stream. Events are sorted by (at, rank) with
/// round(0) < top(1) < trade(2) < round_end(3), so the same inputs always
/// produce byte-identical output (the manifest's sha256 is a real
/// fingerprint). Returns (events, skipped_rows).
/// One token's published price series over one round window: `(t_sec, price)`
/// points, keyed by `(token, start_sec, end_sec)`.
type PriceSeries = BTreeMap<(String, i64, i64), Vec<(i64, Decimal)>>;

fn convert(
    rows: &[Value],
    gamma: &BTreeMap<String, Value>,
    price_series: &PriceSeries,
    req: &PullRequest,
) -> Result<(Vec<DataEvent>, u64), String> {
    use crate::data_source::event_to_json;

    // Condition → (slug, traded tokens) for the rounds we will emit.
    let mut by_condition: BTreeMap<&str, &Value> = BTreeMap::new();
    let mut skipped: u64 = 0;
    for r in rows {
        let cid = r.get("conditionId").and_then(Value::as_str).unwrap_or("");
        if cid.is_empty() {
            skipped += 1;
            continue;
        }
        by_condition.entry(cid).or_insert(r);
    }

    struct RoundCtx {
        start_sec: i64,
        end_sec: i64,
        markets: Vec<CryptoMarket>,
    }
    let mut rounds: BTreeMap<i64, RoundCtx> = BTreeMap::new();

    for (cid, sample) in &by_condition {
        let g = gamma.get(*cid).cloned().unwrap_or(Value::Null);
        let slug = sample
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(rs) = parse_round_slug(slug) else {
            skipped += 1;
            continue;
        };
        // Filters: asset / market duration, when the request asked for one.
        if let Some(a) = &req.asset
            && !a.eq_ignore_ascii_case(&rs.asset)
        {
            skipped += 1;
            continue;
        }
        let dur_label = match rs.duration_sec {
            300 => "5m",
            900 => "15m",
            3600 => "1h",
            14_400 => "4h",
            _ => "",
        };
        if let Some(m) = &req.market
            && !m.eq_ignore_ascii_case(dur_label)
        {
            skipped += 1;
            continue;
        }
        let end_sec = rs.start_sec + rs.duration_sec;
        let tokens = gamma_tokens(&g);
        let up_token = tokens.first().cloned().unwrap_or_default();
        let down_token = tokens.get(1).cloned().unwrap_or_default();
        // Gamma's clobTokenIds order is authoritative ([0]=Up, [1]=Down); the
        // traded fill's outcomeIndex only disambiguates when gamma returned
        // nothing usable (a market gamma has since pruned).
        let outcome_index = sample
            .get("outcomeIndex")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let traded_token = sample
            .get("asset")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (up, down) = if up_token.is_empty() && down_token.is_empty() {
            match outcome_index {
                0 => (traded_token.to_string(), String::new()),
                1 => (String::new(), traded_token.to_string()),
                _ => (up_token, down_token),
            }
        } else {
            (up_token, down_token)
        };
        // Seed the declaration prices with each token's first published point
        // (0.5 when the series is empty — an honest "unknown mid").
        let seed = |t: &str| {
            price_series
                .get(&(t.to_string(), rs.start_sec, end_sec))
                .and_then(|v| v.first())
                .map(|(_, d)| *d)
                .unwrap_or(Decimal::new(5, 1))
        };
        let market = CryptoMarket {
            asset: rs.asset.clone(),
            condition_id: (*cid).to_string(),
            question_id: g
                .get("questionID")
                .and_then(Value::as_str)
                .unwrap_or(*cid)
                .to_string(),
            up_token_id: up.clone(),
            down_token_id: down.clone(),
            up_price: seed(&up),
            down_price: seed(&down),
            expires_at_ms: end_sec * 1000,
            // #354 (5.4): the kernel's slot is an INDEX on the round grid
            // (scanner `current_slot` = now_ms/1000/duration; a manual close's
            // expiry derives `(slot+1) * round_duration_sec`), not an epoch.
            // The converter used to write the round's absolute start second,
            // which a 5m round under any grid arithmetic silently mis-times —
            // the slot index is start/duration by definition.
            round_slot: rs.start_sec / rs.duration_sec,
            // #354 (5.4): the slug's declared duration travels with the market,
            // so a replay can refuse a corpus whose round cadence disagrees
            // with the configured grid (5m data on a 15m assumption ages every
            // round 3× too slowly) instead of silently mis-timing it.
            round_duration_sec: rs.duration_sec,
            neg_risk: g.get("negRisk").and_then(Value::as_bool).unwrap_or(false),
            question: sample
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        };
        rounds.entry(rs.start_sec).or_insert(RoundCtx {
            start_sec: rs.start_sec,
            end_sec,
            markets: Vec::new(),
        });
        if let Some(rc) = rounds.get_mut(&rs.start_sec)
            && !rc.markets.iter().any(|m| m.condition_id == *cid)
        {
            rc.markets.push(market);
        }
    }

    // rank: round < top < trade < round_end — the canonical same-instant order.
    let rank = |ev: &DataEvent| match ev {
        DataEvent::RoundMarkets { .. } => 0u8,
        DataEvent::TopOfBook { .. } => 1,
        DataEvent::Trade { .. } => 2,
        DataEvent::RoundEnd { .. } => 3,
        _ => 4,
    };
    let mut events: Vec<DataEvent> = Vec::new();
    for rc in rounds.values() {
        events.push(DataEvent::RoundMarkets {
            markets: rc.markets.clone(),
            now_ms: rc.start_sec * 1000,
        });
        for m in &rc.markets {
            for t in [&m.up_token_id, &m.down_token_id] {
                if t.is_empty() {
                    continue;
                }
                if let Some(pts) = price_series.get(&(t.clone(), rc.start_sec, rc.end_sec)) {
                    for (tsec, p) in pts {
                        events.push(DataEvent::TopOfBook {
                            token_id: t.clone(),
                            best_bid: Some(*p),
                            best_ask: Some(*p),
                            now_ms: tsec * 1000,
                        });
                    }
                }
            }
        }
    }
    for r in rows {
        let cid = r.get("conditionId").and_then(Value::as_str).unwrap_or("");
        let Some(sample) = by_condition.get(cid) else {
            continue;
        };
        let slug = sample.get("slug").and_then(Value::as_str).unwrap_or("");
        let Some(rs) = parse_round_slug(slug) else {
            continue;
        };
        // Filters again: a skipped condition's fills are skipped too.
        if let Some(a) = &req.asset
            && !a.eq_ignore_ascii_case(&rs.asset)
        {
            continue;
        }
        let dur_label = match rs.duration_sec {
            300 => "5m",
            900 => "15m",
            3600 => "1h",
            14_400 => "4h",
            _ => "",
        };
        if let Some(m) = &req.market
            && !m.eq_ignore_ascii_case(dur_label)
        {
            continue;
        }
        let dec = |p: &str| -> Option<Decimal> {
            let raw = r.get(p)?.to_string();
            Decimal::from_str_exact(raw.trim_matches('"')).ok()
        };
        let (Some(price), Some(size)) = (dec("price"), dec("size")) else {
            skipped += 1;
            continue;
        };
        let side = match r.get("side").and_then(Value::as_str).unwrap_or("") {
            "BUY" => crate::model::Side::Buy,
            "SELL" => crate::model::Side::Sell,
            _ => {
                skipped += 1;
                continue;
            }
        };
        events.push(DataEvent::Trade {
            token_id: r
                .get("asset")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            side,
            price,
            size,
            now_ms: r.get("timestamp").and_then(Value::as_i64).unwrap_or(0) * 1000,
        });
    }
    for rc in rounds.values() {
        events.push(DataEvent::RoundEnd {
            now_ms: rc.end_sec * 1000,
        });
    }

    // Deterministic order: (at, rank, canonical line). Keys are computed once
    // — serializing per comparison would be O(n log n) JSON round-trips.
    let mut keyed: Vec<(i64, u8, String, DataEvent)> = events
        .into_iter()
        .map(|ev| {
            let at = crate::data_source::event_at_ms(&ev);
            let r = rank(&ev);
            let line = event_to_json(&ev).to_string();
            (at, r, line, ev)
        })
        .collect();
    keyed.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    let events: Vec<DataEvent> = keyed.into_iter().map(|(_, _, _, ev)| ev).collect();
    Ok((events, skipped))
}

/// Write the event stream: one canonical JSON line per event, sorted (the
/// sort already happened in [`convert`]).
fn write_events(path: &Path, events: &[DataEvent]) -> Result<(), String> {
    use crate::data_source::event_to_json;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?,
    );
    for ev in events {
        let line = event_to_json(ev).to_string();
        writeln!(f, "{line}").map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    f.flush()
        .map_err(|e| format!("flush {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomOrd};

    const WALLET: &str = "0x3725d52f3c252e8374999cc8617292ea2608ad88";
    const COND1: &str = "0x404b2d3ae09e641646f062f6dc08d4ac3910fe6d582b71d8940866a37edb18c7";
    const COND2: &str = "0x20d2ba1b27dd8657c4a04247967ade89f4f8dfc49d56d07086e7e230a7ad7b19";
    const UP1: &str = "UP1";
    const DOWN1: &str = "DOWN1";
    const UP2: &str = "UP2";
    const DOWN2: &str = "DOWN2";
    /// Two rounds inside one window: a 15m and a 5m, one fill each.
    const R1_START: i64 = 1_790_784_900; // btc-updown-15m-1790784900
    const R2_START: i64 = 1_790_785_200; // btc-updown-5m-1790785200

    #[allow(clippy::too_many_arguments)]
    fn trade_row(
        cid: &str,
        asset: &str,
        side: &str,
        price: f64,
        size: f64,
        ts: i64,
        slug: &str,
        oi: i64,
    ) -> Value {
        json!({
            "proxyWallet": WALLET, "side": side, "asset": asset,
            "conditionId": cid, "size": size, "price": price, "timestamp": ts,
            "title": "Bitcoin Up or Down", "slug": slug,
            "outcome": if oi == 0 { "Up" } else { "Down" },
            "outcomeIndex": oi, "transactionHash": format!("0x{ts:x}-{oi}"),
        })
    }

    fn rows_ab() -> Vec<Value> {
        vec![
            trade_row(
                COND1,
                UP1,
                "BUY",
                0.93,
                47.36,
                R1_START + 100,
                "btc-updown-15m-1790784900",
                0,
            ),
            trade_row(
                COND2,
                DOWN2,
                "SELL",
                0.42,
                10.0,
                R2_START + 100,
                "btc-updown-5m-1790785200",
                1,
            ),
        ]
    }

    fn gamma_map() -> BTreeMap<String, Value> {
        let g = |up: &str, down: &str| {
            json!({
                "clobTokenIds": serde_json::to_string(&[up, down]).unwrap(),
                "questionID": "q", "negRisk": false, "closed": true,
            })
        };
        BTreeMap::from([
            (COND1.to_string(), g(UP1, DOWN1)),
            (COND2.to_string(), g(UP2, DOWN2)),
        ])
    }

    /// A scripted fetcher: trades pages served in call order (offset-blind,
    /// which is exactly the page-shift hazard the dedupe exists for), gamma
    /// and prices-history answered per URL.
    struct Scripted {
        trades_pages: Vec<Vec<Value>>,
        trades_calls: AtomicUsize,
        gammas: BTreeMap<String, Value>,
        fail_second_trades: bool,
        calls: AtomicUsize,
    }

    impl Scripted {
        fn new(
            trades_pages: Vec<Vec<Value>>,
            gammas: BTreeMap<String, Value>,
            fail_second_trades: bool,
        ) -> Self {
            Self {
                trades_pages,
                trades_calls: AtomicUsize::new(0),
                gammas,
                fail_second_trades,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl JsonFetcher for Scripted {
        fn get_json<'a>(
            &'a self,
            url: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
            self.calls.fetch_add(1, AtomOrd::SeqCst);
            let out: Result<Value, String> = if url.contains("/trades?") {
                let i = self.trades_calls.fetch_add(1, AtomOrd::SeqCst);
                if self.fail_second_trades && i >= 1 {
                    Err("network dead after page 0".into())
                } else {
                    Ok(Value::Array(
                        self.trades_pages.get(i).cloned().unwrap_or_default(),
                    ))
                }
            } else if url.contains("gamma-api") {
                let cid = url
                    .split("condition_ids=")
                    .nth(1)
                    .unwrap_or("")
                    .split('&')
                    .next()
                    .unwrap_or("");
                Ok(json!([self
                    .gammas
                    .get(cid)
                    .cloned()
                    .unwrap_or(Value::Null)]))
            } else if url.contains("prices-history") {
                let start: i64 = url
                    .split("startTs=")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                Ok(json!({ "history": [
                    { "t": start + 60, "p": 0.4 },
                    { "t": start + 120, "p": 0.6 },
                ]}))
            } else {
                Err(format!("unexpected url {url}"))
            };
            Box::pin(async move { out })
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bk-onchain-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn req(out: PathBuf) -> PullRequest {
        PullRequest {
            wallet: WALLET.into(),
            start_ms: 1_790_784_000_000,
            end_ms: 1_790_786_000_000,
            asset: None,
            market: None,
            out_dir: out,
            page_limit: 500,
        }
    }

    fn noop_cb() -> impl Fn(&PullProgress) + Send + Sync {
        |_: &PullProgress| {}
    }

    /// 2 rounds → 2 round + 8 top + 2 trade + 2 round_end events.
    const EXPECTED_EVENTS: u64 = 14;

    #[tokio::test]
    async fn pull_converts_to_a_complete_event_stream() {
        let fetch = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
        let r = req(tmp("happy"));
        let seen: std::sync::Arc<Mutex<Vec<(PullPhase, u64)>>> = std::sync::Arc::default();
        let seen2 = seen.clone();
        let cb = move |p: &PullProgress| seen2.lock().unwrap().push((p.phase, p.fetched));
        let o = pull_and_convert(&fetch, &r, &cb).await.unwrap();

        assert_eq!(o.trades, 2);
        assert_eq!(o.events, EXPECTED_EVENTS);
        assert!(!o.cache_hit);

        // The four event kinds the issue names, all present.
        let txt = std::fs::read_to_string(&o.events_path).unwrap();
        let count = |k: &str| txt.matches(&format!("\"k\":\"{k}\"")).count();
        assert_eq!(count("round"), 2, "round_start declarations: {txt}");
        assert_eq!(count("top"), 8, "published price points as top-of-book");
        assert_eq!(count("trade"), 2);
        assert_eq!(count("round_end"), 2);

        // The manifest's digests verify.
        verify_dataset(&o.manifest_path).unwrap();

        // Reverse gate: the pull reported per phase AND per page — a silent
        // blocking loop would leave `seen` without Trades progress points.
        let phases = seen.lock().unwrap();
        for want in [
            PullPhase::Trades,
            PullPhase::Markets,
            PullPhase::Prices,
            PullPhase::Convert,
            PullPhase::Done,
        ] {
            assert!(
                phases.iter().any(|(p, _)| *p == want),
                "no {want} progress was reported"
            );
        }
        let max_fetched = phases
            .iter()
            .filter(|(p, _)| *p == PullPhase::Trades)
            .map(|(_, f)| *f)
            .max()
            .unwrap_or(0);
        assert!(
            max_fetched >= 2,
            "fetched counts must advance, got {max_fetched}"
        );
    }

    #[tokio::test]
    async fn a_completed_pull_answers_from_cache() {
        let fetch = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
        let r = req(tmp("cache"));
        let first = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
        assert!(!first.cache_hit);
        let calls_after_first = fetch.calls.load(AtomOrd::SeqCst);
        let second = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
        assert!(second.cache_hit, "a verified manifest must answer offline");
        assert_eq!(
            fetch.calls.load(AtomOrd::SeqCst),
            calls_after_first,
            "the cache hit must not touch the network"
        );
        assert_eq!(second.trades, first.trades);
    }

    #[tokio::test]
    async fn an_interrupted_pull_resumes_without_double_counting() {
        let r = req(tmp("resume"));
        // First attempt dies on the second trades page.
        let dead = Scripted::new(vec![rows_ab()], gamma_map(), true);
        let err = pull_and_convert(&dead, &r, &noop_cb()).await;
        assert!(
            err.is_err(),
            "the network death must surface, not be swallowed"
        );
        // The resume state survived the crash: offset after page 0, incomplete.
        let m: Value = read_json(&manifest_path(&r)).unwrap();
        assert_eq!(
            m.pointer("/state/tradesOffset").and_then(Value::as_i64),
            Some(2)
        );
        assert_eq!(
            m.pointer("/state/complete").and_then(Value::as_bool),
            Some(false)
        );
        // The venue re-serves page 0 (pages shifted between requests): the
        // dedupe must keep every fill exactly once.
        let alive = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
        let o = pull_and_convert(&alive, &r, &noop_cb()).await.unwrap();
        assert_eq!(o.trades, 2, "overlapping pages must dedupe to unique fills");
        verify_dataset(&o.manifest_path).unwrap();
    }

    /// A venue-faithful mock: the frozen history is filtered by the URL's
    /// own `start`/`end` and served `offset`/`limit`-paged (newest first) —
    /// the exact semantics probed live 2026-10-02. Unlike `Scripted` this
    /// mock is offset-AWARE, so a walk that pages wrongly loses or dupes
    /// rows, and any request past `TRADES_OFFSET_CAP` dies the way the real
    /// venue does ("max historical trades offset of 10000 exceeded").
    struct PagingVenue {
        all: Vec<Value>,
        trades_urls: Mutex<Vec<String>>,
        gammas: BTreeMap<String, Value>,
    }

    impl JsonFetcher for PagingVenue {
        fn get_json<'a>(
            &'a self,
            url: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
            let out: Result<Value, String> = if url.contains("/trades?") {
                self.trades_urls.lock().unwrap().push(url.to_string());
                let param = |key: &str| {
                    url.split(&format!("{key}="))
                        .nth(1)
                        .and_then(|s| s.split('&').next())
                        .and_then(|s| s.parse::<i64>().ok())
                };
                let window = (
                    param("limit"),
                    param("offset"),
                    param("start"),
                    param("end"),
                );
                if let (Some(limit), Some(offset), Some(start), Some(end)) = window {
                    if offset > TRADES_OFFSET_CAP {
                        Err("max historical trades offset of 10000 exceeded".into())
                    } else {
                        let rows: Vec<Value> = self
                            .all
                            .iter()
                            .filter(|r| {
                                let ts = r.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
                                ts >= start && ts <= end
                            })
                            .skip(offset as usize)
                            .take(limit as usize)
                            .cloned()
                            .collect();
                        Ok(Value::Array(rows))
                    }
                } else {
                    Err(format!("trades url without the window contract: {url}"))
                }
            } else if url.contains("gamma-api") {
                let cid = url
                    .split("condition_ids=")
                    .nth(1)
                    .unwrap_or("")
                    .split('&')
                    .next()
                    .unwrap_or("");
                Ok(json!([self
                    .gammas
                    .get(cid)
                    .cloned()
                    .unwrap_or(Value::Null)]))
            } else if url.contains("prices-history") {
                let start: i64 = url
                    .split("startTs=")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                Ok(json!({ "history": [
                    { "t": start + 60, "p": 0.4 },
                    { "t": start + 120, "p": 0.6 },
                ]}))
            } else {
                Err(format!("unexpected url {url}"))
            };
            Box::pin(async move { out })
        }
    }

    /// Acceptance (#355 prerequisite): a history longer than the venue's
    /// offset ceiling is walked COMPLETELY — the full-page-at-the-cap split
    /// resumes on the older prefix, the boundary row re-served by both
    /// segments lands exactly once (fill-key dedupe), and every request
    /// carried the server-side window. 10,503 fills × 500-page = 21 full
    /// pages: one more than the 10,000 ceiling allows, so the tail is
    /// reachable ONLY through the split.
    #[tokio::test]
    async fn a_full_history_walk_splits_at_the_venue_offset_cap() {
        const N: usize = 10_503;
        const T0: i64 = 1_790_785_000;
        let all: Vec<Value> = (0..N)
            .map(|i| {
                let ts = T0 - i as i64;
                let even = i % 2 == 0;
                json!({
                    "proxyWallet": WALLET, "side": "BUY",
                    "asset": if even { UP1 } else { DOWN2 },
                    "conditionId": if even { COND1 } else { COND2 },
                    "size": 1.0, "price": 0.5, "timestamp": ts,
                    "title": "t",
                    "slug": if even { "btc-updown-15m-1790784900" } else { "btc-updown-5m-1790785200" },
                    "outcome": "Up", "outcomeIndex": 0,
                    "transactionHash": format!("0x{i:x}"),
                })
            })
            .collect();
        let fetch = PagingVenue {
            all,
            trades_urls: Mutex::default(),
            gammas: gamma_map(),
        };
        let mut r = req(tmp("paging"));
        // Wide enough that every generated second sits inside the window.
        r.start_ms = (T0 - (N as i64 + 500)) * 1000;
        r.end_ms = (T0 + 10) * 1000;
        let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
        assert_eq!(
            o.trades, N as u64,
            "every fill captured exactly once despite the boundary re-serve"
        );
        let urls = fetch.trades_urls.lock().unwrap();
        assert!(
            urls.iter()
                .all(|u| u.contains("start=") && u.contains("end=")),
            "every trades request must carry the server-side window: {:?}",
            urls.first()
        );
        assert!(
            urls.len() > 21,
            "the walk crossed the offset cap only via the split: {} requests",
            urls.len()
        );
        verify_dataset(&o.manifest_path).unwrap();
    }

    /// Gamma prunes closed short-cycle updown markets (probed live: it
    /// answered `[]` for every September 2026 condition), and a pull made
    /// before the fallback existed cached those holes as literal `null`
    /// files. The gate: a `null` cache is a MISS — the CLOB fallback
    /// recovers the record, the round declarations carry both token ids,
    /// and the healed cache is written back.
    struct GammaPruned {
        clob_tokens: bool,
        trades_pages: Mutex<Vec<Vec<Value>>>,
    }

    impl JsonFetcher for GammaPruned {
        fn get_json<'a>(
            &'a self,
            url: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
            let out: Result<Value, String> = if url.contains("/trades?") {
                let mut pages = self.trades_pages.lock().unwrap();
                if pages.is_empty() {
                    Ok(Value::Array(Vec::new()))
                } else {
                    Ok(Value::Array(pages.remove(0)))
                }
            } else if url.contains("gamma-api") {
                Ok(json!([])) // pruned: the hole that started this
            } else if url.contains("/markets/0x") {
                let cid = url.rsplit('/').next().unwrap_or("");
                let tokens = if self.clob_tokens {
                    let (up, down) = if cid == COND1 {
                        (UP1, DOWN1)
                    } else {
                        (UP2, DOWN2)
                    };
                    json!([
                        { "token_id": up, "outcome": "Up", "price": 0, "winner": false },
                        { "token_id": down, "outcome": "Down", "price": 1, "winner": true },
                    ])
                } else {
                    json!([])
                };
                Ok(json!({
                    "condition_id": cid, "tokens": tokens,
                    "question_id": "q", "neg_risk": false, "closed": true,
                }))
            } else if url.contains("prices-history") {
                let start: i64 = url
                    .split("startTs=")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                Ok(json!({ "history": [
                    { "t": start + 60, "p": 0.4 },
                    { "t": start + 120, "p": 0.6 },
                ]}))
            } else {
                Err(format!("unexpected url {url}"))
            };
            Box::pin(async move { out })
        }
    }

    #[tokio::test]
    async fn a_gamma_pruned_condition_is_recovered_from_clob() {
        let fetch = GammaPruned {
            clob_tokens: true,
            trades_pages: Mutex::new(vec![rows_ab()]),
        };
        let r = req(tmp("pruned"));
        // The pre-fallback world: a literal `null` cached for COND1.
        let cp = condition_cache_path(&r.out_dir, COND1);
        std::fs::create_dir_all(cp.parent().unwrap()).unwrap();
        write_json_atomic(&cp, &Value::Null).unwrap();
        let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
        assert_eq!(o.trades, 2);
        assert_eq!(o.events, EXPECTED_EVENTS);
        // Both round declarations carry the CLOB-recovered token ids — the
        // converter saw a real record, not the hole.
        let txt = std::fs::read_to_string(&o.events_path).unwrap();
        assert!(
            txt.contains(UP1) && txt.contains(DOWN2),
            "token ids present"
        );
        // The healed cache is no longer a hole.
        let healed: Value = read_json(&cp).unwrap();
        assert!(
            healed.get("clobTokenIds").is_some(),
            "a re-pull must overwrite the null cache with the CLOB record"
        );
        verify_dataset(&o.manifest_path).unwrap();
    }

    /// Reverse gate: scattered dead conditions are fine, but when MOST of
    /// the window has no gamma/clob record the stream would be half-blind —
    /// refuse before converting instead of shipping a fiction.
    #[tokio::test]
    async fn a_metadata_coverage_collapse_refuses_to_convert() {
        let fetch = GammaPruned {
            clob_tokens: false, // CLOB answers too, but tokenless
            trades_pages: Mutex::new(vec![rows_ab()]),
        };
        let r = req(tmp("coverage"));
        let err = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap_err();
        assert!(
            err.contains("metadata coverage collapsed"),
            "want the collapse refusal, got {err}"
        );
    }

    /// Reverse gate: a manifest without sha256 pins is a REFUSAL, not a pass.
    #[tokio::test]
    async fn a_manifest_without_sha256_is_a_refusal() {
        let fetch = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
        let r = req(tmp("nosha"));
        let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
        let mut m: Value = read_json(&o.manifest_path).unwrap();
        m.as_object_mut().unwrap().remove("sha256");
        write_json_atomic(&o.manifest_path, &m).unwrap();
        let err = verify_dataset(&o.manifest_path).unwrap_err();
        assert!(err.contains("no sha256"), "{err}");
    }

    /// Reverse gate: a flipped byte is corruption, detected on read.
    #[tokio::test]
    async fn a_corrupted_dataset_is_detected() {
        let fetch = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
        let r = req(tmp("corrupt"));
        let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
        verify_dataset(&o.manifest_path).unwrap(); // sanity: fresh pull verifies
        let mut bytes = std::fs::read(&o.events_path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] = bytes[last].wrapping_add(1);
        std::fs::write(&o.events_path, &bytes).unwrap();
        let err = verify_dataset(&o.manifest_path).unwrap_err();
        assert!(err.contains("mismatch"), "{err}");
    }

    /// Acceptance: the converter's output is consumed by --backtest with ZERO
    /// malformed lines — the four kinds are first-class archive events.
    #[tokio::test]
    async fn the_event_stream_replays_without_malformed_lines() {
        use crate::data_source::DataSource;
        let fetch = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
        let r = req(tmp("replay"));
        let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
        // The CLI opens the dataset with open_replay_all (segment discovery),
        // so the acceptance must run through that path: the sibling
        // `<stem>.trades.jsonl` side file and the manifest sit in the same
        // directory and must not leak into the replay as phantom segments.
        let mut src = crate::data_source::open_replay_all(o.events_path.to_str().unwrap()).unwrap();
        while src.next_event().is_some() {}
        let st = src.stats();
        assert_eq!(st.events, EXPECTED_EVENTS);
        assert_eq!(st.malformed_lines, 0, "every converted line must parse");
        assert_eq!(st.out_of_order_events, 0, "the stream is sorted");
    }

    /// The converter is a pure function of its inputs: same canned API
    /// responses twice → byte-identical event stream (the manifest's sha256
    /// is a real fingerprint, not decoration).
    #[tokio::test]
    async fn the_same_inputs_convert_to_byte_identical_output() {
        let r = req(tmp("determinism"));
        let f1 = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
        let a = pull_and_convert(&f1, &r, &noop_cb()).await.unwrap();
        let bytes_a = std::fs::read(&a.events_path).unwrap();
        std::fs::remove_file(&a.manifest_path).unwrap();
        std::fs::remove_file(&a.events_path).unwrap();
        let f2 = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
        let b = pull_and_convert(&f2, &r, &noop_cb()).await.unwrap();
        let bytes_b = std::fs::read(&b.events_path).unwrap();
        assert_eq!(bytes_a, bytes_b);
    }

    #[test]
    fn slugs_and_time_args_parse() {
        let s = parse_round_slug("btc-updown-15m-1790784900").unwrap();
        assert_eq!(
            (s.asset.as_str(), s.duration_sec, s.start_sec),
            ("BTC", 900, 1_790_784_900)
        );
        let h = parse_round_slug("eth-updown-4h-123").unwrap();
        assert_eq!((h.asset.as_str(), h.duration_sec), ("ETH", 14_400));
        let m = parse_round_slug("sol-updown-1h-456").unwrap();
        assert_eq!((m.asset.as_str(), m.duration_sec), ("SOL", 3_600));
        // Non-updown markets a wallet also touched are skipped, not crashes.
        assert!(parse_round_slug("bitcoin-world-series").is_none());
        assert!(parse_round_slug("btc-updown-7x-123").is_none());
        // Calendar and epoch spellings agree; garbage is refused.
        // (2026-10-01T00:00:00Z = 1790812800; cross-checked against the
        // round-slug epochs the live API serves.)
        assert_eq!(parse_time_arg("2026-10-01").unwrap(), 1_790_812_800);
        assert_eq!(parse_time_arg("1790812800").unwrap(), 1_790_812_800);
        assert!(parse_time_arg("not-a-date").is_err());
    }
}
