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
    BookUpdate, CoreError, MarketDescriptor, MarketFill, MarketHost, MergeRequest, MergeResult,
    ReconcileSnapshot, RedemptionRequest, RedemptionResult, SelfCheckReport, SettlementQuery,
    TokenId,
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
        _: Option<CoreError>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_place_timeout(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
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
    fn on_reconcile_failed(
        &self,
        _: &str,
        _: CoreError,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_self_check(
        &self,
        _: &str,
        _: SelfCheckReport,
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
    fn take_pending_merges(&self) -> blitzkrieg_market_api::BoxFuture<'_, Vec<MergeRequest>> {
        Box::pin(async { Vec::new() })
    }
    fn on_merge_result(&self, _: MergeResult) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
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

/// Rounds pushed through the host (#899 gate: the discovery loop must re-feed
/// when the venue's batch grows mid-round, and NOT re-feed an unchanged batch).
#[derive(Clone, Default)]
struct DiscoverySink {
    rounds: Arc<Mutex<Vec<Vec<MarketDescriptor>>>>,
}

impl DiscoverySink {
    async fn round_count(&self) -> usize {
        self.rounds.lock().await.len()
    }

    async fn rounds(&self) -> Vec<Vec<MarketDescriptor>> {
        self.rounds.lock().await.clone()
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
        markets: Vec<MarketDescriptor>,
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
    // Egress / settlement seams are out of scope for a discovery-only sink:
    // the #899 gate below only observes the round-ingress path.
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
        _: Option<CoreError>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_place_timeout(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
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
    fn on_reconcile_failed(
        &self,
        _: &str,
        _: CoreError,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_self_check(
        &self,
        _: &str,
        _: SelfCheckReport,
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
    fn take_pending_merges(&self) -> blitzkrieg_market_api::BoxFuture<'_, Vec<MergeRequest>> {
        Box::pin(async { Vec::new() })
    }
    fn on_merge_result(&self, _: MergeResult) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
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

// ═══ 4. #899: discovery must re-feed when the round batch grows mid-round ═══

/// One predict.fun market row for the discovery `GET /v1/markets` mock: the
/// UP/DOWN sibling of `condition_id`, quoted 0..1, ending ~14 min out (so the
/// discovery loop's `end_ms <= now` skip never fires) with no `status` field
/// (absent = live, the venue's list-endpoint shape).
fn market_row(token_id: &str, condition_id: &str) -> String {
    let end = chrono_like_end();
    format!(
        r#"{{"token_id":"{token_id}","question":"Will Bitcoin close above $100k?","condition_id":"{condition_id}","end_date":"{end}","best_bid":"0.44","best_ask":"0.46"}}"#
    )
}

/// One tradeable pair (two outcome rows sharing `condition_id`) for the mock.
/// Both rows carry a two-sided 0.44/0.46 quote, so each side's own mid exists.
fn btc_pair(condition_id: &str) -> Vec<String> {
    vec![
        market_row(&format!("0xup-{condition_id}"), condition_id),
        market_row(&format!("0xdown-{condition_id}"), condition_id),
    ]
}

/// The `/v1/markets` response envelope wrapping the given rows.
fn discovery_markets_body(rows: &[String]) -> String {
    format!(
        r#"{{"success":true,"data":{{"markets":[{}]}}}}"#,
        rows.join(",")
    )
}

/// ISO YYYY-MM-DDTHH:MM:SSZ, ~14 min out, from the local clock
/// (civil-from-days, Howard Hinnant — the inverse of `end_time_to_ms`).
fn chrono_like_end() -> String {
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

/// Same boundary guard as kalshi's reverse_d2 (issue 896 lesson): the loop
/// keys its dedupe on the wall-clock round slot (`now / 1000 / round_sec`).
/// If a poll crossed a `round_sec` boundary mid-test, re-feeding (or not) is
/// correct by design and the assertion would red for a timing reason only.
/// Start just after a boundary so the test's ~10s window stays in one slot;
/// when already within 30s of the boundary, sleep just past it (max 30s).
async fn align_to_round_slot(round_sec: u64) {
    let into_slot = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs()
        % round_sec;
    if into_slot > round_sec - 30 {
        tokio::time::sleep(Duration::from_secs(round_sec + 1 - into_slot)).await;
    }
}

/// A same-slot discovery poll whose batch GREW must reach the host again:
/// the old top guard (`slot == last_slot → continue`) froze the round at
/// whatever subset the first poll caught, so a market listed mid-round
/// (staggered listing / 扩批) never traded that round (#498 同族冻结 bug，
/// kalshi 侧 #548 已修、predictfun 漏了). The #899 fix replaces it with a
/// slot+batch-signature gate, so a grown batch re-feeds on the next poll.
#[tokio::test]
async fn reverse_d_discovery_refeeds_when_the_round_batch_grows() {
    align_to_round_slot(900).await;

    // Poll 1: one pair (condition 0xa1). Poll 2: the same pair PLUS a second
    // (0xb2) — the batch grew mid-round. Responses are consumed strictly in
    // poll order (one canned body per accepted connection).
    let poll1 = discovery_markets_body(&btc_pair("0xa1"));
    let mut rows = btc_pair("0xa1");
    rows.extend(btc_pair("0xb2"));
    let poll2 = discovery_markets_body(&rows);
    let base = scripted_server(vec![Some(http_ok(&poll1)), Some(http_ok(&poll2))]);
    let host = Arc::new(DiscoverySink::default());
    let _handle = blitzkrieg_predictfun::discovery_tests_only_spawn(
        host.clone(),
        client_for(base),
        vec![String::from("BTC")],
        900, // round_sec is injected (predict.fun has no series family gate)
        0,   // minimum cadence (the loop floors at 3s)
    );

    // The production loop floors its cadence at 3s; two polls fit in ~7s.
    let n = host.wait_for_rounds(2, Duration::from_secs(20)).await;
    assert!(
        n >= 2,
        "a batch that grows mid-round (a market listed between polls) must be \
         re-fed: got {n} round feed(s)"
    );
    let feeds = host.rounds().await;
    assert_eq!(
        feeds[0].len(),
        1,
        "the first poll must feed exactly what was listed then (one pair)"
    );
    assert_eq!(
        feeds[1].len(),
        2,
        "the re-fed round must carry the FULL grown batch (both pairs)"
    );
}

/// …and the SAME batch must NOT be re-fed: an unchanged signature skips the
/// host push (no duplicate RoundMarkets every 3s for a settled round). Guards
/// against regressing the fix into plain no-dedup.
#[tokio::test]
async fn reverse_d2_discovery_does_not_refeed_an_unchanged_batch() {
    align_to_round_slot(900).await;

    // Two identical polls; the batch does not change between them.
    let body = discovery_markets_body(&btc_pair("0xa1"));
    let base = scripted_server(vec![Some(http_ok(&body)), Some(http_ok(&body))]);
    let host = Arc::new(DiscoverySink::default());
    let _handle = blitzkrieg_predictfun::discovery_tests_only_spawn(
        host.clone(),
        client_for(base),
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
