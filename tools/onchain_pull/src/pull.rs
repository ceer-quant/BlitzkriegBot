use futures_util::stream::{FuturesUnordered, StreamExt};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Semaphore;

use super::*;

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

/// One condition's metadata with the CLOB fallback (gamma prunes closed
/// short-cycle updown markets; `/markets/{condition_id}` keeps the full
/// record). Adapted into the gamma shape the converter consumes —
/// `clobTokenIds` as the JSON-encoded string array, Up first, Down second.
/// Extracted from the pull loop so the bounded-concurrency pipeline calls
/// it per condition.
async fn fetch_meta(fetch: &dyn JsonFetcher, cid: &str) -> Result<Value, String> {
    let url = format!("{GAMMA_API_BASE}/markets?condition_ids={cid}");
    let resp = fetch
        .get_json(&url)
        .await
        .map_err(|e| format!("gamma {cid}: {e}"))?;
    let arr = resp.as_array().cloned().unwrap_or_default();
    let mut v = arr.into_iter().next().unwrap_or(Value::Null);
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
                    up = toks.first().map(token_of).unwrap_or_default();
                }
                if down.is_empty() {
                    down = toks.get(1).map(token_of).unwrap_or_default();
                }
                if !up.is_empty() || !down.is_empty() {
                    v = json!({
                        "clobTokenIds": serde_json::to_string(&[up, down]).unwrap_or_default(),
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
    Ok(v)
}

/// The published price points of one token/window history, in the
/// `(t, price)` shape the converter consumes. Non-parseable points are
/// dropped, never invented.
fn parse_price_points(hist: &Value) -> Vec<(i64, Decimal)> {
    hist.pointer("/history")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| {
            let t = p.get("t").and_then(Value::as_i64).unwrap_or(0);
            let raw = p.get("p").map(|x| x.to_string()).unwrap_or_default();
            Decimal::from_str_exact(raw.trim_matches('"'))
                .ok()
                .map(|d| (t, d))
        })
        .collect()
}

/// One token's published price history for a round window.
async fn fetch_prices(
    fetch: &dyn JsonFetcher,
    token: &str,
    start_sec: i64,
    end_sec: i64,
) -> Result<Value, String> {
    let url = format!(
        "{CLOB_API_BASE}/prices-history?market={token}&startTs={start_sec}&endTs={end_sec}&fidelity=1"
    );
    fetch
        .get_json(&url)
        .await
        .map_err(|e| format!("prices {token}: {e}"))
}

// ── fill-universe sources ───────────────────────────────────────────────────

/// The `/trades` walk (the original #352 fill source): offset-paged,
/// offset-cap-aware segmented walk, appending pages to the raw side file.
/// Returns `(final offset, fetched rows)`.
async fn walk_trades(
    fetch: &dyn JsonFetcher,
    req: &PullRequest,
    rp: &Path,
    mp: &Path,
    progress: &ProgressCb<'_>,
) -> Result<(i64, u64), String> {
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
            append_line(rp, &line)?;
            fetched += 1;
        }
        offset += rows.len() as i64;
        // Persist the resume state after EVERY page: a crash loses nothing
        // that the dedupe cannot absorb on the restart.
        write_json_atomic(
            mp,
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
    Ok((offset, fetched))
}

/// #355: the activity-ledger fill source. Copies every in-window `TRADE`
/// row through the raw side file; finalize dedupes/sorts exactly as for
/// walked pages. The copy is a local read — always rewritten from scratch,
/// idempotent by construction (no resume state needed).
///
/// 治 #2: REDEEM rows ride along into a SEPARATE side file (never the fill
/// stream — a redemption is not a fill and would poison the fill key space):
/// they are the wallet's own redemption activity, the on-chain evidence the
/// converter turns into verdict events. The window filter is intentionally
/// WIDER than the fills' (`end_sec` + one day): a round that expired at the
/// window's last instant settles minutes-to-hours later, and clipping the
/// evidence to the fill window would blind exactly the settlements the
/// replay's tail is supposed to close.
///
/// #388: MAKER_REBATE / REWARD rows ride a THIRD side file (again never the
/// fill stream — a rebate is not a fill). Each row is one wallet-level cash
/// payment at its own timestamp; the window is the FILLS' window (a rebate
/// prints at trade time, unlike a redemption), and the row's identity is its
/// content, so nothing needs deduping.
fn copy_activity(req: &PullRequest, path: &Path, rp: &Path) -> Result<(u64, u64, u64), String> {
    if let Some(parent) = rp.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let start_sec = req.start_ms / 1000;
    let end_sec = req.end_ms / 1000;
    let rdp = redemptions_path(req);
    let mut rf = std::io::BufWriter::new(
        std::fs::File::create(&rdp).map_err(|e| format!("create {}: {e}", rdp.display()))?,
    );
    let cfp = cashflows_path(req);
    let mut cf = std::io::BufWriter::new(
        std::fs::File::create(&cfp).map_err(|e| format!("create {}: {e}", cfp.display()))?,
    );
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(rp).map_err(|e| format!("create {}: {e}", rp.display()))?,
    );
    let mut n: u64 = 0;
    let mut r: u64 = 0;
    let mut c: u64 = 0;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // a torn line — same tolerance as finalize
        };
        let ts = v.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
        match v.get("type").and_then(Value::as_str) {
            // The activity ledger also carries MERGE/rebate rows; TRADE rows
            // are fills, REDEEM rows verdict evidence, and the cash rows
            // (#388) below wallet-level cash — each in its own channel.
            Some("TRADE") => {
                if ts < start_sec || ts > end_sec {
                    continue;
                }
                // No MAX_FILLS guard here: the ledger is a local, finite file —
                // the runaway guard exists for the network walk, not for a
                // bounded copy.
                writeln!(f, "{v}").map_err(|e| format!("write {}: {e}", rp.display()))?;
                n += 1;
            }
            Some("REDEEM") => {
                // Wider window: redemption lags expiry by minutes-to-hours.
                if ts < start_sec || ts > end_sec + 86_400 {
                    continue;
                }
                writeln!(rf, "{v}").map_err(|e| format!("write {}: {e}", rdp.display()))?;
                r += 1;
            }
            kind @ (Some("MAKER_REBATE") | Some("REWARD")) => {
                // Same window as the fills: the row IS the payment.
                if ts < start_sec || ts > end_sec {
                    continue;
                }
                writeln!(cf, "{v}").map_err(|e| format!("write {}: {e}", cfp.display()))?;
                let _ = kind;
                c += 1;
            }
            _ => continue,
        }
    }
    f.flush()
        .map_err(|e| format!("flush {}: {e}", rp.display()))?;
    rf.flush()
        .map_err(|e| format!("flush {}: {e}", rdp.display()))?;
    cf.flush()
        .map_err(|e| format!("flush {}: {e}", cfp.display()))?;
    Ok((n, r, c))
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
            // CONVERTER_VERSION gate (治 #2): a dataset converted by an older
            // converter carries no verdict events — answering it from cache
            // would silently replay the settlement blind spot forever. The
            // stem's content tag hashes the LEDGER (its identity), not the
            // converter, so a converter bump is checked here against the
            // manifest pin; a mismatch (or an older manifest with no pin)
            // falls through to a fresh convert over the same cached fills.
            if v.pointer("/converterVersion").and_then(Value::as_u64) == Some(CONVERTER_VERSION) {
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
    }

    // ── Trades ────────────────────────────────────────────────────────────
    let rp = raw_path(req);
    // Two fill sources, one canonical output: the live `/trades` walk (the
    // original #352 path) or a pre-fetched `/activity` ledger (#355 — the
    // complete universe; /trades answers only a taker-side subset). Both
    // feed the same raw side file; finalize dedupes and sorts identically.
    // #388: only the activity ledger carries cash rows, so `cashflows` is
    // 0 for a /trades walk and its side file is written EMPTY by
    // `copy_activity` (an empty stream converts zero cashflow events).
    let (offset, fetched, cashflow_rows) = match &req.fill_source {
        FillSource::TradesApi => {
            let (offset, fetched) = walk_trades(fetch, req, &rp, &mp, progress).await?;
            (offset, fetched, 0u64)
        }
        FillSource::ActivityFile(path) => {
            report(
                progress,
                PullPhase::Trades,
                0,
                format!("activity ledger {}", path.display()),
            );
            let (n, r, c) = copy_activity(req, path, &rp)?;
            report(
                progress,
                PullPhase::Trades,
                n,
                format!("rows copied, {r} redemptions, {c} cash rows"),
            );
            (0, n, c)
        }
    };

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
        // Push the asset/market filters ahead of the expensive phases: a
        // filtered pull must not spend hours fetching gamma/clob metadata
        // and price history for conditions the converter would skip anyway
        // (two parallel filtered pulls would otherwise walk the SAME
        // unfiltered condition set, duplicating each other's work through
        // the shared cache). Only applied when a filter was requested — an
        // unfiltered pull keeps its exact row set, and the converter's own
        // skip accounting (non-updown markets, dead slugs) stays untouched.
        if req.asset.is_some() || req.market.is_some() {
            let kept = parse_round_slug(v.get("slug").and_then(Value::as_str).unwrap_or(""))
                .is_some_and(|rs| {
                    let dur_label = match rs.duration_sec {
                        300 => "5m",
                        900 => "15m",
                        3600 => "1h",
                        14_400 => "4h",
                        _ => "",
                    };
                    req.asset
                        .as_ref()
                        .is_none_or(|a| a.eq_ignore_ascii_case(&rs.asset))
                        && req
                            .market
                            .as_ref()
                            .is_none_or(|m| m.eq_ignore_ascii_case(dur_label))
                });
            if !kept {
                continue;
            }
        }
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
    {
        // Bounded-concurrency metadata pipeline: conditions stream through a
        // semaphore-backed window (META_CONCURRENCY in flight), each task
        // reusing the extracted per-condition fetch. Cache hits return
        // without touching the network, so a re-pull pays only for holes.
        let sem = Arc::new(Semaphore::new(META_CONCURRENCY));
        let mut tasks: FuturesUnordered<_> = conditions
            .iter()
            .map(|cid| {
                let sem = sem.clone();
                async move {
                    let _g = sem.acquire().await;
                    let cp = condition_cache_path(&req.out_dir, cid);
                    let cached = if cp.exists() {
                        Some(read_json(&cp)?)
                    } else {
                        None
                    };
                    // A cached `null` is a MISS, not a fact: Gamma prunes closed
                    // short-cycle updown markets (it answered `[]` for every September
                    // 2026 condition — probed live), and the CLOB fallback recovers
                    // them, so a re-pull self-heals instead of replaying the hole.
                    match cached {
                        Some(v) if !v.is_null() && v.get("clobTokenIds").is_some() => {
                            Ok::<_, String>((cid.clone(), v, false))
                        }
                        _ => {
                            let v = fetch_meta(fetch, cid).await?;
                            let missed = v.is_null();
                            write_json_atomic(&cp, &v)?;
                            Ok((cid.clone(), v, missed))
                        }
                    }
                }
            })
            .collect();
        while let Some(r) = tasks.next().await {
            let (cid, v, missed) = r?;
            if missed {
                no_meta += 1;
            }
            gamma.insert(cid.clone(), v);
            report(
                progress,
                PullPhase::Markets,
                gamma.len() as u64,
                cid.clone(),
            );
        }
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
    // published price series (the honest top-of-book reconstruction). The
    // work list is deduped up front (a token can serve several conditions),
    // then streamed through the bounded-concurrency pipeline.
    let mut price_series: BTreeMap<(String, i64, i64), Vec<(i64, Decimal)>> = BTreeMap::new();
    let mut price_jobs: Vec<(String, i64, i64)> = Vec::new();
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
        for token in gamma_tokens(&g) {
            if token.is_empty() {
                continue;
            }
            let key = (token.clone(), rs.start_sec, end_sec);
            if !price_jobs.contains(&key) {
                price_jobs.push(key);
            }
        }
    }
    {
        let sem = Arc::new(Semaphore::new(PRICE_CONCURRENCY));
        let mut tasks: FuturesUnordered<_> = price_jobs
            .into_iter()
            .map(|key| {
                let sem = sem.clone();
                async move {
                    let _g = sem.acquire().await;
                    let (ref token, start_sec, end_sec) = key;
                    let pp = price_cache_path(&req.out_dir, token, start_sec, end_sec);
                    let hist = if pp.exists() {
                        read_json(&pp)?
                    } else {
                        let v = fetch_prices(fetch, token, start_sec, end_sec).await?;
                        write_json_atomic(&pp, &v)?;
                        v
                    };
                    Ok::<_, String>((key, parse_price_points(&hist)))
                }
            })
            .collect();
        while let Some(r) = tasks.next().await {
            let (key, pts) = r?;
            let token = key.0.clone();
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
    // 治 #2: the redemption side file (written by copy_activity for an
    // activity-ledger source; absent for a /trades walk) is the wallet's own
    // on-chain verdict evidence — folded per condition and handed to the
    // converter, which flags verdict markets and emits Resolution events.
    let verdicts = load_redemptions(&redemptions_path(req))?;
    // #388: the cash side file folds each MAKER_REBATE / REWARD row into one
    // `cashflow` event (a /trades walk has none, so its datasets convert
    // cashflow-less and replay exactly as before).
    let cashflows = load_cashflows(&cashflows_path(req))?;
    let (events, skipped) = convert(&rows, &gamma, &price_series, &verdicts, &cashflows, req)?;
    write_events(&ep, &events)?;
    report(
        progress,
        PullPhase::Convert,
        events.len() as u64,
        format!(
            "{} verdict conditions, {} cashflow events",
            verdicts.len(),
            cashflows.len()
        ),
    );

    let trades_sha = sha256_file(&tp)?;
    let events_sha = sha256_file(&ep)?;
    let manifest = json!({
        "wallet": req.wallet,
        "startMs": req.start_ms, "endMs": req.end_ms,
        "asset": req.asset, "market": req.market,
        // 治 #2: the cache-hit gate reads this pin — a dataset converted by
        // an older converter (no pin, or a lower one) is re-converted instead
        // of silently replaying the settlement blind spot forever.
        "converterVersion": CONVERTER_VERSION,
        "pulledAtMs": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "files": {
            "trades": tp.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            "events": ep.file_name().and_then(|s| s.to_str()).unwrap_or(""),
        },
        "state": { "tradesOffset": offset, "fetched": fetched, "complete": true },
        "counts": { "trades": trades_n, "conditions": conditions.len(), "events": events.len(), "skippedRows": skipped, "verdicts": verdicts.len(), "cashflows": cashflow_rows },
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
