//! Feed layer — Rust-native market data ingestion (P4).
//!
//! Two sources, both with reconnect:
//!   - Polymarket orderbook: REST `POST /books` polling (see [`DEFAULT_POLL_MS`]
//!     for why this is deliberately *not* the venue's WebSocket market channel).
//!   - Binance spot: a raw `tokio-tungstenite` connection to the combined
//!     `@trade` stream, parsed into per-asset spot ticks for the momentum filter.
//!
//! Everything is translated into `MarketHost` calls (the market seam), so this
//! module no longer touches `Core` directly and can move into a market extension
//! unchanged. Node no longer has to push `books.*` / `spot.price`; those IPC
//! methods remain for tests and as a manual override.
//!
//! The authenticated *user* channel (order and fill events) stays on WebSocket,
//! in `venue.rs`: it carries only this bot's own orders rather than a whole
//! market, so it costs almost nothing, and a fill wants to arrive as an event
//! rather than on a poll boundary.

use blitzkrieg_market_api::{BookUpdate, MarketHost, SpotUpdate};
use futures_util::StreamExt;
use polymarket_client_sdk_v2::error::{
    Error as SdkError, Kind as SdkErrorKind, Status as SdkErrorStatus,
};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Default interval for polling `POST /books`, in milliseconds.
///
/// The venue's market *WebSocket* channel has no message-type filter, no
/// negotiated compression and no per-token throttle. Measured on this bot's own
/// subscription it costs 33-40 GB/day per socket, and 98.8% of those bytes are
/// `price_change` frames that repeat a quote already received: a 10-second
/// sample for one token carried 706 quote-bearing entries with exactly 1
/// distinct value (88.2 entries/s against 0.12 real changes/s), i.e. 99.9% of
/// the wire was re-sends. That is what put ~250 GB/day through the VPN.
///
/// `POST /books` returns full depth for many tokens in a single request and
/// carries a venue timestamp, measured at 20 KB for all 8 round tokens — about
/// 1.75 GB/day at this interval against 33-40 GB/day, and it is a *snapshot*
/// API, so there is no subscription to release and therefore nothing that can
/// leak on a round rollover. Interval is `POLYMARKET_POLL_MS`.
const DEFAULT_POLL_MS: u64 = 1_000;

/// Floor on the poll interval. A typo (or an over-eager setting) otherwise turns
/// straight back into the bandwidth problem this replaced, so the knob has a
/// bottom: even at the floor the feed costs ~5x less than the WebSocket did.
const MIN_POLL_MS: u64 = 250;

/// Ceiling on the poll interval, and the cap for the 429 backoff below.
const MAX_POLL_MS: u64 = 60_000;

/// Bound on a single poll. A request that never returns would silently stop the
/// book advancing, which the engine reports only as an unpricable market; a
/// timeout turns it into a logged event instead.
const REQUEST_TIMEOUT_MS: u64 = 5_000;

/// The engine's *compiled default* `max_orderbook_stale_ms` (`engine.rs`,
/// `DEFAULT_MAX_ORDERBOOK_STALE_MS`): it refuses to price a book it considers
/// older than this. Kept here as a named constant so the poll-cadence guard and
/// its test state the dependency explicitly rather than carrying a bare `8000`.
///
/// The kernel's budget is configurable (`--max-orderbook-stale-ms` /
/// `BK_MAX_ORDERBOOK_STALE_MS`, issue #205) and this crate must not depend on
/// `blitzkrieg-core`, so the warning below resolves the operator's value from the
/// same environment variable and falls back to this constant — the value the
/// kernel itself uses when the variable is unset. Only the fallback and the
/// compile-time cadence assert are frozen at build time.
const ENGINE_MAX_ORDERBOOK_STALE_MS: u64 = 8_000;

/// Upper bound the kernel accepts for the budget (`MAX_ORDERBOOK_STALE_MS_CEILING`
/// in `engine.rs`). A larger value aborts the kernel at startup, so it cannot be
/// the budget in force and is treated here as unset.
const MAX_ORDERBOOK_STALE_MS_CEILING: i64 = 600_000;

/// Latency allowance folded into the staleness warning below. The poll loop
/// sleeps a full interval *after* each response, so the achieved period is
/// `interval + request latency`. A cold request was measured at 657 ms (client
/// build, TLS, first call); warm keep-alive calls are far cheaper. This is the
/// value the warning assumes, chosen generous so the warning is not purely
/// theoretical.
const LATENCY_ALLOWANCE_MS: u64 = 500;

