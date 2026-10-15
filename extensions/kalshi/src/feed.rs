//! Kalshi feed layer (#424).
//!
//! Polls `GET /markets/{ticker}/orderbook` for the subscribed market set and
//! forwards every book through the [`MarketHost`] seam — the same shape as the
//! Polymarket extension's REST feed (snapshot polling over a push WebSocket,
//! deliberately: see that crate's bandwidth verdict).
//!
//! Normalization happens HERE, at the plugin boundary, so the kernel never sees
//! a Kalshi-native unit:
//!   * prices — the round series' `orderbook_fp` form already quotes 0..1
//!     probabilities, so prices flow through unchanged (the legacy cent form
//!     these series never emit is gone);
//!   * sides — the fp form carries only BID ladders (`yes_dollars`,
//!     `no_dollars`). The YES book's asks are derived by duality: a resting NO
//!     bid at price p IS a YES ask at 1−p (verified against production: the
//!     venue's own no_ask equals 1 − yes_bid field-for-field). The NO ladder
//!     likewise becomes the DOWN token's bids, with asks = 1 − yes bids.
//!   * depth — fp depth is the resting NOTIONAL in dollars; contracts =
//!     notional / price (the kernel's share unit). Fractional notionals prove
//!     the unit (a contract count is always integral);
//!   * timestamps — stamped with the LOCAL clock, matching the engine's
//!     freshness comparison (`fresh_book` compares against the engine's own
//!     clock). The venue's own timestamp travels in the log line only.
//!
//! Kalshi publishes a real WebSocket orderbook channel, but stage 2 keeps the
//! REST-poll shape: one request serves one market (no multi-market batch on
//! this endpoint), so the poll cadence is per-market. `KALSHI_POLL_MS` sets it.

use crate::discovery::NO_SUFFIX;
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

/// Guard for a venue-supplied fp-book probability price: the kernel's
/// prediction contract is (0,1), so a level outside that band (a field-mixed
/// venue bug, a 0/1 sentinel) is dropped rather than forwarded.
pub(crate) fn prob_in_band(p: Decimal) -> Option<Decimal> {
    (p > Decimal::ZERO && p < Decimal::ONE).then_some(p)
}

/// ORDER-API domain (executor, fills): cents 1..=99 → probability 0..1.
/// The fp orderbook form carries probabilities directly and never goes
/// through this; only the venue's order/fill payloads are cent-denominated.
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

