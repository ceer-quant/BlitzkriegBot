//! Feed layer — Rust-native market data ingestion (P4).
//!
//! Two sources, both with reconnect:
//!   - Polymarket orderbook: the SDK `clob::ws` client streams `BookUpdate`
//!     snapshots and `PriceChange`/`BestBidAsk` top-of-book updates for the
//!     subscribed token ids.
//!   - Binance spot: a raw `tokio-tungstenite` connection to the combined
//!     `@trade` stream, parsed into per-asset spot ticks for the momentum filter.
//!
//! Everything is translated into `MarketHost` calls (the market seam), so this
//! module no longer touches `Core` directly and can move into a market extension
//! unchanged. Node no longer has to push `books.*` / `spot.price`; those IPC
//! methods remain for tests and as a manual override.

use blitzkrieg_market_api::{BookUpdate, MarketHost, SpotUpdate, TopOfBookUpdate};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Heartbeat (ms) used when top-of-book coalescing is enabled, i.e. when
/// `POLYMARKET_TOP_HEARTBEAT_MS` is set to a positive value. Unset / `0` leaves
/// the feed forwarding every quote, exactly as before this coalescer existed.
///
/// The 1 s figure is anchored to the engine's own tolerance,
/// `max_orderbook_stale_ms` (8 s, `engine.rs`): one beacon per second leaves an
/// 8× margin before the engine would treat a quiet book as unpricable.
///
/// Enabling this is NOT behaviour-preserving and is therefore off by default.
/// The engine's trend ratio (`above / total`, `signal.rs`) and `PriceBuffer::mean`
/// (`sum / n`) are both weighted by SAMPLE COUNT, so withholding repeated quotes
/// changes them toward duration weighting. Every real price change still passes
/// through untouched; only the no-op repeats are collapsed. Validate against a
/// shadow/backtest comparison before running it live.
const TOP_HEARTBEAT_MS_RECOMMENDED: i64 = 1_000;

/// Resolve the `POLYMARKET_TOP_HEARTBEAT_MS` setting. `None`, `0`, empty and
/// unparseable values all mean "disabled" (forward every quote), so a typo can
/// only ever fall back to the pre-coalescer behaviour, never to a tiny
/// heartbeat that would starve the engine. The literal `recommended` opts into
/// [`TOP_HEARTBEAT_MS_RECOMMENDED`] so an operator does not have to remember
/// the number or risk picking one that outruns the engine's staleness rule.
fn parse_heartbeat(raw: Option<&str>) -> i64 {
    match raw.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("recommended") => TOP_HEARTBEAT_MS_RECOMMENDED,
        Some(v) => v.parse().ok().filter(|ms| *ms > 0).unwrap_or(0),
        None => 0,
    }
}

/// Market-data event produced by the feed loops, consumed by the pump which
/// forwards each one onto the [`MarketHost`].
#[derive(Debug, Clone)]
pub enum FeedEvent {
    Book { token_id: String, bids: Vec<(Decimal, Decimal)>, asks: Vec<(Decimal, Decimal)>, now_ms: i64 },
    TopOfBook { token_id: String, best_bid: Option<Decimal>, best_ask: Option<Decimal>, now_ms: i64 },
    Spot { asset: String, price: Decimal, now_ms: i64 },
    Info(String),
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
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
                    FeedEvent::Book { token_id, bids, asks, now_ms } => {
                        host.on_book(BookUpdate { token_id, bids, asks, ts_ms: now_ms }).await;
                    }
                    FeedEvent::TopOfBook { token_id, best_bid, best_ask, now_ms } => {
                        host.on_top_of_book(TopOfBookUpdate { token_id, best_bid, best_ask, ts_ms: now_ms }).await;
                    }
                    FeedEvent::Spot { asset, price, now_ms } => {
                        host.on_spot(SpotUpdate { asset, price, ts_ms: now_ms }).await;
                    }
                    FeedEvent::Info(msg) => {
                        tracing::info!(feed = %msg, "feed");
                    }
                }
            }
        });
    }

    let (ctl_tx, ctl_rx) = mpsc::channel::<Vec<String>>(16);

    // Polymarket orderbook feed (reconnect + dynamic re-subscribe on new rounds).
    // Resolved here rather than inside the loop so the loop takes its endpoint as a
    // plain argument and can be exercised against a local server in tests.
    let ws_url = std::env::var("POLYMARKET_WS_URL")
        .unwrap_or_else(|_| "wss://ws-subscriptions-clob.polymarket.com".into());
    {
        let ev_tx = ev_tx.clone();
        let tokens = token_ids.clone();
        let ws_url = ws_url.clone();
        tokio::spawn(async move {
            poly_orderbook_loop(tokens, ev_tx, ctl_rx, ws_url).await;
        });
    }

    // Binance spot feed (reconnect loop).
    if !binance_assets.is_empty() {
        let ev_tx = ev_tx.clone();
        let assets = binance_assets.clone();
        tokio::spawn(async move {
            binance_spot_loop(assets, ev_tx).await;
        });
    }

    FeedHandle { tx: ctl_tx, _ev: ev_tx }
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