/// Resolve `POLYMARKET_POLL_MS`.
///
/// Unset, non-numeric and `0` all fall back to [`DEFAULT_POLL_MS`] rather than to
/// "no delay", so a malformed value can only ever cost the default cadence, never
/// become a busy loop against the venue. Values outside the supported band are
/// clamped into it for the same reason.
fn parse_poll_ms(raw: Option<&str>) -> u64 {
    raw.map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_POLL_MS)
        .clamp(MIN_POLL_MS, MAX_POLL_MS)
}

/// Whether the venue refused the request with HTTP 429.
///
/// The SDK folds every non-2xx into one error kind and keeps the status in a
/// downcastable source, so the rate limit has to be recovered from there rather
/// than read off the top-level variant.
fn is_rate_limited(e: &SdkError) -> bool {
    e.kind() == SdkErrorKind::Status
        && e.downcast_ref::<SdkErrorStatus>()
            .is_some_and(|s| s.status_code == 429)
}

/// The engine's freshness budget in force, resolved from
/// `BK_MAX_ORDERBOOK_STALE_MS`, or `None` when there is no budget to warn about.
///
/// The kernel validates the same variable loudly (a non-numeric, negative or
/// over-ceiling value aborts startup with exit 2, and `0` turns the freshness
/// check OFF); this reader only has to agree with what the kernel does *not*
/// reject, because the two run in one process:
///
/// - unset / empty / unparseable / out of band -> the compiled default, which is
///   the kernel's own fallback (the rejected cases cannot be running);
/// - `0` -> `None`: the freshness check is disabled, so a slow poll cannot leave
///   the engine with an unpricable book and there is nothing to warn about;
/// - positive -> that many milliseconds.
fn parse_stale_budget(raw: Option<&str>) -> Option<u64> {
    match raw.map(str::trim) {
        None | Some("") => Some(ENGINE_MAX_ORDERBOOK_STALE_MS),
        Some(v) => match v.parse::<i64>() {
            Ok(0) => None,
            Ok(ms) if ms > 0 && ms <= MAX_ORDERBOOK_STALE_MS_CEILING => Some(ms as u64),
            _ => Some(ENGINE_MAX_ORDERBOOK_STALE_MS),
        },
    }
}

/// The budget currently in force, read from the environment once per call site.
fn effective_stale_budget_ms() -> Option<u64> {
    parse_stale_budget(std::env::var("BK_MAX_ORDERBOOK_STALE_MS").ok().as_deref())
}

/// Whether a poll interval leaves the engine a priceable book, and why not.
///
/// Polling slower than the engine's freshness budget leaves the book unpricable
/// between polls, which surfaces as "the bot stopped trading" rather than as a
/// configuration error; this is what lets the feed say which it is. The latency
/// allowance is included because the loop sleeps *after* each response, so the
/// achieved period is `interval + latency`, not the interval alone.
///
/// Pure so the guard can be tested directly: driving it through `spawn_feed`
/// would need a full `MarketHost` stub for one log line. `budget_ms` is `None`
/// when the kernel's freshness check is off (`--max-orderbook-stale-ms 0`), in
/// which case no cadence can starve it.
fn cadence_warning_with(poll_ms: u64, budget_ms: Option<u64>) -> Option<String> {
    let budget = budget_ms?;
    if poll_ms + LATENCY_ALLOWANCE_MS <= budget {
        return None;
    }
    Some(format!(
        "poly poll interval {poll_ms}ms (plus up to {LATENCY_ALLOWANCE_MS}ms request latency) \
         is close to or past the engine's {budget}ms orderbook \
         staleness budget: the engine will have a stale book for part of every poll cycle"
    ))
}

/// [`cadence_warning_with`] against the budget the kernel is actually running
/// with (`BK_MAX_ORDERBOOK_STALE_MS`, else the compiled default).
fn cadence_warning(poll_ms: u64) -> Option<String> {
    cadence_warning_with(poll_ms, effective_stale_budget_ms())
}

/// Market-data event produced by the feed loops, consumed by the pump which
/// forwards each one onto the [`MarketHost`].
#[derive(Debug, Clone)]
pub enum FeedEvent {
    Book {
        token_id: String,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        now_ms: i64,
    },
    Spot {
        asset: String,
        price: Decimal,
        now_ms: i64,
    },
    Info(String),
}

