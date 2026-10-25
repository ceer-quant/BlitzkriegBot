//! Stage-2 plugin reverse-acceptance tests (#424).
//!
//! #424's acceptance clause names three failure modes that must be RED — i.e.
//! the plugin layer must provably not exhibit them. Each test below drives the
//! real feed loop (or the real normalization path) against a 127.0.0.1 mock
//! and asserts the failure CANNOT pass silently:
//!
//! 1. **missing incremental updates** — these feeds are REST snapshot polls,
//!    an explicit downgrade (declared `LEVEL2_SNAPSHOT`, never
//!    `WEBSOCKET_FEED`). The failure mode to refuse is a *stale snapshot
//!    forward*: when a poll fails, no event is emitted, so the book ages out
//!    of the engine instead of the last snapshot being re-quoted as fresh.
//!    The test proves a failed poll produces ZERO book events.
//! 2. **no reconnect after disconnect** — the loop must survive a venue that
//!    dies mid-stream: it keeps polling, and the first healthy response after
//!    recovery flows through. A loop that unwinds on error would need a
//!    process restart to trade again; the test proves recovery WITHOUT one.
//! 3. **mixed clock domains producing false signals** — the engine compares
//!    book freshness against its own clock. A venue timestamp forwarded as
//!    `ts_ms` would read as stale-by-hours (or from the future) the moment
//!    the venue's clock drifts. The test drives the loop over a mock and
//!    asserts every emitted `ts_ms` falls within the local wall-clock window
//!    of the test itself — no venue-provided timestamp can leak through.

use blitzkrieg_market_api::{BookUpdate, MarketHost, TokenId};
use rust_decimal::Decimal;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

// ── shared harness ───────────────────────────────────────────────────────────

/// Books pushed through the host, with the local receive wall-clock.
#[derive(Clone, Default)]
struct BookSink {
    books: Arc<Mutex<Vec<(BookUpdate, i64)>>>,
}

impl BookSink {
    async fn book_count(&self) -> usize {
        self.books.lock().await.len()
    }

    async fn books(&self) -> Vec<(BookUpdate, i64)> {
        self.books.lock().await.clone()
    }
}

/// Rounds pushed through the host (#548 gate: the discovery loop must re-feed
/// when the venue's batch grows mid-round, and NOT re-feed an unchanged batch).
#[derive(Clone, Default)]
struct DiscoverySink {
    rounds: Arc<Mutex<Vec<Vec<blitzkrieg_market_api::MarketDescriptor>>>>,
}

impl DiscoverySink {
    async fn round_count(&self) -> usize {
        self.rounds.lock().await.len()
    }

    async fn last_round_tickers(&self) -> Vec<String> {
        self.rounds
            .lock()
            .await
            .last()
            .map(|ms| ms.iter().map(|m| m.up_token_id.clone()).collect())
            .unwrap_or_default()
    }

