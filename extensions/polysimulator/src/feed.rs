//! PolySimulator feed layer (#454).
//!
//! Polls `GET /v1/clob/book/{token_id}` for the subscribed token set and
//! forwards every snapshot through the [`MarketHost`] seam — the same REST
//! snapshot-polling shape as the kalshi and polymarket extension feeds.
//!
//! Normalization happens HERE, at the plugin boundary:
//!   * prices — the venue quotes dollars 0..1 (the same probability band the
//!     kernel speaks), so prices pass through UNCHANGED; levels outside the
//!     kernel's (0,1] contract are dropped;
//!   * depth — venue `size` is shares, the kernel's unit: unchanged;
//!   * timestamps — stamped with the LOCAL clock, matching the engine's
//!     freshness comparison (`fresh_book` compares against the engine's own
//!     clock). The venue's own string timestamp travels in the log line only.
//!
//! The venue publishes PM-compat and native WebSocket book channels, but the
//! extension keeps the REST-poll shape deliberately (one dependency surface
//! fewer: no JWT minting, no reconnect ladder, and the free tier's single-WS
//! connection cap cannot starve the feed). `POLYSIM_POLL_MS` sets the cadence.

use crate::rest::PolySimRest;
use blitzkrieg_market_api::net::now_ms;
use blitzkrieg_market_api::{BookUpdate, MarketHost, TokenId};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Default interval between book polls per token, in milliseconds. The free
/// tier sustains 2 req/s: at the 1000ms default one token costs 1 read/s, so
/// two concurrent tokens sit right at the published ceiling — the limiter
/// smooths the burst and the failure path backs off.
pub(crate) const DEFAULT_POLL_MS: u64 = 1_000;

/// Floor on the poll interval — a typo must not become a busy loop against
/// the venue.
pub(crate) const MIN_POLL_MS: u64 = 250;

/// Ceiling, and the cap for the 429 backoff.
pub(crate) const MAX_POLL_MS: u64 = 60_000;

/// Bound on a single poll request.
const REQUEST_TIMEOUT_MS: u64 = 5_000;

/// Resolve `POLYSIM_POLL_MS` — same discipline as the sibling feeds: a
/// malformed value falls back to the default, never to "no delay".
pub(crate) fn parse_poll_ms(raw: Option<&str>) -> u64 {
    raw.map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_POLL_MS)
        .clamp(MIN_POLL_MS, MAX_POLL_MS)
}

/// Book levels pass through the 0..1 band the kernel's prediction-price
/// contract requires; a level outside it is dropped rather than forwarded
/// (a 0 or 1 quote carries no tradeable price anyway).
pub(crate) fn band_to_prob(price: Decimal) -> Option<Decimal> {
    (price > Decimal::ZERO && price < Decimal::ONE).then_some(price)
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
    /// Replace the book subscription set (new round's tokens).
    pub async fn subscribe(&self, tokens: Vec<TokenId>) {
        let _ = self.tx.send(tokens).await;
    }
}

impl blitzkrieg_market_api::SubscriptionControl for FeedHandle {
    fn set_tokens(&self, tokens: Vec<TokenId>) {
        let _ = self.tx.try_send(tokens);
    }
}

/// Test-only entry for the reverse-acceptance suite: drive the real poll loop
/// against a mock server with an explicit cadence (`POLYSIM_POLL_MS` env
/// parsing stays out of the test's control). Not part of the public surface —
/// it exists so the gates exercise the production loop, not a copy of it.
#[doc(hidden)]
pub fn feed_tests_only_spawn(
    host: Arc<dyn MarketHost>,
    client: Arc<PolySimRest>,
    initial_tokens: Vec<TokenId>,
    poll_ms: u64,
) -> FeedHandle {
    spawn_feed_with(host, client, initial_tokens, poll_ms)
}

/// Test-only alias of [`band_to_prob`] (it lives behind `pub(crate)`).
#[doc(hidden)]
pub fn feed_tests_only_band_to_prob(price: Decimal) -> Option<Decimal> {
    band_to_prob(price)
}

