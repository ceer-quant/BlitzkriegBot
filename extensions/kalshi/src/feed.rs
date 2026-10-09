//! Kalshi feed layer (#424).
//!
//! Polls `GET /markets/{ticker}/orderbook` for the subscribed market set and
//! forwards every book through the [`MarketHost`] seam — the same shape as the
//! Polymarket extension's REST feed (snapshot polling over a push WebSocket,
//! deliberately: see that crate's bandwidth verdict).
//!
//! Normalization happens HERE, at the plugin boundary, so the kernel never sees
//! a Kalshi-native unit:
//!   * prices — Kalshi quotes cents 0..100; the kernel's prediction-price
//!     contract is a 0..1 probability (`MarketHost` prices in (0,1]), so every
//!     level is divided by 100 before it is sent;
//!   * depth — Kalshi depth is a contract COUNT; the kernel's depth unit is
//!     shares. One Kalshi contract pays $1 at settlement, the same as one
//!     Polymarket share, so the count is sent through unchanged (documented,
//!     not silently converted);
//!   * timestamps — stamped with the LOCAL clock, matching the engine's
//!     freshness comparison (`fresh_book` compares against the engine's own
//!     clock). The venue's own timestamp travels in the log line only.
//!
//! Kalshi publishes a real WebSocket orderbook channel, but stage 2 keeps the
//! REST-poll shape: one request serves one market (no multi-market batch on
//! this endpoint), so the poll cadence is per-market. `KALSHI_POLL_MS` sets it.

use crate::rest::KalshiRest;
use blitzkrieg_market_api::net::now_ms;
use blitzkrieg_market_api::{BookUpdate, MarketHost, TokenId};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Default interval between orderbook polls per market, in milliseconds.
/// Kalshi's public rate limit for market data is generous (10 read/s
/// sustained); at 1000ms one market costs 1 read/s. `KALSHI_POLL_MS` overrides.
pub(crate) const DEFAULT_POLL_MS: u64 = 1_000;

/// Floor on the poll interval — a typo must not become a busy loop against
/// the venue.
pub(crate) const MIN_POLL_MS: u64 = 250;

/// Ceiling, and the cap for the 429 backoff.
pub(crate) const MAX_POLL_MS: u64 = 60_000;

/// Bound on a single poll request.
const REQUEST_TIMEOUT_MS: u64 = 5_000;

/// Resolve `KALSHI_POLL_MS` — same discipline as the Polymarket feed: a
/// malformed value falls back to the default, never to "no delay".
pub(crate) fn parse_poll_ms(raw: Option<&str>) -> u64 {
    raw.map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_POLL_MS)
        .clamp(MIN_POLL_MS, MAX_POLL_MS)
}

/// Cents 0..100 → probability 0..1. The kernel refuses prediction prices
/// outside (0,1], so a level that does not survive the mapping is dropped
/// rather than forwarded (a 0 or 100 cent quote carries no tradeable price
/// anyway).
pub(crate) fn cents_to_prob(cents: Decimal) -> Option<Decimal> {
    let p = cents / Decimal::from(100);
    (p > Decimal::ZERO && p < Decimal::ONE).then_some(p)
}

/// Feed event: a normalized book, or a log line.
#[derive(Debug, Clone)]
pub(crate) enum FeedEvent {
    Book {
        token_id: TokenId,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
    },
    Info(String),
}

/// Handle the host registers so later `subscribe_tokens` calls reach the loop.
#[derive(Debug, Clone)]
pub struct FeedHandle {
    tx: mpsc::Sender<Vec<TokenId>>,
}

impl FeedHandle {
    /// Replace the orderbook subscription set (new round's tickers).
    pub async fn subscribe(&self, tickers: Vec<TokenId>) {
        let _ = self.tx.send(tickers).await;
    }
}

impl blitzkrieg_market_api::SubscriptionControl for FeedHandle {
    fn set_tokens(&self, tickers: Vec<TokenId>) {
        let _ = self.tx.try_send(tickers);
    }
}

/// Test-only entry for the reverse-acceptance suite: drive the real poll loop
/// against a mock server with an explicit cadence (`KALSHI_POLL_MS` env
/// parsing stays out of the test's control). Not part of the public surface —
/// it exists so the #424 gates exercise the production loop, not a copy of it.
#[doc(hidden)]
pub fn feed_tests_only_spawn(
    host: Arc<dyn MarketHost>,
    client: Arc<KalshiRest>,
    initial_tickers: Vec<TokenId>,
    poll_ms: u64,
) -> FeedHandle {
    spawn_feed_with(host, client, initial_tickers, poll_ms)
}

/// Test-only alias of [`cents_to_prob`] (it lives behind `pub(crate)`).
#[doc(hidden)]
pub fn feed_tests_only_cents_to_prob(cents: Decimal) -> Option<Decimal> {
    cents_to_prob(cents)
}