/// Shared wall clock for the extension's feed-side modules (discovery/live
/// reuse this one instead of keeping their own copies).
pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run all feeds, forwarding every update to the [`MarketHost`]. `token_ids` is
/// the initial subscription set; the caller may add more via [`FeedHandle`].
pub async fn spawn_feed(
    host: Arc<dyn MarketHost>,
    token_ids: Vec<String>,
    binance_assets: Vec<String>,
) -> FeedHandle {
    let (ev_tx, ev_rx) = mpsc::channel::<FeedEvent>(1024);

    // Event pump: feed → host (the market seam).
    {
        let host = host.clone();
        let mut ev_rx = ev_rx;
        tokio::spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                match ev {
                    FeedEvent::Book {
                        token_id,
                        bids,
                        asks,
                        now_ms,
                    } => {
                        host.on_book(BookUpdate {
                            token_id,
                            bids,
                            asks,
                            ts_ms: now_ms,
                        })
                        .await;
                    }
                    FeedEvent::Spot {
                        asset,
                        price,
                        now_ms,
                    } => {
                        host.on_spot(SpotUpdate {
                            asset,
                            price,
                            ts_ms: now_ms,
                        })
                        .await;
                    }
                    FeedEvent::Info(msg) => {
                        tracing::info!(feed = %msg, "feed");
                    }
                }
            }
        });
    }

    let (ctl_tx, ctl_rx) = mpsc::channel::<Vec<String>>(16);

    // Polymarket orderbook feed. Endpoint and cadence are resolved here rather
    // than inside the loop so the loop takes both as plain arguments and can be
    // exercised against a local server in tests. `CLOB_API_URL` is the same
    // variable the venue actor reads, so one setting covers the whole CLOB API.
    let rest_url =
        std::env::var("CLOB_API_URL").unwrap_or_else(|_| "https://clob.polymarket.com".into());
    let poll_ms = parse_poll_ms(std::env::var("POLYMARKET_POLL_MS").ok().as_deref());
    {
        let ev_tx = ev_tx.clone();
        let tokens = token_ids.clone();
        let rest_url = rest_url.clone();
        tokio::spawn(async move {
            poly_rest_loop(tokens, ev_tx, ctl_rx, rest_url, poll_ms).await;
        });
    }

    let _ = ev_tx
        .send(FeedEvent::Info(format!(
            "poly rest feed polling every {poll_ms}ms"
        )))
        .await;
    if let Some(warning) = cadence_warning(poll_ms) {
        let _ = ev_tx.send(FeedEvent::Info(warning)).await;
    }

    // Binance spot feed (reconnect loop).
    if !binance_assets.is_empty() {
        let ev_tx = ev_tx.clone();
        let assets = binance_assets.clone();
        tokio::spawn(async move {
            binance_spot_loop(assets, ev_tx).await;
        });
    }

    FeedHandle {
        tx: ctl_tx,
        _ev: ev_tx,
    }
}

/// Handle for subscribing additional tokens at runtime (new rounds).
#[derive(Debug, Clone)]
pub struct FeedHandle {
    tx: mpsc::Sender<Vec<String>>,
    _ev: mpsc::Sender<FeedEvent>,
}

impl FeedHandle {
    /// Replace the Polymarket orderbook subscription set (new round tokens).
    pub async fn subscribe(&self, tokens: Vec<String>) {
        let _ = self.tx.send(tokens).await;
    }
}

impl blitzkrieg_market_api::SubscriptionControl for FeedHandle {
    fn set_tokens(&self, tokens: Vec<String>) {
        // Non-async control surface: `try_send` avoids blocking the host's tick.
        // The channel has capacity 16 and is only written on round rollovers.
        let _ = self.tx.try_send(tokens);
    }
}

// ── Polymarket orderbook via REST polling ───────────────────────────────────