/// Body of [`spawn_feed`] with the cadence as a parameter (0 = env/default).
pub(crate) fn spawn_feed_with(
    host: Arc<dyn MarketHost>,
    client: Arc<PolySimRest>,
    initial_tokens: Vec<TokenId>,
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
                        tracing::info!(feed = %msg, "polysimulator feed");
                    }
                }
            }
        });
    }

    let poll_ms = poll_ms_or_env(poll_ms);
    tokio::spawn(polysim_rest_loop(
        client,
        initial_tokens,
        ev_tx.clone(),
        ctl_rx,
        poll_ms,
    ));
    // Log the cadence through the pump (try_send: spawn_feed is sync).
    let _ = ev_tx.try_send(FeedEvent::Info(format!(
        "polysimulator rest feed polling every {poll_ms}ms"
    )));

    FeedHandle { tx: ctl_tx }
}

/// The configured cadence, or the `POLYSIM_POLL_MS` env override when the
/// caller passed 0 (the production entry point).
fn poll_ms_or_env(explicit: u64) -> u64 {
    if explicit > 0 {
        return explicit.clamp(MIN_POLL_MS, MAX_POLL_MS);
    }
    parse_poll_ms(std::env::var("POLYSIM_POLL_MS").ok().as_deref())
}

/// Poll `GET /v1/clob/book/{token_id}` for the subscribed token set.
///
/// REST snapshots, no retained state between rounds: a rollover is just a new
/// token set, so there is no subscription to leak. A poll failure emits no
/// event, so the book correctly ages out of the engine on its own; the loop
/// itself never stops.
async fn polysim_rest_loop(
    client: Arc<PolySimRest>,
    mut tokens: Vec<TokenId>,
    ev_tx: mpsc::Sender<FeedEvent>,
    mut ctl_rx: mpsc::Receiver<Vec<TokenId>>,
    base_ms: u64,
) {
    let mut interval_ms = base_ms;
    loop {
        if tokens.is_empty() {
            // Nothing to ask the venue for: wait for a usable set instead of
            // spinning on requests that cannot be built.
            match ctl_rx.recv().await {
                Some(t) => tokens = t,
                None => return,
            }
            continue;
        }

        for token in &tokens {
            match tokio::time::timeout(
                Duration::from_millis(REQUEST_TIMEOUT_MS),
                client.book(token),
            )
            .await
            {
                Ok(Ok(book)) => {
                    interval_ms = base_ms; // recovered: back to the configured cadence
                    let bids: Vec<(Decimal, Decimal)> = book
                        .bids
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|l| band_to_prob(l.price).map(|p| (p, l.size)))
                        .collect();
                    let asks: Vec<(Decimal, Decimal)> = book
                        .asks
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|l| band_to_prob(l.price).map(|p| (p, l.size)))
                        .collect();
                    if ev_tx
                        .send(FeedEvent::Book {
                            token_id: token.clone(),
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
                        .send(FeedEvent::Info(format!("polysimulator book {token}: {e}")))
                        .await;
                }
                Err(_) => {
                    let _ = ev_tx
                        .send(FeedEvent::Info(format!(
                            "polysimulator book {token} timed out after {REQUEST_TIMEOUT_MS}ms"
                        )))
                        .await;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn band_prices_pass_through_unmapped() {
        // The venue already speaks 0..1: identical values to kalshi's
        // cents_to_prob but with the division gone.
        assert_eq!(
            band_to_prob(Decimal::from_str("0.43").unwrap()),
            Some(Decimal::from_str("0.43").unwrap())
        );
        assert_eq!(
            band_to_prob(Decimal::from_str("0.995").unwrap()),
            Some(Decimal::from_str("0.995").unwrap())
        );
    }

    #[test]
    fn degenerate_prices_are_dropped_not_forwarded() {
        assert_eq!(band_to_prob(Decimal::ZERO), None);
        assert_eq!(band_to_prob(Decimal::ONE), None);
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
