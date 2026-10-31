use super::*;
use blitzkrieg_data_model::CashflowKind;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
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
        fill_source: Default::default(),
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
    meta_urls: Mutex<Vec<String>>,
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
            self.meta_urls.lock().unwrap().push(url.to_string());
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
        meta_urls: Mutex::default(),
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

/// The asset/market filters are part of the EXPENSIVE phases, not just
/// the converter: a filtered pull must not spend hours fetching gamma
/// metadata and price history for conditions the converter would skip
/// (two parallel filtered pulls would walk the same unfiltered
/// condition set through the shared cache, duplicating work). A 15m
/// pull over a mixed 15m+5m corpus captures only the 15m fills and
/// touches only the 15m conditions' metadata.
#[tokio::test]
async fn a_filtered_pull_skips_metadata_for_filtered_out_conditions() {
    const N: usize = 100;
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
        meta_urls: Mutex::default(),
        gammas: gamma_map(),
    };
    let mut r = req(tmp("filtered-meta"));
    r.market = Some("15m".into());
    r.start_ms = (T0 - (N as i64 + 500)) * 1000;
    r.end_ms = (T0 + 10) * 1000;
    let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
    // The walk itself stays unfiltered (the offset-cap split needs the
    // real page shape; the raw corpus is the wallet's whole-window
    // truth) — the filter lives in the row collection: metadata is
    // fetched ONLY for surviving conditions and the event stream
    // carries only them.
    assert_eq!(o.trades, N as u64, "raw corpus is the unfiltered truth");
    let metas = fetch.meta_urls.lock().unwrap();
    assert!(
        metas.iter().any(|u| u.contains(COND1)),
        "the kept condition's metadata is fetched"
    );
    assert!(
        metas.iter().all(|u| !u.contains(COND2)),
        "no metadata requests for the filtered-out 5m condition: {metas:?}"
    );
    let manifest = std::fs::read_to_string(&o.manifest_path).unwrap();
    let manifest: Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(
        manifest
            .pointer("/counts/conditions")
            .and_then(Value::as_u64),
        Some(1),
        "only the 15m condition survives into the dataset"
    );
    let events = std::fs::read_to_string(&o.events_path).unwrap();
    assert!(
        !events.contains(COND2),
        "no event references the filtered-out condition"
    );
    verify_dataset(&o.manifest_path).unwrap();
}

/// #355: the activity-ledger fill source. `/trades` answers only a
/// taker-side subset of a wallet's fills (measured 2026-10-02: 17.6% of
/// the same window's notional), so the pull must be able to take its
/// universe from a pre-fetched `/activity` export instead: TRADE rows
/// only, window-clamped, ZERO network in the fills phase — while the
/// metadata/price phases still run (the ledger carries no token ids).
#[tokio::test]
async fn an_activity_ledger_seeds_the_universe_without_trades_api_calls() {
    const T0: i64 = 1_790_785_000;
    let fill = |i: usize, cid: &str, tok: &str, slug: &str, ts: i64| {
        json!({
            "proxyWallet": WALLET, "type": "TRADE", "side": "BUY",
            "asset": tok, "conditionId": cid,
            "size": 1.0, "price": 0.5, "timestamp": ts,
            "slug": slug, "outcome": "Up", "outcomeIndex": 0,
            "transactionHash": format!("0x{i:x}"),
        })
    };
    let mut rows = Vec::new();
    for i in 0..40 {
        let (cid, tok, slug) = if i % 2 == 0 {
            (COND1, UP1, "btc-updown-15m-1790784900")
        } else {
            (COND2, DOWN2, "btc-updown-5m-1790785200")
        };
        rows.push(fill(i, cid, tok, slug, T0));
    }
    // A REDEEM row and an out-of-window fill must never enter the corpus.
    rows.push(json!({
        "proxyWallet": WALLET, "type": "REDEEM", "side": "",
        "asset": UP1, "conditionId": COND1, "size": 10.0, "usdcSize": 10.0,
        "price": 0, "timestamp": T0, "slug": "btc-updown-15m-1790784900",
        "transactionHash": "0xdead",
    }));
    rows.push(fill(99, COND1, UP1, "btc-updown-15m-1790784900", T0 - 3600));
    let ledger = tmp("activity").join("activity.jsonl");
    std::fs::write(
        &ledger,
        rows.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();

    let fetch = PagingVenue {
        all: Vec::new(),
        trades_urls: Mutex::default(),
        meta_urls: Mutex::default(),
        gammas: gamma_map(),
    };
    let mut r = req(tmp("activity-pull"));
    r.fill_source = FillSource::ActivityFile(ledger);
    let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();

    assert!(
        fetch.trades_urls.lock().unwrap().is_empty(),
        "the /trades endpoint is never touched by the activity source"
    );
    assert_eq!(
        o.trades, 40,
        "in-window TRADE rows only: no REDEEM, no out-of-window row"
    );
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&o.manifest_path).unwrap()).unwrap();
    assert_eq!(
        manifest
            .pointer("/counts/conditions")
            .and_then(Value::as_u64),
        Some(2),
        "both durations' conditions survive into the dataset"
    );
    verify_dataset(&o.manifest_path).unwrap();
}

