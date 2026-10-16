//! Order-executor integration tests (stage 4 completion).
//!
//! Every test binds a 127.0.0.1 mock server and drives the REAL executor loop
//! (`executor_tests_only_spawn` — the same `executor_loop` the plugin's
//! `OrderExecutor::start` reaches) through a client built with a
//! programmatically-generated throwaway RSA key. No test touches the network
//! beyond loopback and no credential material is fixed in source.
//!
//! The mock is ROUTED, not sequenced: the executor polls on its own cadence
//! (startup + a sweep every ~5s), so a canned-in-order server would race the
//! polls and hand a sweep a body meant for another endpoint. Instead each
//! request path always gets the same body, and only the one-shot mutations
//! (place POST, cancel DELETE) consume an ack from a queue.
//!
//! Covered shapes (reverse acceptance where noted):
//! 1. **no credentials → INERT** — the production spawn answers `Ok(None)`
//!    and never opens a socket; dry-run keeps an honest `has_executor`
//!    without order egress.
//! 2. **place path** — a pending order the host hands over is submitted as a
//!    POST whose body carries the ticker/side/count/cents mapping, and the
//!    venue's id comes back through `on_order_accepted` with the core's id
//!    preserved (the idempotency key). REMOVE the mapping and the assert on
//!    the recorded body goes red.
//! 3. **reconcile sweep** — fills land in `ReconcileSnapshot` rows carrying
//!    the discovery convention's token ids (`ticker` / `ticker:no`) and
//!    cent→probability prices; a venue-open id the core does not know (an
//!    orphan) is DELETEd; the balance flows to `venue_free_balance`.
//! 4. **cancel-first serialization** — a pending cancel is DELETEd before the
//!    same tick's pending order is POSTed (the #462 rule).
//! 5. **unreadable fill rows** are skipped, never forwarded as garbage.

use blitzkrieg_kalshi::rest::Credentials;
use blitzkrieg_kalshi::{KalshiRest, RestConfig, executor};
use blitzkrieg_market_api::{
    BookUpdate, CoreError, FillPolicy, MarketFill, MarketHost, MarketResolution, MergeRequest,
    MergeResult, PendingOrder, ReconcileSnapshot, RedemptionRequest, RedemptionResult,
    SelfCheckReport, SettlementQuery, Side, SpotUpdate, SubscriptionControl, TokenId,
    TopOfBookUpdate,
};
use rust_decimal::Decimal;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;

// ── harness: a MarketHost sink that records what the executor does ──────

type Shared<T> = Arc<Mutex<T>>;
type Rejections = Vec<(String, Option<CoreError>)>;

#[derive(Clone, Default)]
struct ExecSink {
    accepted: Shared<Vec<(String, String)>>,
    rejected: Shared<Rejections>,
    fills: Shared<Vec<MarketFill>>,
    reconciles: Shared<Vec<ReconcileSnapshot>>,
    orphan_cancels: Shared<Vec<String>>,
    balances: Shared<Vec<Decimal>>,
    self_checks: Shared<Vec<SelfCheckReport>>,
    errors: Shared<Vec<CoreError>>,
    pending_orders: Shared<Vec<PendingOrder>>,
    pending_cancels: Shared<Vec<String>>,
    known_ids: Shared<Vec<String>>,
}

impl ExecSink {
    async fn push_pending(&self, order: PendingOrder) {
        self.pending_orders.lock().await.push(order);
    }
    async fn push_cancel(&self, id: &str) {
        self.pending_cancels.lock().await.push(id.to_string());
    }
}