/// Body of [`spawn_feed`] with the cadence as a parameter (0 = env/default).
pub(crate) fn spawn_feed_with(
    host: Arc<dyn MarketHost>,
    client: Arc<KalshiRest>,
    initial_tickers: Vec<TokenId>,
    poll_ms: u64,
) -> FeedHandle {
    let (ev_tx, mut ev_rx) = mpsc::channel::<FeedEvent>(256);
    let (ctl_tx, ctl_rx) = mpsc::channel::<Vec<TokenId>>(16);

    // Event pump: feed → host (the market seam).
    {
        let host = host.clone();
        tokio::spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                match ev {
                    FeedEvent::Book {
                        token_id,
                        bids,
                        asks,
                    } => {
                        host.on_book(BookUpdate {
                            token_id,
                            bids,
                            asks,
                            ts_ms: now_ms(),
                        })
                        .await;
                    }
                    FeedEvent::Info(msg) => {
                        tracing::info!(feed = %msg, "kalshi feed");
                    }
                }
            }
        });
    }

    let poll_ms = poll_ms_or_env(poll_ms);
    tokio::spawn(kalshi_rest_loop(
        client,
        initial_tickers,
        ev_tx.clone(),
        ctl_rx,
        poll_ms,
    ));
    // Log the cadence through the pump (try_send: spawn_feed is sync).
    let _ = ev_tx.try_send(FeedEvent::Info(format!(
        "kalshi rest feed polling every {poll_ms}ms"
    )));

    FeedHandle { tx: ctl_tx }
}

/// The configured cadence, or the `KALSHI_POLL_MS` env override when the
/// caller passed 0 (the production entry point).
fn poll_ms_or_env(explicit: u64) -> u64 {
    if explicit > 0 {
        return explicit.clamp(MIN_POLL_MS, MAX_POLL_MS);
    }
    parse_poll_ms(std::env::var("KALSHI_POLL_MS").ok().as_deref())
}

/// Poll `GET /markets/{ticker}/orderbook` for the subscribed ticker set.
///
/// REST snapshots, no retained state between rounds: a rollover is just a new
/// ticker set, so there is no subscription to leak. A poll failure emits no
/// event, so the book correctly ages out of the engine on its own; the loop
/// itself never stops.
async fn kalshi_rest_loop(
    client: Arc<KalshiRest>,
    mut tickers: Vec<TokenId>,
    ev_tx: mpsc::Sender<FeedEvent>,
    mut ctl_rx: mpsc::Receiver<Vec<TokenId>>,
    base_ms: u64,
) {
    let mut interval_ms = base_ms;
    loop {
        if tickers.is_empty() {
            // Nothing to ask the venue for: wait for a usable set instead of
            // spinning on requests that cannot be built.
            match ctl_rx.recv().await {
                Some(t) => tickers = t,
                None => return,
            }
            continue;
        }

        for ticker in &tickers {
            match tokio::time::timeout(
                Duration::from_millis(REQUEST_TIMEOUT_MS),
                client.orderbook(ticker),
            )
            .await
            {
                Ok(Ok(book)) => {
                    interval_ms = base_ms; // recovered: back to the configured cadence
                    let bids: Vec<(Decimal, Decimal)> = book
                        .yes_levels()
                        .iter()
                        .filter_map(|l| cents_to_prob(l.price).map(|p| (p, l.shares())))
                        .collect();
                    let asks: Vec<(Decimal, Decimal)> = book
                        .no_levels()
                        .iter()
                        .filter_map(|l| cents_to_prob(l.price).map(|p| (p, l.shares())))
                        .collect();
                    if ev_tx
                        .send(FeedEvent::Book {
                            token_id: ticker.clone(),
                            bids,
                            asks,
                        })
                        .await
                        .is_err()
                    {
                        return; // host is gone
                    }
                }
                Ok(Err(e)) => {
                    if e.is_rate_limited() {
                        interval_ms = interval_ms.saturating_mul(2).min(MAX_POLL_MS);
                    }
                    let _ = ev_tx
                        .send(FeedEvent::Info(format!("kalshi orderbook {ticker}: {e}")))
                        .await;
                }
                Err(_) => {
                    let _ = ev_tx
                        .send(FeedEvent::Info(format!(
                            "kalshi orderbook {ticker} timed out after {REQUEST_TIMEOUT_MS}ms"
                        )))
                        .await;
                }
            }
        }

        // Wait out the interval, but let a new ticker set cut the wait short: a
        // rollover should not sit on the previous round's cadence before its
        // first fetch.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {}
            update = ctl_rx.recv() => match update {
                Some(t) => tickers = t,
                None => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn cents_map_into_the_kernel_probability_band() {
        assert_eq!(
            cents_to_prob(Decimal::from_str("43").unwrap()),
            Some(Decimal::from_str("0.43").unwrap())
        );
        assert_eq!(
            cents_to_prob(Decimal::from_str("1").unwrap()),
            Some(Decimal::from_str("0.01").unwrap())
        );
        assert_eq!(
            cents_to_prob(Decimal::from_str("99.5").unwrap()),
            Some(Decimal::from_str("0.995").unwrap())
        );
    }

    #[test]
    fn degenerate_cents_are_dropped_not_forwarded() {
        // 0 and 100 cents would breach the kernel's (0,1] price contract;
        // dropping beats mapping into a refusal at the host seam.
        assert_eq!(cents_to_prob(Decimal::ZERO), None);
        assert_eq!(cents_to_prob(Decimal::from(100)), None);
    }

    #[test]
    fn poll_ms_malformed_values_fall_back_never_busy_loop() {
        assert_eq!(parse_poll_ms(None), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("not-a-number")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("0")), DEFAULT_POLL_MS);
        assert_eq!(parse_poll_ms(Some("50")), MIN_POLL_MS);
        assert_eq!(parse_poll_ms(Some("999999")), MAX_POLL_MS);
        assert_eq!(parse_poll_ms(Some("2500")), 2_500);
    }
}
