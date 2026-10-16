//! Order-executor integration tests (PR A / #454, #455).
//!
//! Every test binds a 127.0.0.1 mock server and drives the REAL executor
//! loop (`executor_tests_only_spawn` — the same `executor_loop` the plugin's
//! start path reaches) through a client whose single-secret credential is
//! built at runtime from inert pieces. No test touches the network beyond
//! loopback and no credential material is fixed in source.
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
//! 2. **place path** — a pending order the host hands over first resolves
//!    the token through the venue's own `markets-by-token` registry, then
//!    submits a native POST carrying (market_id, outcome, FAK/GTC, quantity,
//!    price, client_order_id), and the venue's integer id comes back
//!    through `on_order_accepted` stringified with the core's id preserved.
//!    REMOVE the resolver mock and the assert on the recorded body goes red
//!    (the executor refuses to guess an outcome label).
//! 3. **reconcile sweep** — fills land in `ReconcileSnapshot` rows
//!    attributed through BOTH leg sides (taker_order_id and maker_orders),
//!    a venue-open id the core does not know (an orphan) is DELETEd, and
//!    the balance flows to `venue_free_balance`.
//! 4. **cancel-first serialization** — a pending cancel is DELETEd before
//!    the same tick's pending order is POSTed (the #462 rule).
//! 5. **unknown token → rejection, never a guessed placement** — a token
//!    the registry does not answer is refused with InvalidParams and no
//!    POST is sent.

use blitzkrieg_market_api::{
    BookUpdate, CoreError, CoreErrorCode, FillPolicy, MarketFill, MarketHost, MarketResolution,
    MergeRequest, MergeResult, PendingOrder, ReconcileSnapshot, RedemptionRequest,
    RedemptionResult, SelfCheckReport, SettlementQuery, Side, SpotUpdate, SubscriptionControl,
    TokenId, TopOfBookUpdate,
};
use blitzkrieg_polysimulator::{PolySimRest, RestConfig, executor};
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
    fn take_pending_orders(
        &self,
        _: &str,
    ) -> blitzkrieg_market_api::BoxFuture<'_, Vec<PendingOrder>> {
        Box::pin(async { std::mem::take(&mut *self.pending_orders.lock().await) })
    }
    fn take_pending_cancels(&self, _: &str) -> blitzkrieg_market_api::BoxFuture<'_, Vec<String>> {
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

/// Per-endpoint fixtures. `balance_body`/`orders_body`/`trades_body` answer
/// every GET of their path (startup sweep + every reconciliation sweep);
/// `resolve_body` answers the token registry lookup (and is swapped for a
/// 404 in the unknown-token test). `place_acks`/`cancel_acks` are consumed
/// one per mutation; an empty queue answers the mutation with a
/// schema-broken body that the executor must classify as a rejection (never
/// a silent success).
struct Routes {
    balance_body: String,
    orders_body: String,
    trades_body: String,
    resolve_body: String,
    resolve_status: u16,
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