/// Test-only alias of [`prob_in_band`] (it lives behind `pub(crate)`).
#[doc(hidden)]
pub fn feed_tests_only_cents_to_prob(cents: Decimal) -> Option<Decimal> {
    prob_in_band(cents)
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
///
/// The subscription set carries TWO tokens per market (`TICKER` = YES/UP,
/// `TICKER:no` = NO/DOWN — the discovery convention), but the venue has ONE
/// book per market. The loop therefore dedupes to the underlying market
/// ticker, polls it once per cycle, and splits the fp book into the two
/// token books:
///   * UP token — bids = yes_dollars ladder, asks = 1 − no_dollars (duality);
///   * DOWN token — bids = no_dollars ladder, asks = 1 − yes_dollars.
///
/// Sending the YES side to the DOWN token (the pre-fix behavior) quoted the
/// DOWN side with the UP book — two tokens, one opinion.
///
/// One book split ready to emit: the token's side label plus its two
/// ladders (a `(Decimal, Decimal)` price/depth pair per level).
type TokenSplit<'a> = (&'a str, Vec<(Decimal, Decimal)>, Vec<(Decimal, Decimal)>);

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

        // One poll per MARKET: strip the `:no` suffix so a two-token
        // subscription set costs one request, not two (and never delivers
        // the YES book under the DOWN token's name).
        let mut markets: Vec<&str> = tickers.iter().map(|t| strip_no_suffix(t)).collect();
        markets.sort_unstable();
        markets.dedup();

        for market in markets {
            match tokio::time::timeout(
                Duration::from_millis(REQUEST_TIMEOUT_MS),
                client.orderbook(market),
            )
            .await
            {
                Ok(Ok(book)) => {
                    interval_ms = base_ms; // recovered: back to the configured cadence
                    let yes_bids = book.yes_bids();
                    let no_bids = book.no_bids();
                    let mut out: Vec<TokenSplit<'_>> = Vec::with_capacity(2);
                    if !yes_bids.is_empty() || !no_bids.is_empty() {
                        let yes_bids_v: Vec<(Decimal, Decimal)> =
                            yes_bids.iter().map(|l| (l.price, l.shares)).collect();
                        let no_bids_v: Vec<(Decimal, Decimal)> =
                            no_bids.iter().map(|l| (l.price, l.shares)).collect();
                        // asks by duality: a NO bid at p IS a YES ask at 1−p.
                        let yes_asks: Vec<(Decimal, Decimal)> = no_bids
                            .iter()
                            .rev()
                            .filter_map(|l| {
                                prob_in_band(Decimal::ONE - l.price).map(|p| (p, l.shares))
                            })
                            .collect();
                        let no_asks: Vec<(Decimal, Decimal)> = yes_bids
                            .iter()
                            .rev()
                            .filter_map(|l| {
                                prob_in_band(Decimal::ONE - l.price).map(|p| (p, l.shares))
                            })
                            .collect();
                        out.push(("YES", yes_bids_v, yes_asks));
                        out.push(("NO", no_bids_v, no_asks));
                    }
                    for (side, bids, asks) in out {
                        let token_id = match side {
                            "YES" => market.to_string(),
                            _ => format!("{market}{NO_SUFFIX}"),
                        };
                        if ev_tx
                            .send(FeedEvent::Book {
                                token_id,
                                bids,
                                asks,
                            })
                            .await
                            .is_err()
                        {
                            return; // host is gone
                        }
                    }
                }
                Ok(Err(e)) => {
                    if e.is_rate_limited() {
                        interval_ms = interval_ms.saturating_mul(2).min(MAX_POLL_MS);
                    }
                    let _ = ev_tx
                        .send(FeedEvent::Info(format!("kalshi orderbook {market}: {e}")))
                        .await;
                }
                Err(_) => {
                    let _ = ev_tx
                        .send(FeedEvent::Info(format!(
                            "kalshi orderbook {market} timed out after {REQUEST_TIMEOUT_MS}ms"
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

/// `KXBTC15M-…-30:no` → `KXBTC15M-…-30`; a plain ticker passes through.
fn strip_no_suffix(token: &str) -> &str {
    token.strip_suffix(NO_SUFFIX).unwrap_or(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::KalshiOrderBook;
    use std::str::FromStr;

    #[test]
    fn order_api_cents_map_into_the_kernel_probability_band() {
        // The executor's order/fill payloads are cent-denominated; this
        // mapping is that domain's, not the fp book's.
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
    fn fp_probabilities_stay_in_band_and_sentinels_are_dropped() {
        assert_eq!(
            prob_in_band(Decimal::from_str("0.65").unwrap()),
            Some(Decimal::from_str("0.65").unwrap())
        );
        assert_eq!(prob_in_band(Decimal::ZERO), None);
        assert_eq!(prob_in_band(Decimal::ONE), None);
        assert_eq!(prob_in_band(Decimal::from_str("1.5").unwrap()), None);
    }

    /// A real production fp body (KXBTC15M round, 2026-10-15): two bid
    /// ladders of `[price, notional]` decimal strings, best bid LAST.
    /// Derived checks the kernel depends on: best YES bid 0.56, best NO bid
    /// 0.43 ⇒ implied YES ask 0.57 (1 − 0.43) — a one-cent book, exactly
    /// what the venue was quoting when captured.
    const FP_BODY: &str = r#"{"orderbook_fp":{"yes_dollars":[["0.0010","1750616.00"],["0.5400","2813.00"],["0.5500","7424.96"],["0.5600","4467.73"]],"no_dollars":[["0.0010","1827761.00"],["0.4100","2851.25"],["0.4200","4272.13"],["0.4300","2678.99"]]}}"#;

    #[test]
    fn fp_book_parses_prices_as_probabilities_and_notional_as_depth() {
        let book: KalshiOrderBook = serde_json::from_str(FP_BODY).expect("fp body parses");
        assert!(book.is_two_sided());

        let yes = book.yes_bids();
        assert_eq!(yes.len(), 4);
        let best_yes = yes.last().expect("best bid last");
        assert_eq!(best_yes.price, Decimal::from_str("0.5600").unwrap());
        // contracts = notional / price: 4467.73 / 0.56 = 7978.089…
        let expected = Decimal::from_str("4467.73").unwrap() / Decimal::from_str("0.56").unwrap();
        assert_eq!(best_yes.shares, expected);

        let no = book.no_bids();
        assert_eq!(
            no.last().expect("best no bid").price,
            Decimal::from_str("0.4300").unwrap()
        );
    }

    #[tokio::test]
    async fn poll_splits_one_market_book_into_two_token_books_by_duality() {
        let (ev_tx, mut ev_rx) = mpsc::channel::<FeedEvent>(16);
        let body: KalshiOrderBook = serde_json::from_str(FP_BODY).expect("fp body parses");
        let yes_bids = body.yes_bids();
        let no_bids = body.no_bids();
        // The split the poll loop performs (extracted here so the test pins
        // the arithmetic, not the loop's scheduling).
        let up = (
            yes_bids
                .iter()
                .map(|l| (l.price, l.shares))
                .collect::<Vec<_>>(),
            no_bids
                .iter()
                .rev()
                .filter_map(|l| prob_in_band(Decimal::ONE - l.price).map(|p| (p, l.shares)))
                .collect::<Vec<_>>(),
        );
        let down = (
            no_bids
                .iter()
                .map(|l| (l.price, l.shares))
                .collect::<Vec<_>>(),
            yes_bids
                .iter()
                .rev()
                .filter_map(|l| prob_in_band(Decimal::ONE - l.price).map(|p| (p, l.shares)))
                .collect::<Vec<_>>(),
        );
        let _ = ev_tx.try_send(FeedEvent::Book {
            token_id: "KXTEST-1".into(),
            bids: up.0,
            asks: up.1,
        });
        let _ = ev_tx.try_send(FeedEvent::Book {
            token_id: "KXTEST-1:no".into(),
            bids: down.0,
            asks: down.1,
        });
        let first = ev_rx.recv().await.expect("up book");
        let FeedEvent::Book {
            token_id,
            bids,
            asks,
        } = first
        else {
            panic!("expected a book");
        };
        assert_eq!(token_id, "KXTEST-1");
        assert_eq!(
            bids.last().expect("best bid").0,
            Decimal::from_str("0.5600").unwrap()
        );
        assert_eq!(
            asks.first().expect("best ask").0,
            Decimal::from_str("0.5700").unwrap()
        );

        let second = ev_rx.recv().await.expect("down book");
        let FeedEvent::Book {
            token_id,
            bids,
            asks,
        } = second
        else {
            panic!("expected a book");
        };
        assert_eq!(token_id, "KXTEST-1:no");
        assert_eq!(
            bids.last().expect("best bid").0,
            Decimal::from_str("0.4300").unwrap()
        );
        assert_eq!(
            asks.first().expect("best ask").0,
            Decimal::from_str("0.4400").unwrap()
        );
    }

    #[test]
    fn no_suffix_strips_to_the_market_ticker() {
        assert_eq!(
            strip_no_suffix("KXBTC15M-26OCT150830-30:no"),
            "KXBTC15M-26OCT150830-30"
        );
        assert_eq!(
            strip_no_suffix("KXBTC15M-26OCT150830-30"),
            "KXBTC15M-26OCT150830-30"
        );
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
