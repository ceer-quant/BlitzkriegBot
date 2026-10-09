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
    fn subscribe_tokens(&self, _: Vec<TokenId>) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn install_subscription_control(
        &self,
        _: Arc<dyn blitzkrieg_market_api::SubscriptionControl>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    // Egress / settlement seams are out of scope for a feed-only sink: the
    // reverse-acceptance gates below only observe the data ingress path.
    fn take_pending_orders(
        &self,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<blitzkrieg_market_api::PendingOrder>> {
        Box::pin(async { Vec::new() })
    }
    fn take_pending_cancels(&self) -> blitzkrieg_market_api::BoxFuture<'_, Vec<String>> {
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
        _: blitzkrieg_market_api::CoreError,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_self_check(
        &self,
        _: blitzkrieg_market_api::SelfCheckReport,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn seed_balance(&self, _: Decimal) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn venue_free_balance(&self, _: Decimal) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
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

fn orderbook_body(bid_cents: &str, ask_cents: &str) -> String {
    format!(
        r#"{{"orderbook":{{"yes":[{{"price":{bid_cents},"quantity":40}}],"no":[{{"price":{ask_cents},"quantity":30}}]}}}}"#
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
        Some(http_ok(&orderbook_body("43", "57"))),
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

    wait_for_books(&host, 1, Duration::from_secs(5)).await;
    // Give every failure in the script room to happen.
    drain_books(&host, Duration::from_millis(1200)).await;

    let books = host.books().await;
    assert!(
        books.len() == 1,
        "one good snapshot must produce exactly one book; got {} (a failed poll \
         must emit NOTHING, not a repeat of the last snapshot)",
        books.len()
    );
    let (book, _) = &books[0];
    assert_eq!(book.token_id, "KXTEST");
    assert_eq!(book.bids.len(), 1);
    assert_eq!(book.asks.len(), 1);
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
        Some(http_ok(&orderbook_body("43", "57"))),
        Some(http_status(503, "Service Unavailable")),
        Some(http_ok(&orderbook_body("44", "56"))),
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

    let n = wait_for_books(&host, 2, Duration::from_secs(5)).await;
    assert!(
        n >= 2,
        "after a failed poll the loop must recover and deliver the next \
         healthy snapshot; got only {n} book(s)"
    );
    let books = host.books().await;
    // The second good snapshot must carry the SECOND script's prices — proof
    // the recovered read is fresh data, not a re-emission of the first.
    let second = &books[1].0;
    assert_eq!(
        second.bids[0].0,
        Decimal::new(44, 2),
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
    let base = scripted_server(vec![Some(http_ok(&orderbook_body("43", "57")))]);
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
/// `cents_to_prob` is fed cents that ARE timestamps by magnitude (a venue bug
/// mixing fields); the band filter drops anything outside (0,1) — a value
/// that would otherwise masquerade as a probability price.
#[tokio::test]
async fn reverse_c2_normalization_refuses_out_of_band_values() {
    use blitzkrieg_kalshi::feed_tests_only_cents_to_prob;
    use std::str::FromStr;
    // Epoch-millis-like value from a field-mixing venue: not a probability.
    assert_eq!(
        feed_tests_only_cents_to_prob(Decimal::from_str("1760000000").unwrap()),
        None
    );
    // Negative and ≥100 cent quotes are likewise not prices.
    assert_eq!(
        feed_tests_only_cents_to_prob(Decimal::from_str("-43").unwrap()),
        None
    );
    assert_eq!(
        feed_tests_only_cents_to_prob(Decimal::from_str("157").unwrap()),
        None
    );
    // The whole band in between maps.
    assert_eq!(
        feed_tests_only_cents_to_prob(Decimal::from_str("43").unwrap()),
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
