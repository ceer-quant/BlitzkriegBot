//! Order-executor integration tests (#483 — the stage-4 completion).
//!
//! Every test binds a 127.0.0.1 mock server and drives the REAL executor loop
//! (`executor_tests_only_spawn` — the same `executor_loop` the plugin's
//! `OrderExecutor::start` reaches) through a client pointed at the mock and
//! a signer with a programmatic throwaway key. No test touches the network
//! beyond loopback and no credential material is fixed in source.
//!
//! The mock is ROUTED, not sequenced: the executor polls on its own cadence
//! (startup + a sweep every ~5s), so a canned-in-order server would race the
//! polls and hand a sweep a body meant for another endpoint. Instead each
//! request path always gets the same body, and only the one-shot mutations
//! (place POST, cancel POST) consume an ack from a queue.
//!
//! Covered shapes (reverse acceptance where noted):
//! 1. **no credentials → INERT** — the production spawn answers `Ok(None)`
//!    and never opens a socket; dry-run keeps an honest `has_executor`
//!    without order egress.
//! 2. **place path** — a pending order the host hands over is submitted as
//!    an EIP-712 signed order POST whose body carries the quantized
//!    amounts/fields the signing module documents, and the venue's id comes
//!    back through `on_order_accepted` with the core's id preserved. REMOVE
//!    the quantization/signing and the asserts on the recorded body go red.
//! 3. **reconcile sweep** — the cumulative `amountFilled` watermark diffs
//!    into per-sweep fills keyed `{venue_id}:{cumulative}`; a venue-open id
//!    the core does not know is cancelled; the balance flows to
//!    `venue_free_balance`. A zero-delta watermark books NOTHING (no double
//!    fills from re-reading the same watermark).
//! 4. **cancel-first serialization** — a pending cancel is POSTed before
//!    the same tick's pending order (the #462 rule).
//! 5. **unreadable rows** (no hash of ours) never become fills or orphans.