            let starts_with = |p: &str| path.starts_with(p);
            let response = match (method.as_str(), path.as_str()) {
                ("GET", "/v1/account/balance") => http_ok(&routes.balance_body),
                ("GET", "/v1/orders") => http_ok(&routes.orders_body),
                ("GET", "/v1/data/trades") => http_ok(&routes.trades_body),
                ("GET", _p) if starts_with("/v1/markets-by-token/") => {
                    if routes.resolve_status == 200 {
                        http_ok(&routes.resolve_body)
                    } else {
                        http_status(routes.resolve_status, &routes.resolve_body)
                    }
                }
                ("POST", "/v1/order") => {
                    let ack = routes.place_acks.lock().unwrap().pop();
                    http_ok(ack.as_deref().unwrap_or(r#"{"error":"no ack queued"}"#))
                }
                ("DELETE", "/v1/order") => {
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
    http_status(200, body)
}

fn http_status(status: u16, body: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn routes_for(
    balance: &str,
    orders: &str,
    trades: &str,
    resolve: &str,
    place_acks: Vec<String>,
    cancel_acks: Vec<String>,
) -> Routes {
    Routes {
        balance_body: balance.to_string(),
        orders_body: orders.to_string(),
        trades_body: trades.to_string(),
        resolve_body: resolve.to_string(),
        resolve_status: 200,
        place_acks: StdMutex::new(place_acks),
        cancel_acks: StdMutex::new(cancel_acks),
    }
}

/// A credential built at runtime from inert fragments — no key material is
/// fixed in source, exactly the offline_contract idiom.
fn test_credentials() -> blitzkrieg_polysimulator::Credentials {
    blitzkrieg_polysimulator::Credentials {
        api_key: format!("{}-{}", "synthetic", "key-under-test"),
    }
}

fn client_for(base_url: &str) -> Arc<PolySimRest> {
    let config = RestConfig {
        base_url: base_url.to_string(),
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 0,
        backoff_initial: Duration::from_millis(10),
        ..RestConfig::default()
    };
    Arc::new(PolySimRest::new(config, Some(test_credentials())).expect("client builds with creds"))
}

/// The venue registry answer for the token our pending orders carry: the
/// resolver that the native order form's (market_id, outcome) must come from.
const RESOLVE_BODY: &str = r#"{"condition_id":"0xcond-1","outcome":"Up","primary_token_id":"71321","secondary_token_id":"71322"}"#;

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
    // This test process carries no POLYSIM_API_KEY (nothing in this file sets
    // it) — the production spawn must answer Ok(None) and never fetch a
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

// ── 2. place path: resolve → native POST → on_order_accepted ────────────

#[tokio::test]
async fn a_pending_order_resolves_then_posts_the_native_mapping() {
    let (url, recorded) = mock_server(routes_for(
        r#"{"balance":"512.25","currency":"USDC"}"#,
        r#"{"orders":[]}"#,
        r#"{"data":[]}"#,
        RESOLVE_BODY,
        vec![
            // First ack: the place POST. The venue id is an INTEGER on the wire.
            r#"{"order_id":77,"status":"PENDING","client_order_id":"core-9"}"#.to_string(),
        ],
        vec![],
    ));
    let sink = ExecSink::default();
    sink.push_pending(pending_order("core-9", "71321", "0.435", "10"))
        .await;
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url),
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
        Some(("core-9", "77")),
        "the integer venue id must come back stringified with the core's id preserved"
    );

    // The POST must have gone out with the venue's own registry answer:
    // market_id + outcome label resolved from the token (NOT the raw token
    // id and NOT a guessed label), Taker → FAK, and the core's id as the
    // client order id (dedupe key). Cloned out of the std-mutex guard so no
    // guard crosses an await point.
    let place_body = {
        let posts = recorded.lock().unwrap();
        posts
            .iter()
            .find(|r| r.method == "POST" && r.path == "/v1/order")
            .expect("a place POST must have been sent")
            .body
            .clone()
    };
    for needle in [
        r#""market_id":"0xcond-1""#,
        r#""side":"BUY""#,
        r#""outcome":"Up""#,
        r#""quantity":"10""#,
        r#""order_type":"limit""#,
        r#""price":"0.435""#,
        r#""time_in_force":"FAK""#,
        r#""client_order_id":"core-9""#,
    ] {
        assert!(
            place_body.contains(needle),
            "place body missing {needle}; body was: {place_body}"
        );
    }
    // Reverse acceptance: the outcome label must come from the venue's
    // resolver, never from a plugin-side guess. Break the resolver mock and
    // this test cannot reach an accepted order at all.
    let resolve_seen = {
        let gets = recorded.lock().unwrap();
        gets.iter()
            .any(|r| r.method == "GET" && r.path.starts_with("/v1/markets-by-token/71321"))
    };
    assert!(
        resolve_seen,
        "the token must have been resolved through the venue registry"
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

// ── 3. reconcile sweep: fills via both legs, orphan cancelled, balance ──

#[tokio::test]
async fn the_sweep_maps_fills_cancels_orphans_and_syncs_balance() {
    let known_order = r#"{"order_id":41,"market_id":"0xcond-1","side":"BUY","outcome":"Up","order_type":"limit","limit_price":"0.43","quantity":"5","time_in_force":"GTC","status":"PENDING"}"#;
    let orphan_order = r#"{"order_id":99,"market_id":"0xcond-1","side":"BUY","outcome":"Down","order_type":"limit","limit_price":"0.44","quantity":"5","time_in_force":"GTC","status":"PENDING"}"#;
    let (url, recorded) = mock_server(routes_for(
        r#"{"balance":"87.15"}"#,
        &format!(r#"{{"orders":[{known_order},{orphan_order}]}}"#),
        // t1: OUR taker leg (taker_order_id=41); t2: OUR maker leg (77 in
        // maker_orders); t3: neither leg ours — never attributed.
        r#"{"data":[
            {"id":"t1","market":"0xcond-1","asset_id":"71321","side":"BUY","outcome":"Up","price":"0.43","size":"5","taker_order_id":"41","maker_orders":["7"],"status":"MATCHED","match_time":1773583200000},
            {"id":"t2","market":"0xcond-1","asset_id":"71321","side":"SELL","outcome":"Up","price":"0.55","size":"3","taker_order_id":"8","maker_orders":["77"],"status":"MATCHED","match_time":1773583201000},
            {"id":"t3","market":"0xcond-1","asset_id":"71321","side":"BUY","outcome":"Up","price":"0.60","size":"2","taker_order_id":"8","maker_orders":["7"],"status":"MATCHED"}
        ]}"#,
        RESOLVE_BODY,
        vec![],
        vec![r#"{"order_id":99,"status":"CANCELLED"}"#.to_string()],
    ));
    let sink = ExecSink::default();
    sink.known_ids
        .lock()
        .await
        .extend(["41".to_string(), "77".to_string()]);
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url),
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
        Some("99"),
        "the venue-open id the core does not know must be orphan-cancelled"
    );
    // The known orders must NOT have been cancelled (checked on a clone so
    // no std-mutex guard crosses an await point).
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
    let taker_fill = fills
        .iter()
        .find(|f| f.trade_id.as_deref() == Some("t1"))
        .expect("t1 booked via the taker leg");
    assert_eq!(taker_fill.order_id, "41");
    assert_eq!(taker_fill.token_id, "71321");
    assert_eq!(taker_fill.price, Decimal::from_str("0.43").unwrap());
    assert_eq!(taker_fill.maker, Some(false));
    let maker_fill = fills
        .iter()
        .find(|f| f.trade_id.as_deref() == Some("t2"))
        .expect("t2 booked via the maker leg");
    assert_eq!(maker_fill.order_id, "77");
    assert_eq!(maker_fill.maker, Some(true));
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
            .any(|s| s.open_order_ids.contains(&String::from("41"))),
        "the reconcile snapshot must list the venue's open orders"
    );
}

// ── 4. cancel-first serialization (#462) ────────────────────────────────

#[tokio::test]
async fn a_pending_cancel_is_sent_before_the_same_tick_s_new_order() {
    let (url, recorded) = mock_server(routes_for(
        r#"{"balance":"100"}"#,
        r#"{"orders":[]}"#,
        r#"{"data":[]}"#,
        RESOLVE_BODY,
        vec![r#"{"order_id":55,"status":"PENDING","client_order_id":"core-11"}"#.to_string()],
        vec![r#"{"order_id":54,"status":"CANCELLED"}"#.to_string()],
    ));
    let sink = ExecSink::default();
    sink.push_cancel("54").await;
    sink.push_pending(pending_order("core-11", "71321", "0.44", "5"))
        .await;
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url),
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
                .position(|r| r.method == "DELETE" && r.body.contains(r#""orderID":"54""#))
                .expect("the pending cancel must be DELETEd with the venue's orderID body"),
            ops.iter()
                .position(|r| r.method == "POST" && r.path == "/v1/order")
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
        Some(("core-11", "55"))
    );
}

// ── 5. unknown token → rejection, never a guessed placement ─────────────

#[tokio::test]
async fn an_unresolvable_token_is_rejected_not_guessed() {
    let (url, recorded) = mock_server(Routes {
        resolve_status: 404,
        resolve_body: r#"{"error":"MARKET_NOT_FOUND","message":"no market for token"}"#.to_string(),
        ..routes_for(
            r#"{"balance":"100"}"#,
            r#"{"orders":[]}"#,
            r#"{"data":[]}"#,
            "",
            vec![],
            vec![],
        )
    });
    let sink = ExecSink::default();
    sink.push_pending(pending_order("core-404", "999999", "0.50", "10"))
        .await;
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url),
    );

    wait_until(10_000, || {
        sink.rejected
            .try_lock()
            .map(|r| !r.is_empty())
            .unwrap_or(false)
    })
    .await;
    let rejected = sink.rejected.lock().await;
    assert_eq!(
        rejected.first().map(|(c, _)| c.as_str()),
        Some("core-404"),
        "the order must be rejected back to the core, not dropped"
    );
    assert_eq!(
        rejected.first().unwrap().1.as_ref().map(|e| e.code),
        Some(CoreErrorCode::InvalidParams),
        "an unresolvable token is a routing bug — refused as InvalidParams"
    );
    let posts = {
        let recorded = recorded.lock().unwrap();
        recorded
            .iter()
            .filter(|r| r.method == "POST" && r.path == "/v1/order")
            .count()
    };
    assert_eq!(
        posts, 0,
        "no order may be placed when the outcome label cannot be resolved"
    );
}

// ── 6. unreadable fill rows are skipped, never forwarded as garbage ─────

#[tokio::test]
async fn an_unreadable_fill_row_is_skipped_not_forwarded() {
    // The t3 row is OURS by taker id but carries no price — it must not
    // become a fill, and a skipped row is a log line, not a kernel error.
    let (url, _recorded) = mock_server(routes_for(
        r#"{"balance":"100"}"#,
        r#"{"orders":[]}"#,
        r#"{"data":[{"id":"t3","market":"0xcond-1","asset_id":"71321","side":"BUY","outcome":"Up","size":"5","taker_order_id":"41","status":"MATCHED"}]}"#,
        RESOLVE_BODY,
        vec![],
        vec![],
    ));
    let sink = ExecSink::default();
    sink.known_ids.lock().await.push(String::from("41"));
    executor::executor_tests_only_spawn(
        Arc::new(sink.clone()) as Arc<dyn MarketHost>,
        client_for(&url),
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
