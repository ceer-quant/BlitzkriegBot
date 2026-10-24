//! Plugin reverse-acceptance tests (PR A / #454, #455).
//!
//! Each test drives the REAL feed loop (or the real normalization path) via
//! `feed_tests_only_spawn` against a 127.0.0.1 mock and asserts a named
//! failure mode CANNOT pass silently:
//!
//! 1. **missing incremental updates** — this feed is REST snapshot polling,
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
//!    the venue's clock drifts. The test drives the loop over a mock whose
//!    payload carries a hostile far-future `timestamp` and asserts every
//!    emitted `ts_ms` falls within the local wall-clock window of the test.
//! 4. **the SIMULATED capability bit** — the plugin must declare
//!    `LEVEL2_SNAPSHOT | SIMULATED` so the UI can badge it 模拟盘（第三方）;
//!    the bit must come from the plugin's own declaration, not a venue-name
//!    branch anywhere in the kernel (no-venue-branch rule).

use blitzkrieg_market_api::{BookUpdate, MarketCapabilities, MarketHost, TokenId};
use blitzkrieg_polysimulator::{
    PolySimRest, PolySimulatorPlugin, RestConfig, feed_tests_only_band_to_prob,
    feed_tests_only_spawn,
};
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
    http_status(200, "OK", body)
}

fn http_status(status: u16, reason: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
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

fn client_for(base_url: &str) -> Arc<PolySimRest> {
    let config = rest_config(base_url);
    Arc::new(PolySimRest::new(config, None).expect("client"))
}

fn rest_config(base: &str) -> RestConfig {
    RestConfig {
        base_url: base.to_string(),
        no_proxy: true,
        timeout: Duration::from_secs(2),
        max_retries: 0,
        ..RestConfig::default()
    }
}

fn book_body(bid: &str, ask: &str) -> String {
    format!(
        r#"{{"bids":[{{"price":"{bid}","size":"40"}}],"asks":[{{"price":"{ask}","size":"30"}}],"timestamp":"99999999999999"}}"#
    )
}

// ═══ 1. Missing incremental updates must not become a stale snapshot ════════

/// A failed poll emits NO book event. The engine's own book-aging is what
/// makes a lapsed venue safe; a plugin that re-forwards its last snapshot on
/// failure would turn a dead venue into a live-looking quote (the exact
/// "pretends to have increments" failure the capability contract refuses).
#[tokio::test]
async fn reverse_a_failed_poll_emits_no_book_instead_of_a_stale_repeat() {
    // Script: one good snapshot, then two 500s (the feed's retry budget in
    // tests is 0 — every poll of the window fails), then the process ends.
    let base = scripted_server(vec![
        Some(http_ok(&book_body("0.43", "0.57"))),
        Some(http_status(500, "Internal Server Error", "")),
        Some(http_status(500, "Internal Server Error", "")),
    ]);
    let host = Arc::new(BookSink::default());
    let _handle = feed_tests_only_spawn(
        host.clone(),
        client_for(&base),
        vec![TokenId::from("71321")],
        250, // floor cadence: several failures fit in the window
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
    assert_eq!(book.token_id, "71321");
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
        Some(http_ok(&book_body("0.43", "0.57"))),
        Some(http_status(503, "Service Unavailable", "")),
        Some(http_ok(&book_body("0.44", "0.56"))),
    ]);
    let host = Arc::new(BookSink::default());
    let _handle = feed_tests_only_spawn(
        host.clone(),
        client_for(&base),
        vec![TokenId::from("71321")],
        250,
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
        Decimal::from_str("0.44").unwrap(),
        "recovered snapshot must be the venue's NEW book"
    );
}

// ═══ 3. Venue timestamps must never enter the engine's clock domain ═════════