/// Poll `POST /books` for the current token set.
///
/// Replaces the venue's WebSocket market channel; see [`DEFAULT_POLL_MS`] for
/// the measurements behind that. Two consequences are worth stating because they
/// are what make the trade acceptable:
///
/// * **Nothing is retained between rounds.** A REST fetch is stateless, so a
///   rollover is just a new token set — there is no subscription to release, no
///   refcount to keep balanced, and no socket that can outlive its round.
/// * **The engine's cadence assumptions still hold.** Trend confirmation
///   (`signal.rs`) gates on elapsed time (`spanned_ms >= window_ms * 0.9`) and
///   scores `above / total` over the samples inside that window. Polling at 1 s
///   yields a uniformly spaced sample series, so the ratio stays a faithful
///   time-weighted average of "is the mid above the threshold"; it is not a
///   sample-count gate that a lower rate could starve.
async fn poly_rest_loop(
    mut tokens: Vec<String>,
    ev_tx: mpsc::Sender<FeedEvent>,
    mut ctl_rx: mpsc::Receiver<Vec<String>>,
    host: String,
    base_ms: u64,
) {
    use polymarket_client_sdk_v2::clob::types::request::OrderBookSummaryRequest;
    use polymarket_client_sdk_v2::clob::{Client, Config};
    use polymarket_client_sdk_v2::types::U256;

    let client = match Client::new(&host, Config::default()) {
        Ok(c) => c,
        Err(e) => {
            let _ = ev_tx
                .send(FeedEvent::Info(format!("poly rest client: {e}")))
                .await;
            return;
        }
    };

    let mut interval_ms = base_ms;
    loop {
        // Parse the token set into ids. A set with nothing parseable in it is
        // treated exactly like an empty one: there is nothing to ask the venue
        // for, so the only correct move is to wait for a usable set instead of
        // spinning on requests that cannot be built.
        let ids: Vec<U256> = tokens
            .iter()
            .filter_map(|t| U256::from_str(t).ok())
            .collect();
        if ids.is_empty() {
            match ctl_rx.recv().await {
                Some(t) => tokens = t,
                None => return,
            }
            continue;
        }

        let requests: Vec<OrderBookSummaryRequest> = ids
            .iter()
            .map(|id| OrderBookSummaryRequest::builder().token_id(*id).build())
            .collect();

        match tokio::time::timeout(
            Duration::from_millis(REQUEST_TIMEOUT_MS),
            client.order_books(&requests),
        )
        .await
        {
            Ok(Ok(books)) => {
                interval_ms = base_ms; // recovered: back to the configured cadence
                for book in books {
                    let bids: Vec<(Decimal, Decimal)> =
                        book.bids.iter().map(|l| (l.price, l.size)).collect();
                    let asks: Vec<(Decimal, Decimal)> =
                        book.asks.iter().map(|l| (l.price, l.size)).collect();
                    // Stamped with the local clock, not the venue's `timestamp`.
                    // `fresh_book` compares a book's stamp against the engine's own
                    // clock, so stamping with the venue's would make this bot's
                    // ability to price anything depend on the venue's clock being
                    // aligned with ours. Staleness is already bounded by the poll
                    // cadence: if a poll fails or hangs, no event is emitted and the
                    // book correctly ages out on its own.
                    if ev_tx
                        .send(FeedEvent::Book {
                            token_id: book.asset_id.to_string(),
                            bids,
                            asks,
                            now_ms: now_ms(),
                        })
                        .await
                        .is_err()
                    {
                        return; // host is gone
                    }
                }
            }
            Ok(Err(e)) => {
                let _ = ev_tx
                    .send(FeedEvent::Info(format!("poly rest poll: {e}")))
                    .await;
                if is_rate_limited(&e) {
                    interval_ms = interval_ms.saturating_mul(2).min(MAX_POLL_MS);
                }
            }
            Err(_) => {
                let _ = ev_tx
                    .send(FeedEvent::Info(format!(
                        "poly rest poll timed out after {REQUEST_TIMEOUT_MS}ms ({} tokens)",
                        ids.len()
                    )))
                    .await;
            }
        }

        // Wait out the interval, but let a new token set cut the wait short: a
        // rollover should not sit on the previous round's cadence before its
        // first fetch.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {}
            update = ctl_rx.recv() => match update {
                Some(t) => tokens = t,
                None => return,
            },
        }
    }
}

// ── Binance spot via tokio-tungstenite ──────────────────────────────────────