/// #388: MAKER_REBATE / REWARD ledger rows become wallet-level `cashflow`
/// events — one per row at the row's own timestamp, exact amounts — while
/// MERGE rows (and every other type) stay ignored. The manifest counts the
/// rows separately from the events, and the stream still replays clean.
#[tokio::test]
async fn activity_cash_rows_become_cashflow_events() {
    const T0: i64 = 1_790_785_000;
    let rows = [
        json!({
            "proxyWallet": WALLET, "type": "TRADE", "side": "BUY",
            "asset": UP1, "conditionId": COND1, "size": 1.0, "price": 0.5,
            "timestamp": T0, "slug": "btc-updown-15m-1790784900",
            "outcome": "Up", "outcomeIndex": 0,
            "transactionHash": "0x1",
        }),
        // Two rebates and one reward, all in-window, wallet-level
        // (conditionId empty — the real export's shape).
        json!({
            "proxyWallet": WALLET, "type": "MAKER_REBATE", "side": "",
            "asset": "", "conditionId": "", "size": 0.0,
            "usdcSize": 331.2175, "price": 0, "timestamp": T0 + 5,
            "slug": "", "outcomeIndex": 999,
            "transactionHash": "0xdead",
        }),
        json!({
            "proxyWallet": WALLET, "type": "MAKER_REBATE", "side": "",
            "asset": "", "conditionId": "", "size": 0.0,
            "usdcSize": 120.0, "price": 0, "timestamp": T0 + 10,
            "slug": "", "outcomeIndex": 999,
            "transactionHash": "0xdead2",
        }),
        json!({
            "proxyWallet": WALLET, "type": "REWARD", "side": "",
            "asset": "", "conditionId": "", "size": 0.0,
            "usdcSize": 659.995, "price": 0, "timestamp": T0 + 15,
            "slug": "", "outcomeIndex": 999,
            "transactionHash": "0xdead3",
        }),
        // A MERGE row: the ledger carries them too; never a cash event.
        json!({
            "proxyWallet": WALLET, "type": "MERGE", "side": "",
            "asset": "", "conditionId": COND1, "size": 5.0,
            "usdcSize": 5.0, "price": 0, "timestamp": T0 + 20,
            "slug": "", "outcomeIndex": 999,
            "transactionHash": "0xdead4",
        }),
        // An out-of-window rebate: clipped by the fills' window.
        json!({
            "proxyWallet": WALLET, "type": "MAKER_REBATE", "side": "",
            "asset": "", "conditionId": "", "size": 0.0,
            "usdcSize": 9999.0, "price": 0, "timestamp": T0 + 100_000,
            "slug": "", "outcomeIndex": 999,
            "transactionHash": "0xdead5",
        }),
    ];
    let ledger = tmp("cashrows").join("activity.jsonl");
    std::fs::write(
        &ledger,
        rows.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let mut r = req(tmp("cashrows-pull"));
    r.fill_source = FillSource::ActivityFile(ledger);
    let fetch = PagingVenue {
        all: Vec::new(),
        trades_urls: Mutex::default(),
        meta_urls: Mutex::default(),
        gammas: gamma_map(),
    };
    let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();

    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&o.manifest_path).unwrap()).unwrap();
    assert_eq!(
        manifest
            .pointer("/counts/cashflows")
            .and_then(Value::as_u64),
        Some(3),
        "in-window MAKER_REBATE/REWARD rows only: no MERGE, no out-of-window row"
    );

    // Exactly three cashflow events, at their rows' own instants, with
    // the exact amounts the ledger carried.
    let txt = std::fs::read_to_string(&o.events_path).unwrap();
    let cf: Vec<Value> = txt
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("k").and_then(Value::as_str) == Some("cashflow"))
        .collect();
    assert_eq!(cf.len(), 3, "one event per cash row: {txt}");
    assert_eq!(cf[0].get("which").and_then(Value::as_str), Some("rebate"));
    assert_eq!(
        cf[0].get("usd").and_then(Value::as_str),
        Some("331.2175"),
        "the amount rides exactly, string-decimal"
    );
    assert_eq!(cf[1].get("usd").and_then(Value::as_str), Some("120.0"));
    assert_eq!(cf[2].get("which").and_then(Value::as_str), Some("reward"));
    assert_eq!(cf[2].get("usd").and_then(Value::as_str), Some("659.995"));
    assert_eq!(
        cf.iter()
            .map(|v| v.get("at").and_then(Value::as_i64).unwrap())
            .sum::<i64>(),
        (T0 + 5) * 1000 + (T0 + 10) * 1000 + (T0 + 15) * 1000,
        "each event rides its own row's timestamp"
    );

    // The whole stream still replays clean through the parser.
    verify_dataset(&o.manifest_path).unwrap();
    use blitzkrieg_data_model::DataSource as _;
    let mut src = blitzkrieg_data_model::open_replay(o.events_path.to_str().unwrap()).unwrap();
    let mut n = 0u64;
    while src.next_event().is_some() {
        n += 1;
    }
    assert_eq!(
        n,
        manifest
            .pointer("/counts/events")
            .and_then(Value::as_u64)
            .unwrap()
    );
    assert_eq!(src.stats().malformed_lines, 0, "no line left behind");
}