// ── Polymarket orderbook via the SDK ws client ──────────────────────────────

/// Collapses the venue's incremental top-of-book stream before it reaches the
/// engine.
///
/// Polymarket emits a `price_change` entry for every book-level edit, and almost
/// all of them leave the best bid/ask exactly where they were. Measured on a 2.0M
/// event archive segment: 52,478 real quote moves against 1,737,517 no-ops (97.1%).
/// Mirroring that flood cost 16 GB/day of archive and a full per-tick strategy pass
/// per event, for no informational gain.
///
/// A no-op is not dropped outright, because the engine stamps book freshness from
/// each event's `now_ms` (`LocalBook::update_top`) and refuses to price anything
/// older than `max_orderbook_stale_ms`. Pure silence would therefore read as a dead
/// feed and freeze a genuinely quiet market out of trading. So the rule is:
///
/// * a real change is forwarded immediately — never delayed, never coalesced away;
/// * an unchanged quote is forwarded at most once per `heartbeat_ms`, purely as a
///   liveness beacon that keeps the book fresh.
///
/// Set `heartbeat_ms <= 0` (the default) to disable and forward everything.
struct TopCoalescer {
    heartbeat_ms: i64,
    /// Last forwarded quote per token; `None` means "nothing forwarded yet".
    last: HashMap<String, (Option<Decimal>, Option<Decimal>)>,
    /// When the last event for a token was forwarded.
    sent_at: HashMap<String, i64>,
    forwarded: u64,
    suppressed: u64,
}

impl TopCoalescer {
    fn new(heartbeat_ms: i64) -> Self {
        Self {
            heartbeat_ms,
            last: HashMap::new(),
            sent_at: HashMap::new(),
            forwarded: 0,
            suppressed: 0,
        }
    }

    /// Decide whether this quote should reach the engine. Records the outcome.
    fn admit(&mut self, token_id: &str, bid: Option<Decimal>, ask: Option<Decimal>, now_ms: i64) -> bool {
        if self.heartbeat_ms <= 0 {
            self.forwarded += 1;
            return true;
        }
        let quote = (bid, ask);
        let changed = self.last.get(token_id) != Some(&quote);
        let due = match self.sent_at.get(token_id) {
            Some(t) => now_ms.saturating_sub(*t) >= self.heartbeat_ms,
            None => true,
        };
        if changed || due {
            self.last.insert(token_id.to_string(), quote);
            self.sent_at.insert(token_id.to_string(), now_ms);
            self.forwarded += 1;
            true
        } else {
            self.suppressed += 1;
            false
        }
    }

    /// Forget tokens that are no longer subscribed, so a long capture cannot grow
    /// this map one dead round at a time.
    fn retain_tokens(&mut self, tokens: &[String]) {
        self.last.retain(|t, _| tokens.iter().any(|k| k == t));
        self.sent_at.retain(|t, _| tokens.iter().any(|k| k == t));
    }

    /// `(forwarded, suppressed)` totals since construction.
    fn totals(&self) -> (u64, u64) {
        (self.forwarded, self.suppressed)
    }
}