/// Every emitted `ts_ms` is the LOCAL clock, within the test's own wall-clock
/// window. The mock's payload carries a far-future `timestamp` field; if a
/// venue-provided timestamp leaked into `ts_ms`, the engine's freshness check
/// would mis-age every book — the "mixed time domains → false signal" failure.
#[tokio::test]
async fn reverse_c_book_timestamps_stay_in_the_local_clock_domain() {
    let base = scripted_server(vec![Some(http_ok(&book_body("0.43", "0.57")))]);
    let host = Arc::new(BookSink::default());
    let started = blitzkrieg_market_api::net::now_ms();
    let _handle = feed_tests_only_spawn(
        host.clone(),
        client_for(&base),
        vec![TokenId::from("71321")],
        250,
    );

    wait_for_books(&host, 1, Duration::from_secs(5)).await;
    let books = host.books().await;
    assert!(!books.is_empty(), "the snapshot must flow through the host");

    let ended = blitzkrieg_market_api::net::now_ms();
    for (book, received_local) in &books {
        assert!(
            book.ts_ms >= started - 5 && book.ts_ms <= ended + 5,
            "book ts_ms {} outside the test's local window [{started}, {ended}] — \
             the venue timestamp (99999999999999) leaked into the engine's clock domain",
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

// ═══ 3b. Band filter at the normalization seam, hostile values ══════════════

/// The venue quotes dollars 0..1, so no unit conversion — but a field-mixing
/// venue bug can still put epoch-like magnitudes, negatives or 1.0 into a
/// price field; the band filter must drop them, never forward.
#[tokio::test]
async fn reverse_c2_normalization_refuses_out_of_band_values() {
    // Epoch-millis-like value from a field-mixing venue: not a probability.
    assert_eq!(
        feed_tests_only_band_to_prob(Decimal::from_str("99999999999999").unwrap()),
        None
    );
    // Negative and ≥1 quotes are likewise not prices.
    assert_eq!(
        feed_tests_only_band_to_prob(Decimal::from_str("-0.4").unwrap()),
        None
    );
    assert_eq!(
        feed_tests_only_band_to_prob(Decimal::from_str("1.57").unwrap()),
        None
    );
    assert_eq!(feed_tests_only_band_to_prob(Decimal::ONE), None);
    // The whole band in between maps through UNCHANGED (no unit conversion).
    assert_eq!(
        feed_tests_only_band_to_prob(Decimal::from_str("0.43").unwrap()),
        Some(Decimal::from_str("0.43").unwrap())
    );
}

// ═══ 4. The SIMULATED capability bit is the plugin's own declaration ════════

/// The UI badges this venue 模拟盘（第三方） off the capability NAMES the
/// plugin declares — which means the bit must ride the declaration itself.
/// If the plugin stopped declaring SIMULATED, the panel would present a
/// simulated venue as a live one (the presentation failure #455 refuses),
/// with no venue-name branch anywhere to catch it.
#[test]
fn the_plugin_declares_snapshot_plus_simulated_and_nothing_venue_special() {
    let plugin = PolySimulatorPlugin::new();
    let modes = blitzkrieg_market_api::MarketPlugin::declare_modes(&plugin);
    assert_eq!(modes.len(), 1, "one mode: the 15m up/down round");
    let caps = modes[0].capabilities;
    assert!(
        caps.0 & MarketCapabilities::LEVEL2_SNAPSHOT.0 != 0,
        "the feed is an L2 snapshot poller — the engine must age books accordingly"
    );
    assert!(
        caps.0 & MarketCapabilities::SIMULATED.0 != 0,
        "the simulated bit MUST be declared: it is the only thing standing between \
         the UI and presenting a simulator as a live venue"
    );
    // And the declared names must include the badge key the UI renders.
    let names: Vec<&str> = blitzkrieg_market_api::modes::CAPABILITY_NAMES
        .iter()
        .filter(|(bit, _)| caps.0 & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    assert!(
        names.contains(&"simulated"),
        "capability names must render 'simulated' for the declared bit; got {names:?}"
    );
}