/// #388: a `/trades` walk has no cash rows — its dataset converts
/// cashflow-less (`counts.cashflows == 0`) and replays exactly the
/// pre-#388 stream.
#[tokio::test]
async fn a_trades_api_dataset_has_no_cashflow_events() {
    let fetch = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
    let r = req(tmp("no-cashrows"));
    let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&o.manifest_path).unwrap()).unwrap();
    assert_eq!(
        manifest
            .pointer("/counts/cashflows")
            .and_then(Value::as_u64),
        Some(0),
    );
    let txt = std::fs::read_to_string(&o.events_path).unwrap();
    assert!(!txt.contains("\"k\":\"cashflow\""));
}

/// #388 unit semantics: the fold refuses what it cannot state exactly —
/// a non-exact amount or a torn timestamp fails the row, never zeroes it.
#[test]
fn cash_rows_fold_exact_or_refuse() {
    let mut out = Vec::new();
    fold_cashflow(
        &mut out,
        &json!({"type": "MAKER_REBATE", "usdcSize": 331.2175, "timestamp": 1_790_785_005}),
    )
    .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].which, CashflowKind::Rebate);
    assert_eq!(out[0].amount_usd, Decimal::new(3312175, 4));
    assert_eq!(out[0].at_ms, 1_790_785_005_000);

    // A quoted amount (the export's other shape) parses the same.
    fold_cashflow(
        &mut out,
        &json!({"type": "REWARD", "usdcSize": "659.995", "timestamp": 1_790_785_015}),
    )
    .unwrap();
    assert_eq!(out[1].which, CashflowKind::Reward);

    // A non-exact binary float refuses rather than rounding silently
    // (0.1 prints as its exact decimal literal but 1e300's `to_string`
    // is exponential, which `from_str_exact` refuses).
    assert!(
        fold_cashflow(
            &mut out,
            &json!({"type": "REWARD", "usdcSize": 1e300, "timestamp": 5})
        )
        .is_err()
    );
    // A torn timestamp refuses.
    assert!(
        fold_cashflow(
            &mut out,
            &json!({"type": "REWARD", "usdcSize": 1.0, "timestamp": 0})
        )
        .is_err()
    );
    // A wrong type refuses.
    assert!(
        fold_cashflow(
            &mut out,
            &json!({"type": "MERGE", "usdcSize": 1.0, "timestamp": 5})
        )
        .is_err()
    );
    assert_eq!(out.len(), 2, "refusals never push a row");
}

