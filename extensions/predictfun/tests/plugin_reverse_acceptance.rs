//! Stage-2 plugin reverse-acceptance tests for predict.fun (#424).
//!
//! Mirror of the Kalshi suite (`extensions/kalshi/tests/plugin_reverse_acceptance.rs`)
//! against the predict.fun feed: the three #424 failure modes must be provably
//! refused on BOTH venues.
//!
//! 1. a failed poll emits NO book (a stale snapshot never masquerades fresh);
//! 2. the poll loop survives an outage and resumes on recovery;
//! 3. every emitted `ts_ms` is the LOCAL clock (no venue timestamp leaks).

use blitzkrieg_market_api::{
    BookUpdate, CoreError, MarketDescriptor, MarketFill, MarketHost, ReconcileSnapshot,
    RedemptionRequest, RedemptionResult, SelfCheckReport, SettlementQuery, TokenId,
};
use rust_decimal::Decimal;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

// ── shared harness ───────────────────────────────────────────────────────────

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
        _: Vec<MarketDescriptor>,
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
        _: Option<CoreError>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_fill(&self, _: MarketFill) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_order_live(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_order_cancelled(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_reconcile(&self, _: ReconcileSnapshot) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_reconcile_failed(&self, _: CoreError) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_self_check(&self, _: SelfCheckReport) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
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
    fn report_error(&self, _: CoreError) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn take_settlement_queries(
        &self,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<SettlementQuery>> {
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
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<RedemptionRequest>> {
        Box::pin(async { Vec::new() })
    }
    fn on_redemption_result(
        &self,
        _: RedemptionResult,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

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
        }
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

fn orderbook_body(bid: &str, ask: &str) -> String {
    format!(
        r#"{{"success":true,"data":{{"token_id":"0xtok","bids":[{{"price":"{bid}","shares":"100"}}],"asks":[{{"price":"{ask}","shares":"80"}}]}}}}"#
    )
}

fn client_for(base: String) -> Arc<blitzkrieg_predictfun::PredictRest> {
    let config = blitzkrieg_predictfun::RestConfig {
        base_url: base,
        no_proxy: true,
        timeout: Duration::from_secs(2),
        max_retries: 0,
        ..blitzkrieg_predictfun::RestConfig::default()
    };
    Arc::new(
        blitzkrieg_predictfun::PredictRest::new(config, None)
            .expect("client builds unauthenticated"),
    )
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

// ═══ 1. Missing incremental updates must not become a stale snapshot ════════

#[tokio::test]
async fn reverse_a_failed_poll_emits_no_book_instead_of_a_stale_repeat() {
    let base = scripted_server(vec![
        Some(http_ok(&orderbook_body("0.44", "0.46"))),
        Some(http_status(500, "Internal Server Error")),
        Some(http_status(500, "Internal Server Error")),
    ]);
    let host = Arc::new(BookSink::default());
    let _handle = blitzkrieg_predictfun::feed_tests_only_spawn(
        host.clone(),
        client_for(base),
        vec![String::from("0xtok")],
        100,
    );

    wait_for_books(&host, 1, Duration::from_secs(5)).await;
    drain_books(&host, Duration::from_millis(1200)).await;

    let books = host.books().await;
    assert!(
        books.len() == 1,
        "one good snapshot must produce exactly one book; got {} (a failed poll \
         must emit NOTHING, not a repeat of the last snapshot)",
        books.len()
    );
    let (book, _) = &books[0];
    assert_eq!(book.token_id, "0xtok");
    assert_eq!(
        book.bids[0].0,
        Decimal::new(44, 2),
        "prices are 0..1 already"
    );
}

// ═══ 2. A venue outage must not kill the feed (reconnect-or-bust) ═══════════

#[tokio::test]
async fn reverse_b_loop_survives_outage_and_resumes_on_recovery() {
    let base = scripted_server(vec![
        Some(http_ok(&orderbook_body("0.44", "0.46"))),
        Some(http_status(503, "Service Unavailable")),
        Some(http_ok(&orderbook_body("0.45", "0.47"))),
    ]);
    let host = Arc::new(BookSink::default());
    let _handle = blitzkrieg_predictfun::feed_tests_only_spawn(
        host.clone(),
        client_for(base),
        vec![String::from("0xtok")],
        100,
    );

    let n = wait_for_books(&host, 2, Duration::from_secs(5)).await;
    assert!(
        n >= 2,
        "after a failed poll the loop must recover and deliver the next \
         healthy snapshot; got only {n} book(s)"
    );
    let books = host.books().await;
    let second = &books[1].0;
    assert_eq!(
        second.bids[0].0,
        Decimal::new(45, 2),
        "recovered snapshot must be the venue's NEW book"
    );
}

// ═══ 3. Venue timestamps must never enter the engine's clock domain ═════════

#[tokio::test]
async fn reverse_c_book_timestamps_stay_in_the_local_clock_domain() {
    let base = scripted_server(vec![Some(http_ok(&orderbook_body("0.44", "0.46")))]);
    let host = Arc::new(BookSink::default());
    let started = blitzkrieg_market_api::net::now_ms();
    let _handle = blitzkrieg_predictfun::feed_tests_only_spawn(
        host.clone(),
        client_for(base),
        vec![String::from("0xtok")],
        100,
    );

    wait_for_books(&host, 1, Duration::from_secs(5)).await;
    let books = host.books().await;
    assert!(!books.is_empty(), "the snapshot must flow through the host");

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

/// The predict.fun normalization seam: prices are probabilities already, so
/// the ONLY defense against a field-mixing venue is the degenerate-band
/// filter — out-of-band values (epoch-millis-sized, negative, ≥1) are dropped.
#[tokio::test]
async fn reverse_c2_normalization_refuses_out_of_band_values() {
    use blitzkrieg_predictfun::feed_tests_only_prob_in_band;
    use std::str::FromStr;
    assert_eq!(
        feed_tests_only_prob_in_band(Decimal::from_str("1760000000").unwrap()),
        None,
        "an epoch-millis-sized value is not a probability"
    );
    assert_eq!(
        feed_tests_only_prob_in_band(Decimal::from_str("-0.2").unwrap()),
        None
    );
    assert_eq!(
        feed_tests_only_prob_in_band(Decimal::from_str("1.5").unwrap()),
        None
    );
    assert_eq!(
        feed_tests_only_prob_in_band(Decimal::from_str("0.43").unwrap()),
        Some(Decimal::from_str("0.43").unwrap())
    );
}