async fn poly_orderbook_loop(
    mut tokens: Vec<String>,
    ev_tx: mpsc::Sender<FeedEvent>,
    mut ctl_rx: mpsc::Receiver<Vec<String>>,
    ws_url: String,
) {
    use polymarket_client_sdk_v2::clob::ws::Client as WsClient;
    use polymarket_client_sdk_v2::types::U256;
    use polymarket_client_sdk_v2::ws::config::Config as WsConfig;

    // 0 / unset disables coalescing and forwards every quote (default: the
    // pre-coalescer behaviour).
    let heartbeat_ms = parse_heartbeat(std::env::var("POLYMARKET_TOP_HEARTBEAT_MS").ok().as_deref());
    let mut coalescer = TopCoalescer::new(heartbeat_ms);
    if heartbeat_ms > 0 {
        let _ = ev_tx
            .send(FeedEvent::Info(format!("poly top coalescer enabled, heartbeat {heartbeat_ms}ms")))
            .await;
    }

    /// Release the refcounts this feed holds on `ids`.
    ///
    /// `subscribe_orderbook` and `subscribe_prices` each take a reference on the
    /// same asset — the SDK multiplexes both onto one market channel — and it only
    /// sends the server-side unsubscribe once the count reaches zero. So one
    /// release call per subscription is required to actually stop the venue.
    fn release_tokens(client: &WsClient, ids: &[U256]) {
        if ids.is_empty() {
            return;
        }
        let _ = client.unsubscribe_orderbook(ids);
        let _ = client.unsubscribe_prices(ids);
    }

    // One client for the process lifetime. Building a fresh `WsClient` per round
    // (as this loop used to) leaks the previous socket: the SDK has no `Drop` on
    // its connection, and its read half stays selectable, so the abandoned task
    // keeps reading while the venue keeps pushing that round's tokens to a socket
    // nothing consumes. Every rollover therefore added a subscription that streamed
    // forever, which is how ~13 GB/day of archived data becomes a wire bill many
    // times larger as the day goes on.
    let mut client: Option<WsClient> = None;
    // Tokens this feed currently holds on `client`. Released as soon as their
    // replacement is subscribed, so exactly one round is ever live on the wire and
    // the refcounts cannot drift upward across reconnect attempts.
    let mut held: Vec<U256> = Vec::new();

    loop {
        let ids: Vec<U256> = tokens.iter().filter_map(|t| U256::from_str(t).ok()).collect();
        if ids.is_empty() {
            // No tokens yet: wait for a subscription update (new round).
            match ctl_rx.recv().await {
                Some(t) => tokens = t,
                None => return,
            }
            continue;
        }
        if client.is_none() {
            match WsClient::new(&ws_url, WsConfig::default()) {
                Ok(c) => client = Some(c),
                Err(e) => {
                    let _ = ev_tx.send(FeedEvent::Info(format!("poly ws client: {e}"))).await;
                    if !wait_or_update(&mut tokens, &mut ctl_rx, 5).await {
                        return;
                    }
                    continue;
                }
            }
        }
        let client = client.as_ref().expect("client created above");
        // Polymarket's market channel sends ONE full `book` snapshot on subscribe,
        // then only `price_change` incrementals. The SDK's `subscribe_orderbook`
        // filters those incrementals out (`_ => None`), so relying on it alone left
        // the local book frozen except on the rare re-snapshot — illiquid tokens
        // (XRP/SOL) could sit 20-60s stale while BTC updated, which is what made the
        // panel's price look stuck. `subscribe_prices` carries the same channel's
        // `price_change` events (each entry has best_bid/best_ask), which we apply
        // as top-of-book updates. Both subscriptions share one MARKET channel and
        // refcount their assets, so this does not disturb the book stream.
        // Claim the new round before releasing the outgoing one. Subscribing first
        // keeps the market channel non-empty across the rollover, so the channel is
        // not torn down and rebuilt (a fresh TLS handshake) every 15 minutes; on the
        // same-set path after an error it also means no subscribe/unsubscribe pair
        // goes out at all, since the refcount simply returns to where it started.
        let stream = match client.subscribe_orderbook(ids.clone()) {
            Ok(s) => s,
            Err(e) => {
                let _ = ev_tx.send(FeedEvent::Info(format!("poly subscribe: {e}"))).await;
                if !wait_or_update(&mut tokens, &mut ctl_rx, 5).await {
                    return;
                }
                continue;
            }
        };
        let price_stream = match client.subscribe_prices(ids.clone()) {
            Ok(s) => s,
            Err(e) => {
                // Undo the reference `subscribe_orderbook` just took. `held` still
                // describes the round actually live on the wire, so it is untouched.
                let _ = client.unsubscribe_orderbook(&ids);
                let _ = ev_tx.send(FeedEvent::Info(format!("poly price subscribe: {e}"))).await;
                if !wait_or_update(&mut tokens, &mut ctl_rx, 5).await {
                    return;
                }
                continue;
            }
        };
        let mut stream = Box::pin(stream);
        let mut price_stream = Box::pin(price_stream);
        // The new round is live, so the previous one can safely go.
        release_tokens(client, &held);
        held = ids.clone();
        let _ = ev_tx
            .send(FeedEvent::Info(format!("poly orderbook+price subscribed {} tokens", ids.len())))
            .await;

        loop {
            tokio::select! {
                item = stream.next() => match item {
                    Some(Ok(book)) => {
                        let now = now_ms();
                        let bids: Vec<(Decimal, Decimal)> = book.bids.iter().map(|l| (l.price, l.size)).collect();
                        let asks: Vec<(Decimal, Decimal)> = book.asks.iter().map(|l| (l.price, l.size)).collect();
                        let _ = ev_tx.send(FeedEvent::Book {
                            token_id: book.asset_id.to_string(),
                            bids,
                            asks,
                            now_ms: if book.timestamp > 0 { book.timestamp } else { now },
                        }).await;
                    }
                    Some(Err(e)) => {
                        let _ = ev_tx.send(FeedEvent::Info(format!("poly ws error: {e}"))).await;
                        break; // reconnect
                    }
                    None => break,
                },
                item = price_stream.next() => match item {
                    Some(Ok(pc)) => {
                        let now = now_ms();
                        // Collect first so we don't hold a borrow across the sends.
                        let updates: Vec<(String, Option<Decimal>, Option<Decimal>)> = pc
                            .price_changes
                            .iter()
                            .filter(|c| c.best_bid.is_some() || c.best_ask.is_some())
                            .map(|c| (c.asset_id.to_string(), c.best_bid, c.best_ask))
                            .collect();
                        for (token_id, best_bid, best_ask) in updates {
                            // Drop the no-op majority here rather than downstream, so
                            // neither the archive nor the strategy pass pays for it.
                            if !coalescer.admit(&token_id, best_bid, best_ask, now) {
                                continue;
                            }
                            let _ = ev_tx.send(FeedEvent::TopOfBook {
                                token_id,
                                best_bid,
                                best_ask,
                                now_ms: now,
                            }).await;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = ev_tx.send(FeedEvent::Info(format!("poly price ws error: {e}"))).await;
                        break; // reconnect
                    }
                    None => break,
                },
                update = ctl_rx.recv() => match update {
                    Some(t) => {
                        if t.is_empty() {
                            release_tokens(client, &held);
                            held.clear();
                            tokens.clear();
                            return;
                        }
                        // Reported per round rollover (~15 min): the only honest way
                        // to see what the coalescer actually withheld. When it is
                        // disabled the counters are structurally zero, so say that
                        // rather than print "0.0% withheld" as if it were a result.
                        let detail = if heartbeat_ms > 0 {
                            let (forwarded, suppressed) = coalescer.totals();
                            let total = forwarded + suppressed;
                            let pct = if total > 0 { suppressed as f64 * 100.0 / total as f64 } else { 0.0 };
                            format!(", top coalesced {forwarded} forwarded / {suppressed} suppressed ({pct:.1}% withheld)")
                        } else {
                            String::from(", top coalescer disabled (POLYMARKET_TOP_HEARTBEAT_MS unset)")
                        };
                        let _ = ev_tx.send(FeedEvent::Info(format!(
                            "poly subscription updated: {} tokens{detail}",
                            t.len()
                        ))).await;
                        coalescer.retain_tokens(&t);
                        tokens = t;
                        break; // resubscribe with the new set
                    }
                    None => return,
                },
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Sleep up to `secs`, but return early (with updated tokens) if a control
/// message arrives. Returns false if the control channel closed.
async fn wait_or_update(
    tokens: &mut Vec<String>,
    ctl_rx: &mut mpsc::Receiver<Vec<String>>,
    secs: u64,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(secs)) => true,
        update = ctl_rx.recv() => match update {
            Some(t) => { *tokens = t; true }
            None => false,
        },
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
                let _ = ev_tx.send(FeedEvent::Info("binance spot connected".into())).await;
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
                                    .send(FeedEvent::Spot { asset, price, now_ms: now_ms() })
                                    .await;
                            }
                        }
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                        Ok(Message::Close(_)) => break,
                        Err(e) => {
                            let _ = ev_tx.send(FeedEvent::Info(format!("binance ws error: {e}"))).await;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                let _ = ev_tx.send(FeedEvent::Info(format!("binance connect: {e}"))).await;
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Stand up a throwaway WebSocket server that records every text frame a
    /// client sends it, and answer "PING" so the SDK's heartbeat stays happy.
    /// Returns (url, received-frames handle, shutdown).
    async fn recording_ws_server() -> (
        String,
        Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let seen_srv = Arc::clone(&seen);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    a = listener.accept() => a,
                    _ = &mut stop_rx => break,
                };
                let Ok((tcp, _)) = accepted else { break };
                let seen_conn = Arc::clone(&seen_srv);
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else { return };
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            Message::Text(t) => {
                                if t == "PING" {
                                    let _ = ws.send(Message::Text("PONG".into())).await;
                                    continue;
                                }
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                                    seen_conn.lock().await.push(v);
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });

        (format!("ws://{addr}"), seen, stop_tx)
    }

    /// A rollover must not leave the previous round streaming. The regression this
    /// guards is a leaked socket: the loop used to build a fresh client per round,
    /// and the SDK has no `Drop`, so the old connection stayed subscribed forever.
    /// Here we assert the wire itself — after moving to round B, round A's assets
    /// must have been unsubscribed, so nothing from A keeps arriving.
    #[tokio::test]
    async fn rollover_unsubscribes_the_previous_round_on_the_wire() {
        let (url, seen, stop) = recording_ws_server().await;

        let a = vec!["111111111111111111111111111111111111111111111111111111111111111111".to_string()];
        let b = vec!["222222222222222222222222222222222222222222222222222222222222222222".to_string()];

        let (ev_tx, _ev_rx) = mpsc::channel::<FeedEvent>(16);
        let (ctl_tx, ctl_rx) = mpsc::channel::<Vec<String>>(16);
        let url_clone = url.clone();
        let a_for_loop = a.clone();
        let handle = tokio::spawn(async move {
            poly_orderbook_loop(a_for_loop, ev_tx, ctl_rx, url_clone).await;
        });

        // Give the first round time to subscribe, then move to the second round.
        tokio::time::sleep(Duration::from_millis(600)).await;
        ctl_tx.send(b.clone()).await.expect("send rollover");
        // The loop backs off 5s after a rollover before it subscribes the next
        // round, so the window has to clear that sleep plus the reconnect.
        tokio::time::sleep(Duration::from_millis(7_000)).await;

        let frames = seen.lock().await.clone();
        handle.abort();
        let _ = stop.send(());

        let subscribes: Vec<Vec<String>> = frames
            .iter()
            .filter(|f| f.get("operation").and_then(|o| o.as_str()) == Some("subscribe"))
            .filter_map(|f| f.get("assets_ids").and_then(|a| a.as_array()))
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .collect();
        let unsubscribes: Vec<Vec<String>> = frames
            .iter()
            .filter(|f| f.get("operation").and_then(|o| o.as_str()) == Some("unsubscribe"))
            .filter_map(|f| f.get("assets_ids").and_then(|a| a.as_array()))
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .collect();

        assert!(
            subscribes.iter().any(|s| s.contains(&b[0])),
            "the new round must be subscribed; frames={frames:?}"
        );
        assert!(
            unsubscribes.iter().any(|u| u.contains(&a[0])),
            "the previous round must be released on the wire, otherwise it streams \
             forever; frames={frames:?}"
        );
    }

    #[test]
    fn first_quote_is_always_forwarded() {
        let mut c = TopCoalescer::new(1_000);
        assert!(c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 0));
        assert_eq!(c.totals(), (1, 0));
    }

    #[test]
    fn repeated_quote_is_withheld_until_heartbeat() {
        let mut c = TopCoalescer::new(1_000);
        assert!(c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 0));
        // Same quote at +10ms and +999ms: withheld (both inside the heartbeat).
        assert!(!c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 10));
        assert!(!c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 999));
        // The beacon is due at the heartbeat boundary, so a quiet market still
        // refreshes the engine's staleness clock.
        assert!(c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 1_000));
        assert_eq!(c.totals(), (2, 2));
    }

    #[test]
    fn a_real_change_is_never_delayed_by_the_heartbeat() {
        let mut c = TopCoalescer::new(60_000);
        assert!(c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 0));
        // 1ms later, inside the heartbeat window, but the quote moved: forward now.
        assert!(c.admit("t1", Some(dec("0.41")), Some(dec("0.5")), 1));
        assert!(c.admit("t1", Some(dec("0.41")), Some(dec("0.52")), 2));
        assert!(c.admit("t1", None, Some(dec("0.52")), 3));
        assert_eq!(c.totals(), (4, 0));
    }

    #[test]
    fn tokens_are_isolated_from_each_other() {
        let mut c = TopCoalescer::new(1_000);
        assert!(c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 0));
        // A different token's first quote must not be suppressed by t1's history.
        assert!(c.admit("t2", Some(dec("0.4")), Some(dec("0.5")), 1));
        assert!(!c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 2));
    }

    /// The engine refuses to price a book older than `max_orderbook_stale_ms`
    /// (8 s, `engine.rs`). Coalescing is only safe while the beacon is far more
    /// frequent than that, or a quiet market reads as a dead feed and gets frozen
    /// out of trading. Checked at compile time, so an edit to either constant that
    /// breaks the margin fails the build rather than waiting on a test run.
    const _: () = {
        const ENGINE_MAX_ORDERBOOK_STALE_MS: i64 = 8_000;
        assert!(
            TOP_HEARTBEAT_MS_RECOMMENDED > 0,
            "heartbeat 0 means 'disabled', not a recommendation"
        );
        assert!(
            TOP_HEARTBEAT_MS_RECOMMENDED * 4 <= ENGINE_MAX_ORDERBOOK_STALE_MS,
            "recommended heartbeat must leave at least a 4x margin"
        );
    };

    #[test]
    fn heartbeat_zero_disables_coalescing() {
        let mut c = TopCoalescer::new(0);
        assert!(c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 0));
        assert!(c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 1));
        assert!(c.admit("t1", Some(dec("0.4")), Some(dec("0.5")), 2));
        assert_eq!(c.totals(), (3, 0));
    }

    #[test]
    fn heartbeat_env_defaults_to_disabled() {
        // Every "I didn't really set this" shape must land on disabled.
        assert_eq!(parse_heartbeat(None), 0);
        assert_eq!(parse_heartbeat(Some("")), 0);
        assert_eq!(parse_heartbeat(Some("  ")), 0);
        assert_eq!(parse_heartbeat(Some("0")), 0);
        assert_eq!(parse_heartbeat(Some("-1")), 0);
        // A typo must never produce a live heartbeat; only a clean number can.
        assert_eq!(parse_heartbeat(Some("1s")), 0);
        assert_eq!(parse_heartbeat(Some("1000ms")), 0);
    }

    #[test]
    fn heartbeat_env_accepts_number_and_recommended_keyword() {
        assert_eq!(parse_heartbeat(Some("2500")), 2_500);
        assert_eq!(parse_heartbeat(Some(" 2500 ")), 2_500);
        assert_eq!(parse_heartbeat(Some("recommended")), TOP_HEARTBEAT_MS_RECOMMENDED);
        assert_eq!(parse_heartbeat(Some("Recommended")), TOP_HEARTBEAT_MS_RECOMMENDED);
    }

    #[test]
    fn retain_tokens_forgets_dead_rounds() {
        let mut c = TopCoalescer::new(1_000);
        assert!(c.admit("old", Some(dec("0.4")), Some(dec("0.5")), 0));
        assert!(c.admit("new", Some(dec("0.4")), Some(dec("0.5")), 0));
        c.retain_tokens(&["new".to_string()]);
        assert!(!c.last.contains_key("old"));
        assert!(!c.sent_at.contains_key("old"));
        assert!(c.last.contains_key("new"));
        // A reused token id starts clean rather than inheriting stale state.
        assert!(c.admit("old", Some(dec("0.4")), Some(dec("0.5")), 1));
    }
}