/// 治 #2 unit semantics: the two kinds of REDEEM evidence and the two
/// fallbacks, folded and resolved exactly as the converter consumes them.
#[test]
fn redemption_evidence_folds_and_resolves() {
    // A WINNING row (cash received) names the winner asset outright; the
    // evidence-free companion row (size 0, no asset) changes nothing.
    let mut v = BTreeMap::new();
    fold_redemption(
        &mut v,
        &json!({"conditionId": COND1, "type": "REDEEM", "asset": UP1,
                "outcomeIndex": 0, "size": 41.0, "usdcSize": 41.0}),
    );
    fold_redemption(
        &mut v,
        &json!({"conditionId": COND1, "type": "REDEEM", "asset": "",
                "outcomeIndex": 0, "size": 0.0, "usdcSize": 0.0}),
    );
    let vd = &v[COND1];
    assert_eq!(resolve_winner(vd, UP1, DOWN1).as_deref(), Some(UP1));
    assert_eq!(
        resolved_pair(vd, UP1, DOWN1),
        Some((UP1.to_string(), DOWN1.to_string()))
    );

    // A BURN row (no cash, shares destroyed) names the LOSER; the winner
    // is the other side.
    let mut v = BTreeMap::new();
    fold_redemption(
        &mut v,
        &json!({"conditionId": COND2, "type": "REDEEM", "asset": UP2,
                "outcomeIndex": 0, "size": 41.0, "usdcSize": 0.0}),
    );
    assert_eq!(
        resolve_winner(&v[COND2], UP2, DOWN2).as_deref(),
        Some(DOWN2)
    );

    // The outcome index substitutes when the row carries no asset —
    // gamma's order ([0]=up, [1]=down) is the market's own.
    let mut v = BTreeMap::new();
    fold_redemption(
        &mut v,
        &json!({"conditionId": "c3", "type": "REDEEM", "asset": "",
                "outcomeIndex": 1, "size": 0.0, "usdcSize": 7.0}),
    );
    assert_eq!(
        resolve_winner(&v["c3"], "t-up", "t-down").as_deref(),
        Some("t-down")
    );
    fold_redemption(
        &mut v,
        &json!({"conditionId": "c4", "type": "REDEEM", "asset": "",
                "outcomeIndex": 0, "size": 9.0, "usdcSize": 0.0}),
    );
    assert_eq!(
        resolve_winner(&v["c4"], "t-up", "t-down").as_deref(),
        Some("t-down"),
        "a proven UP loser makes DOWN the winner"
    );

    // Evidence against neither declared token proves nothing — and a
    // one-sided market (a pruned gamma) can never name both sides, so it
    // stays with the dry ladder.
    let mut v = BTreeMap::new();
    fold_redemption(
        &mut v,
        &json!({"conditionId": "c5", "type": "REDEEM", "asset": "OTHER",
                "outcomeIndex": -1, "size": 5.0, "usdcSize": 5.0}),
    );
    assert_eq!(resolve_winner(&v["c5"], UP1, DOWN1), None);
    assert_eq!(resolved_pair(&v["c5"], UP1, DOWN1), None);
    assert_eq!(resolved_pair(&ConditionVerdict::default(), UP1, ""), None);
}

/// A `/trades` walk (no REDEEM rows) produces no redemption file — its
/// datasets convert verdict-less and the dry ladder keeps settling them,
/// exactly the pre-治#2 behavior.
#[test]
fn a_trades_api_dataset_has_no_verdict_file() {
    let r = req(tmp("no-verdicts"));
    assert!(load_redemptions(&redemptions_path(&r)).unwrap().is_empty());
}

