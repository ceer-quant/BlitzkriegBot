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
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

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
    {
        let ev_tx = ev_tx.clone();
        let tokens = token_ids.clone();
        tokio::spawn(async move {
            poly_orderbook_loop(tokens, ev_tx, ctl_rx).await;
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

async fn poly_orderbook_loop(
    mut tokens: Vec<String>,
    ev_tx: mpsc::Sender<FeedEvent>,
    mut ctl_rx: mpsc::Receiver<Vec<String>>,
) {
    use polymarket_client_sdk_v2::clob::ws::Client as WsClient;
    use polymarket_client_sdk_v2::types::U256;
    use polymarket_client_sdk_v2::ws::config::Config as WsConfig;

    let ws_url = std::env::var("POLYMARKET_WS_URL")
        .unwrap_or_else(|_| "wss://ws-subscriptions-clob.polymarket.com".into());

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
        let client = match WsClient::new(&ws_url, WsConfig::default()) {
            Ok(c) => c,
            Err(e) => {
                let _ = ev_tx.send(FeedEvent::Info(format!("poly ws client: {e}"))).await;
                if !wait_or_update(&mut tokens, &mut ctl_rx, 5).await {
                    return;
                }
                continue;
            }
        };
        // Polymarket's market channel sends ONE full `book` snapshot on subscribe,
        // then only `price_change` incrementals. The SDK's `subscribe_orderbook`
        // filters those incrementals out (`_ => None`), so relying on it alone left
        // the local book frozen except on the rare re-snapshot — illiquid tokens
        // (XRP/SOL) could sit 20-60s stale while BTC updated, which is what made the
        // panel's price look stuck. `subscribe_prices` carries the same channel's
        // `price_change` events (each entry has best_bid/best_ask), which we apply
        // as top-of-book updates. Both subscriptions share one MARKET channel and
        // refcount their assets, so this does not disturb the book stream.
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
                let _ = ev_tx.send(FeedEvent::Info(format!("poly price subscribe: {e}"))).await;
                if !wait_or_update(&mut tokens, &mut ctl_rx, 5).await {
                    return;
                }
                continue;
            }
        };
        let mut stream = Box::pin(stream);
        let mut price_stream = Box::pin(price_stream);
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
                        let _ = ev_tx.send(FeedEvent::Info("poly subscription updated".into())).await;
                        if t.is_empty() { tokens.clear(); return; }
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
}