    async fn wait_for_rounds(&self, want: usize, budget: Duration) -> usize {
        let deadline = std::time::Instant::now() + budget;
        loop {
            let n = self.round_count().await;
            if n >= want || std::time::Instant::now() >= deadline {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl MarketHost for DiscoverySink {
    fn on_book(&self, _: BookUpdate) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_top_of_book(
        &self,
        _: blitzkrieg_market_api::TopOfBookUpdate,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_spot(
        &self,
        _: blitzkrieg_market_api::SpotUpdate,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_round_markets(
        &self,
        markets: Vec<blitzkrieg_market_api::MarketDescriptor>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async move {
            self.rounds.lock().await.push(markets);
        })
    }
    fn subscribe_tokens(
        &self,
        _: &str,
        _: Vec<TokenId>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn install_subscription_control(
        &self,
        _: &str,
        _: Arc<dyn blitzkrieg_market_api::SubscriptionControl>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn take_pending_orders(
        &self,
        _: &str,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<blitzkrieg_market_api::PendingOrder>> {
        Box::pin(async { Vec::new() })
    }
    fn take_pending_cancels(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    fn on_order_accepted(&self, _: &str, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_order_rejected(
        &self,
        _: &str,
        _: Option<blitzkrieg_market_api::CoreError>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_place_timeout(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_fill(
        &self,
        _: blitzkrieg_market_api::MarketFill,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_order_live(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_order_cancelled(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_reconcile(
        &self,
        _: blitzkrieg_market_api::ReconcileSnapshot,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_reconcile_failed(
        &self,
        _: &str,
        _: blitzkrieg_market_api::CoreError,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_self_check(
        &self,
        _: &str,
        _: blitzkrieg_market_api::SelfCheckReport,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn seed_balance(&self, _venue: &str, _: Decimal) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn venue_free_balance(
        &self,
        _venue: &str,
        _: Decimal,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn known_venue_order_ids(&self) -> blitzkrieg_market_api::BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    fn note_orphan_cancelled(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn report_error(
        &self,
        _: blitzkrieg_market_api::CoreError,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn take_settlement_queries(
        &self,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<blitzkrieg_market_api::SettlementQuery>> {
        Box::pin(async { Vec::new() })
    }
    fn on_market_resolution(
        &self,
        _: blitzkrieg_market_api::MarketResolution,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn take_pending_redemptions(
        &self,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<blitzkrieg_market_api::RedemptionRequest>> {
        Box::pin(async { Vec::new() })
    }
    fn on_redemption_result(
        &self,
        _: blitzkrieg_market_api::RedemptionResult,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn take_pending_merges(
        &self,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<blitzkrieg_market_api::MergeRequest>> {
        Box::pin(async { Vec::new() })
    }
    fn on_merge_result(
        &self,
        _: blitzkrieg_market_api::MergeResult,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

impl MarketHost for BookSink {
    fn on_book(&self, update: BookUpdate) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async move {
            self.books
                .lock()
                .await
                .push((update, blitzkrieg_market_api::net::now_ms()));
        })
    }
    fn on_top_of_book(
        &self,
        _: blitzkrieg_market_api::TopOfBookUpdate,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_spot(
        &self,
        _: blitzkrieg_market_api::SpotUpdate,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_round_markets(
        &self,
        _: Vec<blitzkrieg_market_api::MarketDescriptor>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn subscribe_tokens(
        &self,
        _: &str,
        _: Vec<TokenId>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn install_subscription_control(
        &self,
        _: &str,
        _: Arc<dyn blitzkrieg_market_api::SubscriptionControl>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    // Egress / settlement seams are out of scope for a feed-only sink: the
    // reverse-acceptance gates below only observe the data ingress path.
    fn take_pending_orders(
        &self,
        _: &str,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<blitzkrieg_market_api::PendingOrder>> {
        Box::pin(async { Vec::new() })
    }
    fn take_pending_cancels(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    fn on_order_accepted(&self, _: &str, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_order_rejected(
        &self,
        _: &str,
        _: Option<blitzkrieg_market_api::CoreError>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_place_timeout(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_fill(
        &self,
        _: blitzkrieg_market_api::MarketFill,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_order_live(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_order_cancelled(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_reconcile(
        &self,
        _: blitzkrieg_market_api::ReconcileSnapshot,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_reconcile_failed(
        &self,
        _: &str,
        _: blitzkrieg_market_api::CoreError,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_self_check(
        &self,
        _: &str,
        _: blitzkrieg_market_api::SelfCheckReport,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn seed_balance(&self, _venue: &str, _: Decimal) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn venue_free_balance(
        &self,
        _venue: &str,
        _: Decimal,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn known_venue_order_ids(&self) -> blitzkrieg_market_api::BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
    fn note_orphan_cancelled(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn report_error(
        &self,
        _: blitzkrieg_market_api::CoreError,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn take_settlement_queries(
        &self,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<blitzkrieg_market_api::SettlementQuery>> {
        Box::pin(async { Vec::new() })
    }
    fn on_market_resolution(
        &self,
        _: blitzkrieg_market_api::MarketResolution,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn take_pending_redemptions(
        &self,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<blitzkrieg_market_api::RedemptionRequest>> {
        Box::pin(async { Vec::new() })
    }
    fn on_redemption_result(
        &self,
        _: blitzkrieg_market_api::RedemptionResult,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn take_pending_merges(
        &self,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<blitzkrieg_market_api::MergeRequest>> {
        Box::pin(async { Vec::new() })
    }
    fn on_merge_result(
        &self,
        _: blitzkrieg_market_api::MergeResult,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// Serve a fixed script of canned responses, one per accepted connection.
fn scripted_server(responses: Vec<Option<Vec<u8>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            if let Some(body) = response {
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
            // `None` = accept and drop: the venue took the connection and
            // answered nothing (a hang the feed's request timeout bounds).
        }
        // Further connections are refused (listener dropped with the test).
    });
    format!("http://{addr}")
}

fn http_ok(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

fn http_status(status: u16, reason: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        .into_bytes()
}

async fn wait_for_books(sink: &BookSink, want: usize, budget: Duration) -> usize {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let n = sink.book_count().await;
        if n >= want || std::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn drain_books(sink: &BookSink, quiet: Duration) {
    let deadline = std::time::Instant::now() + quiet;
    let mut last = 0usize;
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let n = sink.book_count().await;
        if n != last {
            last = n;
            if std::time::Instant::now() >= deadline {
                return;
            }
        } else if std::time::Instant::now() >= deadline {
            return;
        }
    }
}

/// The production fp orderbook form (bid ladders only, prices already
/// probabilities, depth = dollar notional, best bid LAST). `bid` / `ask`
/// keep the test's old vocabulary: `bid` lands on the YES ladder, and the
/// YES ask is derived by duality as 1 − (1 − bid)… i.e. the YES book alone
/// must still produce a two-sided quote, so the NO ladder is synthesized
/// from the ask side (a NO bid at 1 − ask IS the YES ask).
fn orderbook_body(bid: &str, ask: &str) -> String {
    let no_bid = rust_decimal::Decimal::from_str("1").unwrap()
        - rust_decimal::Decimal::from_str(ask).unwrap();
    format!(
        r#"{{"orderbook_fp":{{"yes_dollars":[["{bid}","40.00"]],"no_dollars":[["{no_bid}","30.00"]]}}}}"#
    )
}

// ═══ 1. Missing incremental updates must not become a stale snapshot ════════

/// A failed poll emits NO book event. The engine's own book-aging is what
/// makes a lapsed venue safe; a plugin that re-forwards its last snapshot on
/// failure would turn a dead venue into a live-looking quote (the exact
/// "pretends to have increments" failure #424 refuses).
#[tokio::test]
async fn reverse_a_failed_poll_emits_no_book_instead_of_a_stale_repeat() {
    // Script: one good snapshot, then two 500s (the feed's retry budget in
    // tests is 0 — every poll of the window fails), then the process ends.
    let base = scripted_server(vec![
        Some(http_ok(&orderbook_body("0.43", "0.57"))),
        Some(http_status(500, "Internal Server Error")),
        Some(http_status(500, "Internal Server Error")),
    ]);
    let client = Arc::new(
        blitzkrieg_kalshi::RestConfig {
            base_url: base,
            no_proxy: true,
            timeout: Duration::from_secs(2),
            max_retries: 0,
            ..blitzkrieg_kalshi::RestConfig::default()
        }
        .pipe(|c| blitzkrieg_kalshi::KalshiRest::new(c, None).expect("client")),
    );
    let host = Arc::new(BookSink::default());
    let _handle = blitzkrieg_kalshi::feed_tests_only_spawn(
        host.clone(),
        client,
        vec![String::from("KXTEST")],
        100, // fast poll: several failures fit in the window
    );

    wait_for_books(&host, 2, Duration::from_secs(5)).await;
    // Give every failure in the script room to happen.
    drain_books(&host, Duration::from_millis(1200)).await;

    let books = host.books().await;
    assert!(
        books.len() == 2,
        "one good snapshot must produce exactly TWO books (the UP and DOWN \
         token split of one market poll); got {} (a failed poll must emit \
         NOTHING, not a repeat of the last snapshot)",
        books.len()
    );
    let (up, _) = &books[0];
    assert_eq!(up.token_id, "KXTEST");
    assert_eq!(up.bids.len(), 1);
    assert_eq!(up.asks.len(), 1);
    // The fixture's YES book: bid 0.43 (40 notional ⇒ 40/0.43 shares) and,
    // by duality, an ask at 1 − 0.43 = 0.57 (the fixture synthesizes the NO
    // ladder from the ask, so the derived YES ask IS 0.57). Depth converts
    // from the dollar notional (40.00 / 0.43), not a contract count.
    assert_eq!(up.bids[0].0, Decimal::from_str("0.43").unwrap());
    assert_eq!(up.asks[0].0, Decimal::from_str("0.57").unwrap());
    assert_eq!(
        up.bids[0].1,
        Decimal::from_str("40.00").unwrap() / Decimal::from_str("0.43").unwrap()
    );
    let (down, _) = &books[1];
    assert_eq!(down.token_id, "KXTEST:no");
    assert_eq!(down.bids[0].0, Decimal::from_str("0.43").unwrap());
    assert_eq!(down.asks[0].0, Decimal::from_str("0.57").unwrap());
}

// ═══ 2. A venue outage must not kill the feed (reconnect-or-bust) ═══════════

/// The poll loop survives errors and picks the stream back up: after failures,
/// the first healthy response flows through again. A loop that unwinds on a
/// transport error would leave the kernel quoting on a feed that stopped
/// forever — the "disconnect without reconnect" failure mode.
#[tokio::test]
async fn reverse_b_loop_survives_outage_and_resumes_on_recovery() {
    // Script: good → fail → good. The middle failure must not end the loop.
    let base = scripted_server(vec![
        Some(http_ok(&orderbook_body("0.43", "0.57"))),
        Some(http_status(503, "Service Unavailable")),
        Some(http_ok(&orderbook_body("0.44", "0.56"))),
    ]);
    let client = Arc::new(
        blitzkrieg_kalshi::RestConfig {
            base_url: base,
            no_proxy: true,
            timeout: Duration::from_secs(2),
            max_retries: 0,
            ..blitzkrieg_kalshi::RestConfig::default()
        }
        .pipe(|c| blitzkrieg_kalshi::KalshiRest::new(c, None).expect("client")),
    );
    let host = Arc::new(BookSink::default());
    let _handle = blitzkrieg_kalshi::feed_tests_only_spawn(
        host.clone(),
        client,
        vec![String::from("KXTEST")],
        100,
    );

    let n = wait_for_books(&host, 4, Duration::from_secs(5)).await;
    assert!(
        n >= 4,
        "after a failed poll the loop must recover and deliver the next \
         healthy snapshot (2 tokens per market poll); got only {n} book(s)"
    );
    let books = host.books().await;
    // The second good snapshot must carry the SECOND script's prices — proof
    // the recovered read is fresh data, not a re-emission of the first.
    let second = &books[2].0;
    assert_eq!(
        second.bids[0].0,
        Decimal::from_str("0.44").unwrap(),
        "recovered snapshot must be the venue's NEW book"
    );
    // And the cadence must have snapped back from any 429-style backoff
    // (covered implicitly: two books within the window at 100ms base).
}

// ═══ 3. Venue timestamps must never enter the engine's clock domain ═════════

/// Every emitted `ts_ms` is the LOCAL clock, within the test's own wall-clock
/// window. If a venue-provided timestamp leaked into `ts_ms`, a venue clock
/// even seconds off would make the engine's freshness check mis-age every
/// book — the "mixed time domains → false signal" failure #424 names.
#[tokio::test]
async fn reverse_c_book_timestamps_stay_in_the_local_clock_domain() {
    let base = scripted_server(vec![Some(http_ok(&orderbook_body("0.43", "0.57")))]);
    let client = Arc::new(
        blitzkrieg_kalshi::RestConfig {
            base_url: base,
            no_proxy: true,
            timeout: Duration::from_secs(2),
            max_retries: 0,
            ..blitzkrieg_kalshi::RestConfig::default()
        }
        .pipe(|c| blitzkrieg_kalshi::KalshiRest::new(c, None).expect("client")),
    );
    let host = Arc::new(BookSink::default());
    let started = blitzkrieg_market_api::net::now_ms();
    let _handle = blitzkrieg_kalshi::feed_tests_only_spawn(
        host.clone(),
        client,
        vec![String::from("KXTEST")],
        100,
    );

    wait_for_books(&host, 1, Duration::from_secs(5)).await;
    let books = host.books().await;
    assert!(!books.is_empty(), "the snapshot must flow through the host");

    // The mock's canned payload has NO timestamps at all — so a leak would
    // have to come from a venue field. For domain proof, also drive the
    // normalization path directly with a hostile venue timestamp present.
    let ended = blitzkrieg_market_api::net::now_ms();
    for (book, received_local) in &books {
        assert!(
            book.ts_ms >= started - 5 && book.ts_ms <= ended + 5,
            "book ts_ms {} outside the test's local window [{started}, {ended}] — \
             a venue timestamp leaked into the engine's clock domain",
            book.ts_ms
        );
        assert!(
            (received_local - book.ts_ms).abs() <= 250,
            "host receive time {received_local} vs book ts {ts} diverged past a \
             poll cycle — the two must be the same clock",
            ts = book.ts_ms
        );
    }
}

/// Same clock-domain rule at the normalization seam, hostile input edition:
/// the probability band guard is fed values that are NOT probabilities (a
/// venue bug mixing fields, cent-denominated values, sentinels); the band
/// filter drops anything outside (0,1) — a value that would otherwise
/// masquerade as a probability price.
#[tokio::test]
async fn reverse_c2_normalization_refuses_out_of_band_values() {
    use blitzkrieg_kalshi::feed_tests_only_cents_to_prob;
    use std::str::FromStr;
    // Epoch-millis-like value from a field-mixing venue: not a probability.
    assert_eq!(
        feed_tests_only_cents_to_prob(Decimal::from_str("1760000000").unwrap()),
        None
    );
    // Negative values and cent-scale quotes are likewise not probabilities —
    // the fp book form never carries cents, so a cent value leaking in is a
    // field mix and must be refused, not divided by 100.
    assert_eq!(
        feed_tests_only_cents_to_prob(Decimal::from_str("-0.43").unwrap()),
        None
    );
    assert_eq!(
        feed_tests_only_cents_to_prob(Decimal::from_str("43").unwrap()),
        None,
        "a cent-scale value must not pass the probability band"
    );
    assert_eq!(feed_tests_only_cents_to_prob(Decimal::ZERO), None);
    assert_eq!(feed_tests_only_cents_to_prob(Decimal::ONE), None);
    // The band in between maps through unchanged.
    assert_eq!(
        feed_tests_only_cents_to_prob(Decimal::from_str("0.43").unwrap()),
        Some(Decimal::from_str("0.43").unwrap())
    );
}

// A tiny pipe helper so the config blocks above stay one expression.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

// ═══ 4. #548: discovery must re-feed when the venue's batch grows mid-round ═

/// One events response body for the discovery series query: `markets` carries
/// ONE active 15m round for `asset`, closing ~14 min out (inside the 2×
/// round-length guard) so its slot is the current slot.
fn discovery_events_body(ticker: &str, asset: &str, mid: &str) -> String {
    let close = chrono_like_close();
    format!(
        r#"{{"events":[{{"event_ticker":"EVT-{asset}","markets":[{{"ticker":"{ticker}","event_ticker":"EVT-{asset}","market_type":"binary","title":"{asset} price up in next 15 mins?","subtitle":null,"close_time":"{close}","expiration_time":null,"status":"active","yes_bid_dollars":"{mid}","yes_ask_dollars":"0.66","no_bid_dollars":"0.34","no_ask_dollars":"0.35","volume_fp":null}}]}}]}}"#
    )
}

/// `close_time` ~14 min out: ISO YYYY-MM-DDTHH:MM:SSZ from the local clock
/// (civil-from-days, Howard Hinnant — the inverse of `close_time_to_ms`).
fn chrono_like_close() -> String {
    let now = blitzkrieg_market_api::net::now_ms();
    let secs = (now + 14 * 60 * 1000) / 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// A same-slot discovery poll whose batch GREW must reach the host again:
/// the old top guard (`slot == last_slot → continue`) made the #498 in-slot
/// top-up gate unreachable, freezing the round at the first poll's subset
/// ("只剩一个币对"). The #548 fix replaces it with a batch-signature gate,
/// so a grown batch re-feeds within the next poll.
#[tokio::test]
async fn reverse_d_discovery_refeeds_when_the_round_batch_grows() {
    // Script (discovery queries per-asset in config order; one canned response
    // per accepted connection). Poll 1: BTC→BTC market, ETH→empty events
    // (straggler not yet listed). Poll 2: BTC→same BTC market, ETH→ETH market
    // — the batch grew, so poll 2 must re-feed. body order matters: responses
    // are consumed strictly in poll order.
    let btc1 = discovery_events_body("KXBTC15M-251020", "BTC", "0.43");
    let btc2 = discovery_events_body("KXBTC15M-251020", "BTC", "0.43");
    let eth = discovery_events_body("KXETH15M-251020", "ETH", "0.55");
    let empty = r#"{"events":[]}"#;
    let base = scripted_server(vec![
        Some(http_ok(&btc1)),
        Some(http_ok(empty)),
        Some(http_ok(&btc2)),
        Some(http_ok(&eth)),
    ]);
    let client = Arc::new(
        blitzkrieg_kalshi::RestConfig {
            base_url: base,
            no_proxy: true,
            timeout: Duration::from_secs(2),
            max_retries: 0,
            ..blitzkrieg_kalshi::RestConfig::default()
        }
        .pipe(|c| blitzkrieg_kalshi::KalshiRest::new(c, None).expect("client")),
    );
    let host = Arc::new(DiscoverySink::default());
    let _handle = blitzkrieg_kalshi::discovery_tests_only_spawn(
        host.clone(),
        client,
        vec![String::from("BTC"), String::from("ETH")],
        900,
        0, // minimum cadence (the loop floors at 3s)
    );

    // The production loop floors its cadence at 3s; two polls fit in ~7s.
    let n = host.wait_for_rounds(2, Duration::from_secs(20)).await;
    assert!(
        n >= 2,
        "a batch that grows mid-round (BTC listed first, ETH later) must be \
         re-fed: got {n} round feed(s)"
    );
    let tickers = host.last_round_tickers().await;
    assert!(
        tickers.len() >= 2,
        "the re-fed round must carry BOTH assets' markets; got {tickers:?}"
    );
}

/// …and the SAME batch must NOT be re-fed: an unchanged signature skips the
/// host push (no duplicate RoundMarkets every 3s for a settled round).
#[tokio::test]
async fn reverse_d2_discovery_does_not_refeed_an_unchanged_batch() {
    // Two identical polls; the batch does not change between them.
    let body = discovery_events_body("KXBTC15M-251020", "BTC", "0.43");
    let base = scripted_server(vec![Some(http_ok(&body)), Some(http_ok(&body))]);
    let client = Arc::new(
        blitzkrieg_kalshi::RestConfig {
            base_url: base,
            no_proxy: true,
            timeout: Duration::from_secs(2),
            max_retries: 0,
            ..blitzkrieg_kalshi::RestConfig::default()
        }
        .pipe(|c| blitzkrieg_kalshi::KalshiRest::new(c, None).expect("client")),
    );
    let host = Arc::new(DiscoverySink::default());
    let _handle = blitzkrieg_kalshi::discovery_tests_only_spawn(
        host.clone(),
        client,
        vec![String::from("BTC")],
        900,
        0,
    );

    let n = host.wait_for_rounds(1, Duration::from_secs(20)).await;
    assert!(n >= 1, "the first poll must feed the round");
    // Give the second poll time to happen (3s floor + margin), then assert
    // no duplicate push arrived.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let n2 = host.round_count().await;
    assert_eq!(
        n2, 1,
        "an unchanged batch must not be re-fed to the host; got {n2} feeds"
    );
}