/// One COND1 trade + its winning REDEEM, plus one COND2 trade with no
/// redeem evidence. The dataset must carry the verdict as a first-class
/// event at the round boundary AND flag the market, so the replay settles
/// from the stream and the dry ladder stands down.
fn verdict_dataset(tag: &str) -> (PullRequest, PagingVenue) {
    const T0: i64 = 1_790_785_000;
    let rows = [
        json!({
            "proxyWallet": WALLET, "type": "TRADE", "side": "BUY",
            "asset": UP1, "conditionId": COND1, "size": 1.0, "price": 0.5,
            "timestamp": T0, "slug": "btc-updown-15m-1790784900",
            "outcome": "Up", "outcomeIndex": 0,
            "transactionHash": "0x1",
        }),
        json!({
            "proxyWallet": WALLET, "type": "REDEEM", "side": "",
            "asset": UP1, "conditionId": COND1, "size": 1.0, "usdcSize": 1.0,
            "price": 0, "timestamp": T0 + 60,
            "slug": "btc-updown-15m-1790784900",
            "outcome": "Up", "outcomeIndex": 0,
            "transactionHash": "0xdead",
        }),
        json!({
            "proxyWallet": WALLET, "type": "TRADE", "side": "SELL",
            "asset": DOWN2, "conditionId": COND2, "size": 1.0, "price": 0.4,
            "timestamp": T0, "slug": "btc-updown-5m-1790785200",
            "outcome": "Down", "outcomeIndex": 1,
            "transactionHash": "0x2",
        }),
    ];
    let ledger = tmp(tag).join("activity.jsonl");
    std::fs::write(
        &ledger,
        rows.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let mut r = req(tmp(&format!("{tag}-pull")));
    r.fill_source = FillSource::ActivityFile(ledger);
    let fetch = PagingVenue {
        all: Vec::new(),
        trades_urls: Mutex::default(),
        meta_urls: Mutex::default(),
        gammas: gamma_map(),
    };
    (r, fetch)
}

#[tokio::test]
async fn activity_redemptions_become_verdict_events_and_flags() {
    let (r, fetch) = verdict_dataset("verdicts");
    let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();

    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&o.manifest_path).unwrap()).unwrap();
    assert_eq!(
        manifest
            .pointer("/converterVersion")
            .and_then(Value::as_u64),
        Some(CONVERTER_VERSION),
        "the manifest pins the converter version"
    );
    assert_eq!(
        manifest.pointer("/counts/verdicts").and_then(Value::as_u64),
        Some(1),
        "one condition's redemption evidence"
    );

    // The stream settles COND1 at the round's expiry instant, BOTH sides
    // named, in the market's OWN outcome order (up first — the NegRisk
    // redeem order).
    let txt = std::fs::read_to_string(&o.events_path).unwrap();
    let line = txt
        .lines()
        .find(|l| l.contains("\"k\":\"resolution\""))
        .expect("a resolution event");
    let v: Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["c"], COND1);
    assert_eq!(
        v["at"],
        json!(1_790_785_800_000i64),
        "stamped at the round boundary, not at the redeem print"
    );
    assert_eq!(v["p"], json!([[UP1, "1"], [DOWN1, "0"]]));
    // And the line parses back into the event the replay dispatches.
    match blitzkrieg_data_model::event_from_json(&v).unwrap() {
        blitzkrieg_data_model::DataEvent::Resolution {
            condition_id,
            payouts,
            ..
        } => {
            assert_eq!(condition_id, COND1);
            assert_eq!(
                payouts,
                vec![
                    (UP1.to_string(), Decimal::ONE),
                    (DOWN1.to_string(), Decimal::ZERO)
                ]
            );
        }
        other => panic!("wrong kind: {other:?}"),
    }

    // The declaration flags the verdict market (dry ladder stands down);
    // the market without redeem evidence stays unflagged.
    assert!(
        txt.contains("\"archiveVerdict\":true"),
        "COND1's market declares the archive verdict: {txt}"
    );
    assert!(
        txt.contains("\"archiveVerdict\":false"),
        "COND2's market stays unflagged"
    );

    // A warm-cache re-run with the SAME converter answers from cache.
    let o2 = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
    assert!(o2.cache_hit, "same converter version: cache hit");
}

#[tokio::test]
async fn a_pre_verdict_dataset_reconverts_instead_of_answering_stale() {
    let (r, fetch) = verdict_dataset("stale-verdicts");
    let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&o.manifest_path).unwrap()).unwrap();
    let events_sha = manifest
        .pointer("/sha256/events")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Strip the pin the way an OLDER converter's manifest would lack it.
    let mp = o.manifest_path.clone();
    let mut m = manifest;
    m.as_object_mut().unwrap().remove("converterVersion");
    std::fs::write(&mp, m.to_string()).unwrap();

    // The next pull must NOT answer from cache: it re-converts over the
    // same cached fills (the activity path touches no network) and
    // re-pins the version.
    let o2 = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
    assert!(
        !o2.cache_hit,
        "a manifest without the version pin is a pre-verdict dataset"
    );
    let m2: Value =
        serde_json::from_str(&std::fs::read_to_string(&o2.manifest_path).unwrap()).unwrap();
    assert_eq!(
        m2.pointer("/converterVersion").and_then(Value::as_u64),
        Some(CONVERTER_VERSION)
    );
    // Deterministic converter: the re-converted stream is byte-identical.
    assert_eq!(
        m2.pointer("/sha256/events").and_then(Value::as_str),
        Some(events_sha.as_str()),
        "the re-convert reproduces the same event stream"
    );
    let txt = std::fs::read_to_string(&o2.events_path).unwrap();
    assert!(txt.contains("\"k\":\"resolution\""));
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
    use blitzkrieg_data_model::DataSource;
    let fetch = Scripted::new(vec![rows_ab(), vec![]], gamma_map(), false);
    let r = req(tmp("replay"));
    let o = pull_and_convert(&fetch, &r, &noop_cb()).await.unwrap();
    // The CLI opens the dataset with open_replay_all (segment discovery),
    // so the acceptance must run through that path: the sibling
    // `<stem>.trades.jsonl` side file and the manifest sit in the same
    // directory and must not leak into the replay as phantom segments.
    let mut src = blitzkrieg_data_model::open_replay_all(o.events_path.to_str().unwrap()).unwrap();
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