impl MarketHost for ExecSink {
    fn on_book(&self, _: BookUpdate) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_top_of_book(&self, _: TopOfBookUpdate) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_spot(&self, _: SpotUpdate) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
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
        _: Arc<dyn SubscriptionControl>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn take_pending_orders(&self) -> blitzkrieg_market_api::BoxFuture<'_, Vec<PendingOrder>> {
        Box::pin(async { std::mem::take(&mut *self.pending_orders.lock().await) })
    }
    fn take_pending_cancels(&self) -> blitzkrieg_market_api::BoxFuture<'_, Vec<String>> {
        Box::pin(async { std::mem::take(&mut *self.pending_cancels.lock().await) })
    }
    fn on_order_accepted(
        &self,
        core_id: &str,
        venue_id: &str,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        let pair = (core_id.to_string(), venue_id.to_string());
        Box::pin(async move {
            self.accepted.lock().await.push(pair);
        })
    }
    fn on_order_rejected(
        &self,
        core_id: &str,
        error: Option<CoreError>,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        let core_id = core_id.to_string();
        Box::pin(async move {
            self.rejected.lock().await.push((core_id, error));
        })
    }
    fn on_place_timeout(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_fill(&self, fill: MarketFill) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async move {
            self.fills.lock().await.push(fill);
        })
    }
    fn on_order_live(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_order_cancelled(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_reconcile(
        &self,
        snapshot: ReconcileSnapshot,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async move {
            self.reconciles.lock().await.push(snapshot);
        })
    }
    fn on_reconcile_failed(&self, _: CoreError) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_self_check(&self, report: SelfCheckReport) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async move {
            self.self_checks.lock().await.push(report);
        })
    }
    fn seed_balance(&self, balance: Decimal) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async move {
            self.balances.lock().await.push(balance);
        })
    }
    fn venue_free_balance(&self, free: Decimal) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async move {
            self.balances.lock().await.push(free);
        })
    }
    fn known_venue_order_ids(&self) -> blitzkrieg_market_api::BoxFuture<'_, Vec<String>> {
        Box::pin(async { self.known_ids.lock().await.clone() })
    }
    fn note_orphan_cancelled(
        &self,
        venue_order_id: &str,
    ) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        let id = venue_order_id.to_string();
        Box::pin(async move {
            self.orphan_cancels.lock().await.push(id);
        })
    }
    fn report_error(&self, error: CoreError) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async move {
            self.errors.lock().await.push(error);
        })
    }
    fn take_settlement_queries(
        &self,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<SettlementQuery>> {
        Box::pin(async { Vec::new() })
    }
    fn on_market_resolution(
        &self,
        _: MarketResolution,
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

// ── mock server: routed responses + one-shot mutation acks ──────────────

struct Recorded {
    method: String,
    path: String,
    body: String,
}

/// Per-endpoint fixtures. `orders_body` answers every GET of the portfolio
/// order list (startup sweep + every reconciliation sweep); `fills_body`
/// likewise. `place_acks`/`cancel_acks` are consumed one per mutation; an
/// empty queue answers the mutation with a schema-broken body that the
/// executor must classify as a rejection (never a silent success).
struct Routes {
    balance_body: String,
    orders_body: String,
    fills_body: String,
    place_acks: StdMutex<Vec<String>>,
    cancel_acks: StdMutex<Vec<String>>,
}

fn mock_server(routes: Routes) -> (String, Arc<StdMutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local addr");
    let recorded: Arc<StdMutex<Vec<Recorded>>> = Arc::new(StdMutex::new(Vec::new()));
    let rec = recorded.clone();
    let routes = Arc::new(routes);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 16384];
            let n = stream.read(&mut buf).unwrap_or(0);
            let raw = String::from_utf8_lossy(&buf[..n]).to_string();
            let method = raw.split(' ').next().unwrap_or("").to_string();
            let path = raw
                .split(' ')
                .nth(1)
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("")
                .to_string();
            let body = raw
                .split_once("\r\n\r\n")
                .map(|(_, b)| b.to_string())
                .unwrap_or_default();
            rec.lock().unwrap().push(Recorded {
                method: method.clone(),
                path: path.clone(),
                body,
            });

            let response = match (method.as_str(), path.as_str()) {
                ("GET", "/trade-api/v2/portfolio/get_balance") => http_ok(&routes.balance_body),
                ("GET", "/trade-api/v2/portfolio/orders") => http_ok(&routes.orders_body),
                ("GET", "/trade-api/v2/portfolio/fills") => http_ok(&routes.fills_body),
                ("POST", "/trade-api/v2/portfolio/orders") => {
                    let ack = routes.place_acks.lock().unwrap().pop();
                    http_ok(ack.as_deref().unwrap_or(r#"{"error":"no ack queued"}"#))
                }
                (m, p) if m == "DELETE" && p.starts_with("/trade-api/v2/portfolio/orders/") => {
                    let ack = routes.cancel_acks.lock().unwrap().pop();
                    http_ok(ack.as_deref().unwrap_or(r#"{"error":"no ack queued"}"#))
                }
                _ => http_ok(r#"{"error":"unexpected path"}"#),
            };
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });
    (format!("http://{}", addr), recorded)
}

fn http_ok(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

fn routes_for(
    balance: &str,
    orders: &str,
    fills: &str,
    place_acks: Vec<String>,
    cancel_acks: Vec<String>,
) -> Routes {
    Routes {
        balance_body: balance.to_string(),
        orders_body: orders.to_string(),
        fills_body: fills.to_string(),
        place_acks: StdMutex::new(place_acks),
        cancel_acks: StdMutex::new(cancel_acks),
    }
}

/// A fresh in-memory RSA key pair (generated per test, never a fixed literal,
/// dropped with the test — the offline_contract signer test's idiom).
fn test_credentials() -> Credentials {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    let mut rng = rsa::rand_core::OsRng;
    let key = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("test key");
    let pem = key
        .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
        .expect("pem encodes")
        .to_string();
    Credentials {
        key_id: String::from("test-key-id"),
        key_pem: pem,
    }
}

fn client_for(base_url: &str, creds: Credentials) -> Arc<KalshiRest> {
    let config = RestConfig {
        base_url: base_url.to_string(),
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 0,
        ..RestConfig::default()
    };
    Arc::new(KalshiRest::new(config, Some(creds)).expect("client builds with creds"))
}

fn pending_order(core_id: &str, token_id: &str, price: &str, size: &str) -> PendingOrder {
    PendingOrder {
        core_order_id: core_id.to_string(),
        token_id: token_id.to_string(),
        condition_id: String::new(),
        side: Side::Buy,
        fill_policy: FillPolicy::Taker,
        price: Decimal::from_str(price).unwrap(),
        size: Decimal::from_str(size).unwrap(),
        internal_key: String::new(),
        strategy: String::new(),
        asset: String::from("BTC"),
        direction: String::from("long"),
        round_slot: 0,
    }
}

async fn wait_until(deadline_ms: u64, mut pred: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_millis(deadline_ms);
    while !pred() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ── 1. no credentials → INERT ───────────────────────────────────────────

#[tokio::test]
async fn without_credentials_the_production_spawn_is_inert() {
    // This test process carries no KALSHI_* env vars (nothing in this file
    // sets them) — the production spawn must answer Ok(None) and never fetch
    // a balance. The listener exists so a wrongly-spawning executor's balance
    // fetch would at least connect somewhere instead of erroring fast.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            drop(s);
        }
    });
    let sink = ExecSink::default();
    let outcome = executor::spawn_if_configured(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        blitzkrieg_market_api::ExecutorConfig::default(),
    )
    .await
    .expect("inert is not an error");
    assert!(
        outcome.is_none(),
        "no credentials must mean no executor task"
    );
    assert!(
        sink.balances.lock().await.is_empty(),
        "no balance fetch may have happened"
    );
}

// ── 2. place path: pending order → POST → on_order_accepted ─────────────

#[tokio::test]
async fn a_pending_order_is_posted_with_the_ticker_side_and_cent_mapping() {
    let (url, recorded) = mock_server(routes_for(
        r#"{"balance":"512.25","portfolio_value":"10"}"#,
        r#"{"orders":[]}"#,
        r#"{"fills":[]}"#,
        vec![r#"{"order":{"order_id":"kalshi-o-77","ticker":"KXBTC-26-OCT14","side":"yes","action":"buy","order_type":"limit","count":10,"yes_price":"44","status":"resting"}}"#.to_string()],
        vec![],
    ));
    let sink = ExecSink::default();
    sink.push_pending(pending_order("core-9", "KXBTC-26-OCT14", "0.435", "10"))
        .await;
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url, test_credentials()),
    );

    wait_until(10_000, || {
        sink.accepted
            .try_lock()
            .map(|a| a.len() == 1)
            .unwrap_or(false)
    })
    .await;
    assert_eq!(
        sink.accepted
            .lock()
            .await
            .first()
            .map(|(c, v)| (c.as_str(), v.as_str())),
        Some(("core-9", "kalshi-o-77")),
        "the venue id must come back with the core's id preserved"
    );

    // The POST must have gone out with the mapping: 0.435 → 44 cents
    // (round-half-up to the venue grid), token → ticker + side=yes,
    // count = 10, and the core's id as the client order id (idempotency key).
    // Cloned out of the std-mutex guard so no guard crosses an await point.
    let place_body = {
        let posts = recorded.lock().unwrap();
        posts
            .iter()
            .find(|r| r.method == "POST" && r.path == "/trade-api/v2/portfolio/orders")
            .expect("a place POST must have been sent")
            .body
            .clone()
    };
    for needle in [
        r#""ticker":"KXBTC-26-OCT14""#,
        r#""side":"yes""#,
        r#""action":"buy""#,
        r#""yes_price":44"#,
        r#""count":10"#,
        r#""client_order_id":"core-9""#,
    ] {
        assert!(
            place_body.contains(needle),
            "place body missing {needle}; body was: {place_body}"
        );
    }
    assert!(
        !place_body.contains(r#""no_price":"#),
        "a YES order must not carry a NO price"
    );
    // And the balance seed reached the host before anything was placed.
    assert!(
        sink.balances
            .lock()
            .await
            .contains(&Decimal::from_str("512.25").unwrap()),
        "the startup balance must be seeded through seed_balance"
    );
}

// ── 3. reconcile sweep: fills mapped, orphan cancelled, balance synced ──

#[tokio::test]
async fn the_sweep_maps_fills_cancels_orphans_and_syncs_balance() {
    let known_order = r#"{"order_id":"kalshi-known","ticker":"KXBTC-26-OCT14","side":"yes","action":"buy","order_type":"limit","count":5,"yes_price":"43","status":"resting"}"#;
    let orphan_order = r#"{"order_id":"kalshi-orphan","ticker":"KXBTC-26-OCT14","side":"yes","action":"buy","order_type":"limit","count":5,"yes_price":"44","status":"resting"}"#;
    let (url, recorded) = mock_server(routes_for(
        r#"{"balance":"87.15","portfolio_value":"1"}"#,
        &format!(r#"{{"orders":[{known_order},{orphan_order}]}}"#),
        r#"{"fills":[{"trade_id":"t1","order_id":"kalshi-known","ticker":"KXBTC-26-OCT14","side":"yes","action":"buy","count":"5","yes_price":"43","created_time":"2026-10-14T10:00:00Z"},{"trade_id":"t2","order_id":"kalshi-known","ticker":"KXBTC-26-OCT14","side":"no","action":"buy","count":"3","no_price":"57"}]}"#,
        vec![],
        vec![r#"{"order":{"order_id":"kalshi-orphan","ticker":"KXBTC-26-OCT14","side":"yes","action":"buy","order_type":"limit","count":5,"status":"cancelled"}}"#.to_string()],
    ));
    let sink = ExecSink::default();
    sink.known_ids
        .lock()
        .await
        .push(String::from("kalshi-known"));
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url, test_credentials()),
    );

    wait_until(15_000, || {
        sink.orphan_cancels
            .try_lock()
            .map(|o| !o.is_empty())
            .unwrap_or(false)
    })
    .await;
    assert_eq!(
        sink.orphan_cancels.lock().await.first().map(String::as_str),
        Some("kalshi-orphan"),
        "the venue-open id the core does not know must be orphan-cancelled"
    );
    // The known order must NOT have been cancelled (checked on a clone so no
    // std-mutex guard crosses an await point).
    let delete_count = {
        let recorded = recorded.lock().unwrap();
        recorded.iter().filter(|r| r.method == "DELETE").count()
    };
    assert!(
        delete_count == 1,
        "only the orphan may be DELETEd; a known order must never be orphan-cancelled \
         (saw {delete_count} deletes)"
    );

    wait_until(5_000, || {
        sink.fills.try_lock().map(|f| f.len() == 2).unwrap_or(false)
    })
    .await;
    let fills = sink.fills.lock().await;
    let yes = fills
        .iter()
        .find(|f| f.trade_id.as_deref() == Some("t1"))
        .expect("t1 booked");
    assert_eq!(
        yes.token_id, "KXBTC-26-OCT14",
        "YES fill maps to the bare ticker"
    );
    assert_eq!(yes.price, Decimal::from_str("0.43").unwrap());
    assert_eq!(yes.order_id, "kalshi-known");
    let no = fills
        .iter()
        .find(|f| f.trade_id.as_deref() == Some("t2"))
        .expect("t2 booked");
    assert_eq!(
        no.token_id, "KXBTC-26-OCT14:no",
        "NO fill maps to the discovery suffix"
    );
    assert_eq!(no.price, Decimal::from_str("0.57").unwrap());
    drop(fills);

    wait_until(5_000, || {
        sink.balances
            .try_lock()
            .map(|b| b.contains(&Decimal::from_str("87.15").unwrap()))
            .unwrap_or(false)
    })
    .await;
    assert!(
        sink.reconciles
            .lock()
            .await
            .iter()
            .any(|s| s.open_order_ids.contains(&String::from("kalshi-known"))),
        "the reconcile snapshot must list the venue's open orders"
    );
}

// ── 4. cancel-first serialization (#462) ────────────────────────────────

#[tokio::test]
async fn a_pending_cancel_is_sent_before_the_same_tick_s_new_order() {
    let (url, recorded) = mock_server(routes_for(
        r#"{"balance":"100","portfolio_value":"1"}"#,
        r#"{"orders":[]}"#,
        r#"{"fills":[]}"#,
        vec![r#"{"order":{"order_id":"kalshi-new","ticker":"KXBTC-26-OCT14","side":"yes","action":"buy","order_type":"limit","count":5,"yes_price":"44","status":"resting"}}"#.to_string()],
        vec![r#"{"order":{"order_id":"kalshi-old","ticker":"KXBTC-26-OCT14","side":"yes","action":"buy","order_type":"limit","count":5,"status":"cancelled"}}"#.to_string()],
    ));
    let sink = ExecSink::default();
    sink.push_cancel("kalshi-old").await;
    sink.push_pending(pending_order("core-11", "KXBTC-26-OCT14", "0.44", "5"))
        .await;
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url, test_credentials()),
    );

    wait_until(10_000, || {
        sink.accepted
            .try_lock()
            .map(|a| a.len() == 1)
            .unwrap_or(false)
    })
    .await;
    // Orderings checked on a clone so no std-mutex guard crosses an await.
    let (delete_idx, place_idx) = {
        let ops = recorded.lock().unwrap();
        (
            ops.iter()
                .position(|r| r.method == "DELETE" && r.path.contains("kalshi-old"))
                .expect("the pending cancel must be DELETEd"),
            ops.iter()
                .position(|r| r.method == "POST" && r.path == "/trade-api/v2/portfolio/orders")
                .expect("the pending order must be POSTed"),
        )
    };
    assert!(
        delete_idx < place_idx,
        "the cancel must be serialized BEFORE the same tick's placement (#462)"
    );
    assert_eq!(
        sink.accepted
            .lock()
            .await
            .first()
            .map(|(c, v)| (c.as_str(), v.as_str())),
        Some(("core-11", "kalshi-new"))
    );
}

// ── 5. unreadable fill rows are skipped, never forwarded as garbage ─────

#[tokio::test]
async fn an_unreadable_fill_row_is_skipped_not_forwarded() {
    // The t3 row has no price on either side — it must not become a fill.
    let (url, _recorded) = mock_server(routes_for(
        r#"{"balance":"100","portfolio_value":"1"}"#,
        r#"{"orders":[]}"#,
        r#"{"fills":[{"trade_id":"t3","order_id":"x","ticker":"KXBTC-26-OCT14","side":"yes","action":"buy","count":"5"}]}"#,
        vec![],
        vec![],
    ));
    let sink = ExecSink::default();
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url, test_credentials()),
    );
    // Give the loop several sweeps' worth of time, then assert nothing leaked.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        sink.fills.lock().await.is_empty(),
        "a fill row without a readable price must never be forwarded"
    );
    assert!(
        sink.errors.lock().await.is_empty(),
        "a skipped row is a log line, not a reported kernel error"
    );
}