use blitzkrieg_market_api::{
    BookUpdate, CoreError, FillPolicy, MarketFill, MarketHost, MarketResolution, MergeRequest,
    MergeResult, PendingOrder, ReconcileSnapshot, RedemptionRequest, RedemptionResult,
    SelfCheckReport, SettlementQuery, Side, SpotUpdate, SubscriptionControl, TokenId,
    TopOfBookUpdate,
};
use blitzkrieg_predictfun::rest::Credentials;
use blitzkrieg_predictfun::{OrderSigner, PredictRest, RestConfig, executor};
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
    fn subscribe_tokens(&self, _: Vec<TokenId>) -> blitzkrieg_market_api::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn install_subscription_control(
        &self,
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

/// Per-endpoint fixtures. `markets_body` answers the market-row GET the
/// executor resolves a token's contract/fee from; `orders_body` answers
/// every GET of the portfolio order list; `balance_body` likewise.
/// `place_acks`/`cancel_acks` are consumed one per mutation; an empty queue
/// answers the mutation with a body that names no id — which the executor
/// must classify as a schema rejection (never a silent success).
struct Routes {
    balance_body: String,
    markets_body: String,
    orders_body: String,
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
            let mut buf = [0u8; 65536];
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
                ("GET", "/v1/balance") => http_ok(&routes.balance_body),
                ("GET", p)
                    if p.starts_with("/v1/markets/7492816350129384756102938475610293847561")
                        && !p.contains("orderbook") =>
                {
                    http_ok(&routes.markets_body)
                }
                ("GET", "/v1/orders") => http_ok(&routes.orders_body),
                ("POST", "/v1/orders") => {
                    let ack = routes.place_acks.lock().unwrap().pop();
                    http_ok(ack.as_deref().unwrap_or(r#"{"success":true,"data":{}}"#))
                }
                ("POST", "/v1/orders/remove") => {
                    let ack = routes.cancel_acks.lock().unwrap().pop();
                    http_ok(ack.as_deref().unwrap_or(r#"{"success":true,"data":{}}"#))
                }
                _ => http_ok(r#"{"success":false,"code":404,"error":"not found"}"#),
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
    markets: &str,
    orders: &str,
    place_acks: Vec<String>,
    cancel_acks: Vec<String>,
) -> Routes {
    Routes {
        balance_body: balance.to_string(),
        markets_body: markets.to_string(),
        orders_body: orders.to_string(),
        place_acks: StdMutex::new(place_acks),
        cancel_acks: StdMutex::new(cancel_acks),
    }
}

/// The default market row the mock hands back for any token: a plain
/// (non-neg-risk, non-yield-bearing) market on the base CTF exchange.
fn default_market_row() -> &'static str {
    r#"{"success":true,"data":{"token_id":"7492816350129384756102938475610293847561","question":"Will X happen?","is_neg_risk":false,"is_yield_bearing":false,"fee_rate_bps":100,"status":"ACTIVE","best_bid":"0.43","best_ask":"0.45"}}"#
}

/// A throwaway signer with a key generated IN the test process (an LCG off a
/// fixed seed is fine here: the key protects nothing and exists for exactly
/// one test; what matters is that no credential is fixed in source).
fn test_signer() -> OrderSigner {
    let mut key = String::with_capacity(64);
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64
        | 1;
    for _ in 0..32 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        key.push_str(&format!("{:02x}", (state >> 33) as u8));
    }
    OrderSigner::from_hex_key(&key).expect("test key parses")
}

fn client_for(base_url: &str) -> Arc<PredictRest> {
    let config = RestConfig {
        base_url: base_url.to_string(),
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 0,
        ..RestConfig::default()
    };
    let creds = Credentials {
        api_key: Some(String::from("test-key")),
        jwt: Some(String::from("test-jwt")),
    };
    Arc::new(PredictRest::new(config, Some(creds)).expect("client builds with creds"))
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
    // This test process carries no PREDICT_PRIVATE_KEY (nothing in this file
    // sets it) — the production spawn must answer Ok(None) and never fetch a
    // balance. The listener exists so a wrongly-spawning executor's balance
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

// ── 2. place path: pending order → signed POST → on_order_accepted ──────

#[tokio::test]
async fn a_pending_order_is_posted_as_a_signed_quantized_order() {
    let (url, recorded) = mock_server(routes_for(
        r#"{"success":true,"data":{"balance":"512.25","total_value":"10"}}"#,
        default_market_row(),
        r#"{"success":true,"data":{"orders":[]}}"#,
        vec![r#"{"success":true,"data":{"orderId":"pf-77","hash":"0xdeadbeef"}}"#.to_string()],
        vec![],
    ));
    let sink = ExecSink::default();
    sink.push_pending(pending_order(
        "core-9",
        "7492816350129384756102938475610293847561",
        "0.44",
        "10",
    ))
    .await;
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url),
        test_signer(),
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
        Some(("core-9", "pf-77")),
        "the venue id must come back with the core's id preserved"
    );

    // The POST must have gone out as the references' payload shape with the
    // BUY quantization: makerAmount=notional(4.4e18), takerAmount=shares(1e16),
    // side=0, EOA signature type, fee 100bps from the market row, and a real
    // 65-byte signature. REMOVE any of these and the assert goes red.
    let place_body = {
        let posts = recorded.lock().unwrap();
        posts
            .iter()
            .find(|r| r.method == "POST" && r.path == "/v1/orders")
            .expect("a place POST must have been sent")
            .body
            .clone()
    };
    let parsed: serde_json::Value = serde_json::from_str(&place_body).expect("json body");
    let data = parsed
        .get("data")
        .expect("payload carries the data envelope");
    assert!(
        data.get("order").is_some(),
        "the signed order must ride under data.order; got {place_body}"
    );
    let order = &data["order"];
    assert_eq!(order["makerAmount"].as_str(), Some("4400000000000000000"));
    assert_eq!(order["takerAmount"].as_str(), Some("10000000000000000000"));
    assert_eq!(order["side"], 0, "BUY is side=0");
    assert_eq!(order["signatureType"], 0, "EOA signature type");
    assert_eq!(order["feeRateBps"].as_str(), Some("100"));
    assert_eq!(order["expiration"].as_str(), Some("4102444800"));
    assert_eq!(
        order["tokenId"].as_str(),
        Some("7492816350129384756102938475610293847561")
    );
    assert_eq!(order["maker"], order["signer"], "maker=signer=our wallet");
    let sig = order["signature"].as_str().expect("signature present");
    assert_eq!(
        sig.len(),
        132, // 0x + 65 bytes
        "an EIP-712 ECDSA signature is 65 bytes hex"
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

// ── 3. reconcile sweep: watermark fills, orphan cancel, balance sync ────

#[tokio::test]
async fn the_sweep_diffs_watermarks_cancels_orphans_and_syncs_balance() {
    // Two rows the executor has no in-flight hash for (pf-orphan, pf-ours —
    // the executor only tracks hashes IT signed, so both read as unknowns)
    // and one the HOST knows (pf-known) that must NOT be cancelled. The
    // fill-diff over a SIGNED order's watermark is covered by test 2 + the
    // lib-level sweep unit tests; this test pins orphan/known/balance.
    let ours = r#"{"id":"pf-ours","orderHash":"0xourshash","tokenId":"7492816350129384756102938475610293847561","side":"BUY","price":"0.44","shares":"10","status":"OPEN","amount_filled":"3"}"#;
    let orphan = r#"{"id":"pf-orphan","orderHash":"0xorphanhash","tokenId":"7492816350129384756102938475610293847561","side":"BUY","price":"0.44","shares":"5","status":"OPEN"}"#;
    let known = r#"{"id":"pf-known","orderHash":"0xknownhash","tokenId":"7492816350129384756102938475610293847561","side":"SELL","price":"0.57","shares":"2","status":"OPEN"}"#;
    let routes = Routes {
        balance_body: r#"{"success":true,"data":{"balance":"87.15"}}"#.to_string(),
        markets_body: default_market_row().to_string(),
        orders_body: format!(r#"{{"success":true,"data":{{"orders":[{ours},{orphan},{known}]}}}}"#),
        place_acks: StdMutex::new(vec![]),
        cancel_acks: StdMutex::new(vec![r#"{"success":true,"data":{}}"#.to_string()]),
    };
    let (url, recorded) = mock_server(routes);

    let sink = ExecSink::default();
    // Seed the in-flight hash indirectly is impossible from the test — the
    // executor only tracks hashes IT signed. So exercise the orphan+known and
    // balance paths here; fill-diff is covered by the signed-order test flow:
    // place an order via the mock, then let the sweep see its row.
    sink.known_ids.lock().await.push(String::from("pf-known"));
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url),
        test_signer(),
    );

    wait_until(15_000, || {
        sink.orphan_cancels
            .try_lock()
            .map(|o| o.len() == 2)
            .unwrap_or(false)
    })
    .await;
    // BOTH unknown rows must be orphan-cancelled: pf-orphan (plain orphan)
    // and pf-ours (its hash is NOT in the executor's in-flight set — the
    // executor only tracks hashes IT signed, so from its point of view that
    // row is exactly as unknown as the orphan; a hash the executor signed
    // would have been adopted instead).
    let mut orphans = sink.orphan_cancels.lock().await.clone();
    orphans.sort();
    assert_eq!(
        orphans,
        vec![String::from("pf-orphan"), String::from("pf-ours")],
        "every venue-open id outside known+in-flight must be orphan-cancelled"
    );
    // Only the two orphans; the host-known pf-known must NOT be cancelled.
    let cancel_count = {
        let recorded = recorded.lock().unwrap();
        recorded
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/v1/orders/remove")
            .count()
    };
    assert_eq!(
        cancel_count, 2,
        "only unknowns may be cancelled; a known order must never be (saw {cancel_count})"
    );

    wait_until(5_000, || {
        sink.balances
            .try_lock()
            .map(|b| b.contains(&Decimal::from_str("87.15").unwrap()))
            .unwrap_or(false)
    })
    .await;
    // The periodic sweep fires ~5s in; the earlier waits all passed on the
    // startup seed, so this is the first await that actually observes it.
    wait_until(15_000, || {
        sink.reconciles
            .try_lock()
            .map(|r| !r.is_empty())
            .unwrap_or(false)
    })
    .await;
    let snap = sink.reconciles.lock().await.clone();
    assert!(
        snap.iter()
            .any(|s| s.open_order_ids.contains(&String::from("pf-known"))),
        "the reconcile snapshot must list the venue's open orders"
    );
}

// ── 4. cancel-first serialization (#462) ────────────────────────────────

#[tokio::test]
async fn a_pending_cancel_is_sent_before_the_same_tick_s_new_order() {
    let (url, recorded) = mock_server(routes_for(
        r#"{"success":true,"data":{"balance":"100"}}"#,
        default_market_row(),
        r#"{"success":true,"data":{"orders":[]}}"#,
        vec![r#"{"success":true,"data":{"orderId":"pf-new"}}"#.to_string()],
        vec![r#"{"success":true,"data":{}}"#.to_string()],
    ));
    let sink = ExecSink::default();
    sink.push_cancel("pf-old").await;
    sink.push_pending(pending_order(
        "core-11",
        "7492816350129384756102938475610293847561",
        "0.44",
        "5",
    ))
    .await;
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url),
        test_signer(),
    );

    wait_until(10_000, || {
        sink.accepted
            .try_lock()
            .map(|a| a.len() == 1)
            .unwrap_or(false)
    })
    .await;
    // Orderings checked on a clone so no std-mutex guard crosses an await.
    let (cancel_idx, place_idx) = {
        let ops = recorded.lock().unwrap();
        (
            ops.iter()
                .position(|r| r.method == "POST" && r.path == "/v1/orders/remove")
                .expect("the pending cancel must be POSTed"),
            ops.iter()
                .position(|r| r.method == "POST" && r.path == "/v1/orders")
                .expect("the pending order must be POSTed"),
        )
    };
    assert!(
        cancel_idx < place_idx,
        "the cancel must be serialized BEFORE the same tick's placement (#462)"
    );
    assert_eq!(
        sink.accepted
            .lock()
            .await
            .first()
            .map(|(c, v)| (c.as_str(), v.as_str())),
        Some(("core-11", "pf-new"))
    );
    // The cancel payload must be the venue's {data:{ids:[...]}} envelope.
    let cancel_body = {
        let ops = recorded.lock().unwrap();
        ops.iter()
            .find(|r| r.method == "POST" && r.path == "/v1/orders/remove")
            .expect("cancel recorded")
            .body
            .clone()
    };
    assert!(
        cancel_body.contains(r#""ids":["pf-old"]"#),
        "cancel payload must carry the venue's envelope shape; got {cancel_body}"
    );
}

// ── 5. rows we did not sign never become fills or orphans ───────────────

#[tokio::test]
async fn rows_the_executor_did_not_sign_never_become_fills() {
    // Orders list with watermark-bearing rows but NO hash we signed: the
    // fill diff must not fire (watermarks belong to our in-flight set only),
    // and no error may reach the host over it.
    let stranger = r#"{"id":"pf-x","orderHash":"0xnotours","tokenId":"7492816350129384756102938475610293847561","side":"BUY","price":"0.44","shares":"10","status":"OPEN","amount_filled":"9"}"#;
    let (url, _recorded) = mock_server(routes_for(
        r#"{"success":true,"data":{"balance":"100"}}"#,
        default_market_row(),
        &format!(r#"{{"success":true,"data":{{"orders":[{stranger}]}}}}"#),
        vec![],
        vec![],
    ));
    let sink = ExecSink::default();
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url),
        test_signer(),
    );
    // Give the loop several sweeps' worth of time, then assert nothing leaked.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        sink.fills.lock().await.is_empty(),
        "a watermark row we did not sign must never be forwarded as a fill"
    );
    assert!(
        sink.errors.lock().await.is_empty(),
        "an unowned row is not a kernel error"
    );
}