async fn binance_spot_loop(assets: Vec<String>, ev_tx: mpsc::Sender<FeedEvent>) {
    use tokio_tungstenite::tungstenite::Message;

    let streams: Vec<String> = assets
        .iter()
        .map(|a| format!("{}@trade", a.to_lowercase() + "usdt"))
        .collect();
    let url = format!("wss://stream.binance.com:9443/ws/{}", streams.join("/"));

    loop {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => {
                let _ = ev_tx
                    .send(FeedEvent::Info("binance spot connected".into()))
                    .await;
                let (_write, mut read) = ws.split();
                while let Some(next) = read.next().await {
                    match next {
                        Ok(Message::Text(txt)) => {
                            if let Some((symbol, price)) = parse_binance_trade(&txt) {
                                let asset = symbol
                                    .trim_end_matches("USDT")
                                    .trim_end_matches("USD")
                                    .to_string();
                                let _ = ev_tx
                                    .send(FeedEvent::Spot {
                                        asset,
                                        price,
                                        now_ms: now_ms(),
                                    })
                                    .await;
                            }
                        }
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                        Ok(Message::Close(_)) => break,
                        Err(e) => {
                            let _ = ev_tx
                                .send(FeedEvent::Info(format!("binance ws error: {e}")))
                                .await;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                let _ = ev_tx
                    .send(FeedEvent::Info(format!("binance connect: {e}")))
                    .await;
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Parse a Binance combined `@trade` payload into (symbol, price).
/// Format: {"e":"trade","s":"BTCUSDT","p":"60000.00",...}
pub fn parse_binance_trade(text: &str) -> Option<(String, Decimal)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("e").and_then(|e| e.as_str()) != Some("trade") {
        return None;
    }
    let symbol = v.get("s")?.as_str()?.to_string();
    let price = Decimal::from_str(v.get("p")?.as_str()?).ok()?;
    Some((symbol, price))
}

/// The engine's freshness budget has to stay well clear of the poll interval,
/// or a perfectly healthy poller reads to the engine as a dead feed. Checked at
/// compile time so an edit to either constant fails the build instead of waiting
/// to be noticed as "the bot stopped trading".
///
/// This pins the *shipped defaults* only: an operator who lowers
/// `--max-orderbook-stale-ms` below the poll interval can still starve the engine
/// (by design — that is their stated intent), and [`cadence_warning`] says so at
/// startup rather than letting the build forbid it.
const _: () = {
    assert!(
        DEFAULT_POLL_MS * 4 <= ENGINE_MAX_ORDERBOOK_STALE_MS,
        "the default poll interval must leave at least a 4x margin under the engine's \
         staleness budget"
    );
    assert!(
        MIN_POLL_MS < DEFAULT_POLL_MS,
        "the floor must not override the default"
    );
    assert!(
        MAX_POLL_MS > DEFAULT_POLL_MS,
        "the ceiling must not override the default"
    );
    assert!(MIN_POLL_MS > 0, "zero would be a busy loop, not a floor");
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU16, Ordering};

    #[test]
    fn parses_binance_trade() {
        let msg = r#"{"e":"trade","s":"BTCUSDT","p":"60000.50","q":"0.01"}"#;
        let (sym, px) = parse_binance_trade(msg).unwrap();
        assert_eq!(sym, "BTCUSDT");
        assert_eq!(px, Decimal::from_str("60000.50").unwrap());
    }

    #[test]
    fn ignores_non_trade_messages() {
        assert!(parse_binance_trade(r#"{"e":"24hrTicker","s":"BTCUSDT"}"#).is_none());
        assert!(parse_binance_trade("not json").is_none());
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    /// A venue-shaped token id: 77 decimal digits, as the CLOB and Gamma both
    /// use.
    ///
    /// Deliberately *not* the zero-padded hex form. The SDK parses token ids into
    /// `U256` and re-serializes them canonically as decimal, so a padded or
    /// hex-form id would be echoed back in a different form than it was sent —
    /// which is exactly the class of mismatch that would silently key a book
    /// under an id the engine never looks up. Real ids already round-trip, and
    /// this shape keeps that property under test.
    fn token(n: u8) -> String {
        let base = "2174263314346390629056905015582624153306727273689761495048815684794993883645";
        format!("{base}{n}")
    }

    /// Depth as the feed forwards it: the bids and asks for one token.
    type Depth = (Vec<(Decimal, Decimal)>, Vec<(Decimal, Decimal)>);

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn content_length(head: &str) -> usize {
        head.lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if !k.eq_ignore_ascii_case("content-length") {
                    return None;
                }
                v.trim().parse().ok()
            })
            .unwrap_or(0)
    }

    /// One `POST /books` response body: an array of book summaries, one per
    /// requested token, in the wire shape the real CLOB returns.
    fn books_json(ids: &[String]) -> String {
        let books: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "market": "0x0000000000000000000000000000000000000000000000000000000000000001",
                    "asset_id": id,
                    "timestamp": "1700000000000",
                    "hash": "deadbeef",
                    "bids": [{"price": "0.40", "size": "10"}],
                    "asks": [{"price": "0.50", "size": "5"}],
                    "min_order_size": "5",
                    "neg_risk": true,
                    "tick_size": "0.01",
                    "last_trade_price": "0.45"
                })
            })
            .collect();
        serde_json::to_string(&books).expect("serialize books")
    }

    /// A throwaway HTTP/1.1 server that answers `POST /books` and records the
    /// token ids each request asked for.
    ///
    /// Hand-rolled rather than pulling in a test HTTP framework: the contract
    /// under test is just "POST an array of token ids, get an array of books
    /// back", and the SDK client takes its host as a parameter, so a small
    /// responder keeps this test dependency-free.
    struct FakeClob {
        url: String,
        requests: Arc<tokio::sync::Mutex<Vec<Vec<String>>>>,
        _stop: tokio::sync::oneshot::Sender<()>,
    }

    impl FakeClob {
        async fn start(status: u16) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let status = Arc::new(AtomicU16::new(status));
            let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
            let srv_requests = Arc::clone(&requests);
            let srv_status = Arc::clone(&status);

            tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        a = listener.accept() => a,
                        _ = &mut stop_rx => break,
                    };
                    let Ok((mut tcp, _)) = accepted else { break };
                    let reqs = Arc::clone(&srv_requests);
                    let status = Arc::clone(&srv_status);
                    tokio::spawn(async move {
                        // Headers first, then exactly `Content-Length` body bytes:
                        // the body follows the headers on the same connection and
                        // is not guaranteed to arrive in the same read.
                        let mut buf: Vec<u8> = Vec::new();
                        let mut tmp = [0u8; 4096];
                        let body = loop {
                            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                                let len = content_length(&head);
                                if buf.len() >= pos + 4 + len {
                                    break buf[pos + 4..pos + 4 + len].to_vec();
                                }
                            }
                            match tcp.read(&mut tmp).await {
                                Ok(0) | Err(_) => break Vec::new(),
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        };
                        let ids: Vec<String> =
                            serde_json::from_slice::<Vec<serde_json::Value>>(&body)
                                .map(|v| {
                                    v.iter()
                                        .filter_map(|e| {
                                            e.get("token_id")
                                                .and_then(|t| t.as_str())
                                                .map(str::to_string)
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                        reqs.lock().await.push(ids.clone());

                        let code = status.load(Ordering::SeqCst);
                        let (reason, payload) = if code == 200 {
                            ("200 OK", books_json(&ids))
                        } else {
                            (
                                "429 Too Many Requests",
                                String::from("{\"error\":\"rate limited\"}"),
                            )
                        };
                        let resp = format!(
                            "HTTP/1.1 {reason}\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        );
                        let _ = tcp.write_all(resp.as_bytes()).await;
                        let _ = tcp.flush().await;
                    });
                }
            });

            Self {
                url: format!("http://{addr}"),
                requests,
                _stop: stop_tx,
            }
        }

        async fn seen(&self) -> Vec<Vec<String>> {
            self.requests.lock().await.clone()
        }

        /// Wait until at least `n` requests have arrived, then return all of them.
        async fn wait_for_requests(&self, n: usize) -> Vec<Vec<String>> {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let seen = self.seen().await;
                if seen.len() >= n {
                    return seen;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "only {} poll(s) arrived within 5s, wanted {n}: {seen:?}",
                    seen.len()
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    /// A poll must deliver a book per subscribed token, with the venue's depth
    /// intact: the REST response replaces the book wholesale, so dropping levels
    /// here would silently narrow what every strategy sees.
    #[tokio::test]
    async fn poll_delivers_a_book_for_every_subscribed_token() {
        let srv = FakeClob::start(200).await;
        let a = token(1);
        let b = token(2);
        let (ev_tx, mut ev_rx) = mpsc::channel::<FeedEvent>(16);
        let (_ctl_tx, ctl_rx) = mpsc::channel::<Vec<String>>(4);
        let handle = tokio::spawn(poly_rest_loop(
            vec![a.clone(), b.clone()],
            ev_tx,
            ctl_rx,
            srv.url.clone(),
            250,
        ));

        let mut seen: HashMap<String, Depth> = HashMap::new();
        while seen.len() < 2 {
            let ev = tokio::time::timeout(Duration::from_secs(5), ev_rx.recv())
                .await
                .expect("no book arrived within 5s")
                .expect("feed closed");
            if let FeedEvent::Book {
                token_id,
                bids,
                asks,
                ..
            } = ev
            {
                seen.insert(token_id, (bids, asks));
            }
        }
        handle.abort();

        let (bids, asks) = seen.get(&a).expect("no book for the first token");
        assert_eq!(bids, &vec![(dec("0.40"), dec("10"))]);
        assert_eq!(asks, &vec![(dec("0.50"), dec("5"))]);
        assert!(seen.contains_key(&b), "no book for the second token");
    }

    /// A round rollover must switch the polled set over, and must not keep asking
    /// for the expired round's tokens.
    #[tokio::test]
    async fn rollover_polls_only_the_new_token_set() {
        let srv = FakeClob::start(200).await;
        let a = token(1);
        let b = token(2);
        let (ev_tx, _ev_rx) = mpsc::channel::<FeedEvent>(64);
        let (ctl_tx, ctl_rx) = mpsc::channel::<Vec<String>>(4);
        let handle = tokio::spawn(poly_rest_loop(
            vec![a.clone()],
            ev_tx,
            ctl_rx,
            srv.url.clone(),
            250,
        ));

        let first = srv.wait_for_requests(1).await;
        assert_eq!(
            first[0],
            vec![a.clone()],
            "the first poll must use the first token set"
        );

        ctl_tx.send(vec![b.clone()]).await.expect("send rollover");
        let after = srv.wait_for_requests(2).await;
        handle.abort();

        let latest = after.last().expect("at least one later poll");
        assert!(
            latest.contains(&b),
            "the new round must be polled; polls={after:?}"
        );
        assert!(
            !latest.contains(&a),
            "the previous round must stop being polled; polls={after:?}"
        );
    }

    /// A token set with nothing parseable in it must not become a request loop.
    /// `FeedEvent` cannot express "invalid", so the failure mode would otherwise
    /// be a busy loop against the venue at whatever rate the CPU allows.
    #[tokio::test]
    async fn an_unparseable_token_set_waits_instead_of_spinning() {
        let srv = FakeClob::start(200).await;
        let (ev_tx, _ev_rx) = mpsc::channel::<FeedEvent>(64);
        let (_ctl_tx, ctl_rx) = mpsc::channel::<Vec<String>>(4);
        let handle = tokio::spawn(poly_rest_loop(
            vec!["not-a-token".to_string()],
            ev_tx,
            ctl_rx,
            srv.url.clone(),
            250,
        ));

        tokio::time::sleep(Duration::from_millis(400)).await;
        let polls = srv.seen().await.len();
        handle.abort();
        assert_eq!(
            polls, 0,
            "a set with no parseable token id must not be polled at all"
        );
    }

    /// A 429 must reduce the call rate rather than retry into the limit.
    ///
    /// The venue advertised no rate-limit headers and answered 15 consecutive
    /// 1/second calls with 200, so this path guards a limit that has not been
    /// observed rather than one that has.
    #[tokio::test]
    async fn a_rate_limited_poll_backs_off() {
        let srv = FakeClob::start(429).await;
        let (ev_tx, mut ev_rx) = mpsc::channel::<FeedEvent>(64);
        let (_ctl_tx, ctl_rx) = mpsc::channel::<Vec<String>>(4);
        let handle = tokio::spawn(poly_rest_loop(
            vec![token(1)],
            ev_tx,
            ctl_rx,
            srv.url.clone(),
            MIN_POLL_MS,
        ));

        tokio::time::sleep(Duration::from_millis(900)).await;
        let polls = srv.seen().await.len();
        handle.abort();

        // Doubling from the 250 ms floor admits the immediate poll plus one more
        // at ~500 ms; a loop that ignored 429 would have issued ~4 by now.
        assert!(
            polls <= 2,
            "429 must reduce the poll rate; {polls} polls in 900ms"
        );
        assert!(
            polls >= 1,
            "the poller must keep trying rather than give up"
        );
        assert!(
            !ev_rx
                .try_recv()
                .is_ok_and(|e| matches!(e, FeedEvent::Book { .. })),
            "a rate-limited response must not produce a book"
        );
    }

    #[test]
    fn poll_ms_falls_back_to_the_default() {
        // Every "I did not really set this" shape must land on the default, so a
        // typo cannot become a busy loop.
        assert_eq!(parse_poll_ms(None), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("  ")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("0")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("-1")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("1s")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("1000ms")), DEFAULT_POLL_MS);
    }

    #[test]
    fn poll_ms_is_clamped_into_the_supported_band() {
        assert_eq!(parse_poll_ms(Some("1")), MIN_POLL_MS);
        assert_eq!(parse_poll_ms(Some("100")), MIN_POLL_MS);
        assert_eq!(parse_poll_ms(Some(" 2000 ")), 2_000);
        assert_eq!(parse_poll_ms(Some("9999999")), MAX_POLL_MS);
    }

    /// The default cadence must not trip its own warning: a shipped default that
    /// logs a warning on every start is a warning nobody reads.
    #[test]
    fn the_default_cadence_is_inside_the_engines_staleness_budget() {
        let budget = Some(ENGINE_MAX_ORDERBOOK_STALE_MS);
        assert_eq!(cadence_warning_with(DEFAULT_POLL_MS, budget), None);
        assert_eq!(cadence_warning_with(MIN_POLL_MS, budget), None);
    }

    /// A cadence that would leave the engine unpricable between polls must be
    /// reported, including one that is only *just* over once latency is added.
    #[test]
    fn a_cadence_past_the_staleness_budget_is_reported() {
        let budget = ENGINE_MAX_ORDERBOOK_STALE_MS;
        // Exactly at the boundary, with the latency allowance folded in, is
        // already too slow.
        let boundary = budget - LATENCY_ALLOWANCE_MS;
        assert_eq!(
            cadence_warning_with(boundary, Some(budget)),
            None,
            "the boundary itself is still fine"
        );
        assert!(
            cadence_warning_with(boundary + 1, Some(budget)).is_some(),
            "one millisecond past the boundary leaves no fresh book and must warn"
        );
        assert!(cadence_warning_with(budget, Some(budget)).is_some());
        assert!(cadence_warning_with(MAX_POLL_MS, Some(budget)).is_some());
    }

    /// Issue #205: the warning has to track the budget in force, not the compiled
    /// one. The same poll interval is fine under a widened budget and must warn
    /// under a tightened one — otherwise the operator who tightens the knob gets
    /// the "bot stopped trading" mystery the guard exists to explain.
    #[test]
    fn the_cadence_warning_tracks_the_configured_budget() {
        let poll = DEFAULT_POLL_MS;
        assert_eq!(
            cadence_warning_with(poll, Some(ENGINE_MAX_ORDERBOOK_STALE_MS)),
            None
        );
        assert_eq!(cadence_warning_with(poll, Some(60_000)), None);
        let tight = poll + LATENCY_ALLOWANCE_MS - 1;
        assert!(
            cadence_warning_with(poll, Some(tight)).is_some(),
            "a budget the poll cycle cannot meet must warn"
        );
        // `--max-orderbook-stale-ms 0` disables the check: no budget, nothing to
        // starve, so no warning even at the slowest cadence.
        assert_eq!(cadence_warning_with(MAX_POLL_MS, None), None);
    }

    /// `parse_stale_budget` has to agree with what the kernel lets through: the
    /// values it rejects at startup (exit 2) can never be the budget in force, so
    /// they fall back to the compiled default, and `0` means "check off".
    #[test]
    fn the_stale_budget_reader_agrees_with_the_kernel() {
        assert_eq!(
            parse_stale_budget(None),
            Some(ENGINE_MAX_ORDERBOOK_STALE_MS)
        );
        assert_eq!(
            parse_stale_budget(Some("")),
            Some(ENGINE_MAX_ORDERBOOK_STALE_MS)
        );
        assert_eq!(
            parse_stale_budget(Some("  ")),
            Some(ENGINE_MAX_ORDERBOOK_STALE_MS)
        );
        assert_eq!(parse_stale_budget(Some("1000")), Some(1_000));
        assert_eq!(parse_stale_budget(Some(" 15000 ")), Some(15_000));
        assert_eq!(parse_stale_budget(Some("0")), None);
        // Rejected by the kernel, so never the value in force.
        assert_eq!(
            parse_stale_budget(Some("-1")),
            Some(ENGINE_MAX_ORDERBOOK_STALE_MS)
        );
        assert_eq!(
            parse_stale_budget(Some("8s")),
            Some(ENGINE_MAX_ORDERBOOK_STALE_MS)
        );
        assert_eq!(
            parse_stale_budget(Some("600001")),
            Some(ENGINE_MAX_ORDERBOOK_STALE_MS)
        );
        assert_eq!(parse_stale_budget(Some("600000")), Some(600_000));
    }

    /// The ceiling has to keep the knob's own bounds honest: a max below the
    /// default would make the clamp silently rewrite the default. The constants
    /// themselves are checked in the compile-time block above; this exercises
    /// the clamp that depends on them.
    #[test]
    fn the_poll_band_does_not_rewrite_the_default() {
        assert_eq!(
            parse_poll_ms(Some(&DEFAULT_POLL_MS.to_string())),
            DEFAULT_POLL_MS
        );
    }
}
