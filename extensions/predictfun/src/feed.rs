//! predict.fun feed layer (#424).
//!
//! Polls `GET /v1/markets/{token_id}/orderbook` for the subscribed token set
//! and forwards every book through the [`MarketHost`] seam — the same snapshot
//! polling shape as the Polymarket and Kalshi feeds (explicit downgrade from
//! push: the venue has a WebSocket orderbook topic, stage 2 keeps the poll
//! cadence per token; the bit declared at the seam is `LEVEL2_SNAPSHOT`).
//!
//! Normalization happens HERE, at the plugin boundary, so the kernel never sees
//! a predict.fun-native unit:
//!   * prices — predict.fun already quotes 0..1 probabilities; the only mapping
//!     is the degenerate-band filter (a 0 or 1 quote carries no tradeable
//!     price and would breach the kernel's (0,1] contract), so such levels are
//!     dropped rather than forwarded;
//!   * depth — venue `shares` are already the kernel's depth unit, sent
//!     through unchanged;
//!   * timestamps — stamped with the LOCAL clock, matching the engine's
//!     freshness comparison. The venue's own timestamps never enter the
//!     engine's clock domain (`PREDICT_POLL_MS` sets the cadence).

use crate::rest::PredictRest;
use blitzkrieg_market_api::net::now_ms;
use blitzkrieg_market_api::{BookUpdate, MarketHost, TokenId};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Default interval between orderbook polls per token, in milliseconds.
/// `PREDICT_POLL_MS` overrides.
pub(crate) const DEFAULT_POLL_MS: u64 = 1_000;

/// Floor on the poll interval — a typo must not become a busy loop against
/// the venue.
pub(crate) const MIN_POLL_MS: u64 = 250;

/// Ceiling, and the cap for the 429 backoff.
pub(crate) const MAX_POLL_MS: u64 = 60_000;

/// Bound on a single poll request.
const REQUEST_TIMEOUT_MS: u64 = 5_000;

/// Resolve `PREDICT_POLL_MS` — same discipline as the other feeds: a malformed
/// value falls back to the default, never to "no delay".
pub(crate) fn parse_poll_ms(raw: Option<&str>) -> u64 {
    raw.map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_POLL_MS)
        .clamp(MIN_POLL_MS, MAX_POLL_MS)
}

/// 0..1 probability → the kernel's band. predict.fun prices arrive as
/// probabilities already; the filter only drops degenerate quotes (0 and 1)
/// that would breach the kernel's (0,1] price contract.
pub(crate) fn prob_in_band(price: Decimal) -> Option<Decimal> {
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
    /// Replace the orderbook subscription set (new round's tokens).
    pub async fn subscribe(&self, tokens: Vec<TokenId>) {
        let _ = self.tx.send(tokens).await;
    }
}

impl blitzkrieg_market_api::SubscriptionControl for FeedHandle {
    fn set_tokens(&self, tokens: Vec<TokenId>) {
        let _ = self.tx.try_send(tokens);
    }
}

/// Run the predict.fun feed, forwarding every update to the [`MarketHost`].
/// Returns the handle immediately; the poll loop runs on a background task.
pub(crate) fn spawn_feed(
    host: Arc<dyn MarketHost>,
    client: Arc<PredictRest>,
    initial_tokens: Vec<TokenId>,
) -> FeedHandle {
    spawn_feed_with(host, client, initial_tokens, 0)
}

/// Test-only entry for the reverse-acceptance suite: drive the real poll loop
/// against a mock server with an explicit cadence. Not part of the public
/// surface — the #424 gates exercise the production loop, not a copy of it.
#[doc(hidden)]
pub fn feed_tests_only_spawn(
    host: Arc<dyn MarketHost>,
    client: Arc<PredictRest>,
    initial_tokens: Vec<TokenId>,
    poll_ms: u64,
) -> FeedHandle {
    spawn_feed_with(host, client, initial_tokens, poll_ms)
}

/// Test-only alias of [`prob_in_band`] (it lives behind `pub(crate)`).
#[doc(hidden)]
pub fn feed_tests_only_prob_in_band(price: Decimal) -> Option<Decimal> {
    prob_in_band(price)
}

/// Body of [`spawn_feed`] with the cadence as a parameter (0 = env/default).
pub(crate) fn spawn_feed_with(
    host: Arc<dyn MarketHost>,
    client: Arc<PredictRest>,
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
                        tracing::info!(feed = %msg, "predictfun feed");
                    }
                }
            }
        });
    }

    let poll_ms = if poll_ms > 0 {
        poll_ms.clamp(MIN_POLL_MS, MAX_POLL_MS)
    } else {
        parse_poll_ms(std::env::var("PREDICT_POLL_MS").ok().as_deref())
    };
    tokio::spawn(predict_rest_loop(
        client,
        initial_tokens,
        ev_tx.clone(),
        ctl_rx,
        poll_ms,
    ));
    // Log the cadence through the pump (try_send: spawn_feed is sync).
    let _ = ev_tx.try_send(FeedEvent::Info(format!(
        "predictfun rest feed polling every {poll_ms}ms"
    )));

    FeedHandle { tx: ctl_tx }
}

/// Poll `GET /v1/markets/{token_id}/orderbook` for the subscribed token set.
///
/// REST snapshots, no retained state between rounds: a rollover is just a new
/// token set, so there is no subscription to leak. A poll failure emits no
/// event, so the book correctly ages out of the engine on its own; the loop
/// itself never stops — a venue outage degrades to stale books, never to a
/// dead feed that would need a restart.
async fn predict_rest_loop(
    client: Arc<PredictRest>,
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
                client.orderbook(token),
            )
            .await
            {
                Ok(Ok(book)) => {
                    interval_ms = base_ms; // recovered: back to the configured cadence
                    let bids: Vec<(Decimal, Decimal)> = book
                        .bids
                        .iter()
                        .filter_map(|l| prob_in_band(l.price).map(|p| (p, l.shares)))
                        .collect();
                    let asks: Vec<(Decimal, Decimal)> = book
                        .asks
                        .iter()
                        .filter_map(|l| prob_in_band(l.price).map(|p| (p, l.shares)))
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
                        .send(FeedEvent::Info(format!(
                            "predictfun orderbook {token}: {e}"
                        )))
                        .await;
                }
                Err(_) => {
                    let _ = ev_tx
                        .send(FeedEvent::Info(format!(
                            "predictfun orderbook {token} timed out after {REQUEST_TIMEOUT_MS}ms"
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
    fn probability_prices_pass_through_unchanged() {
        // predict.fun already quotes 0..1 — no scaling, unlike Kalshi's cents.
        assert_eq!(
            prob_in_band(Decimal::from_str("0.43").unwrap()),
            Some(Decimal::from_str("0.43").unwrap())
        );
        assert_eq!(
            prob_in_band(Decimal::from_str("0.001").unwrap()),
            Some(Decimal::from_str("0.001").unwrap())
        );
    }

    #[test]
    fn degenerate_probabilities_are_dropped_not_forwarded() {
        // 0 and 1 would breach the kernel's (0,1] price contract; dropping
        // beats mapping into a refusal at the host seam.
        assert_eq!(prob_in_band(Decimal::ZERO), None);
        assert_eq!(prob_in_band(Decimal::ONE), None);
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
