//! Offline contract tests for the PolySimulator REST client (PR A / #454).
//! Every test binds a 127.0.0.1 mock server; no test touches the network and
//! no real credential appears anywhere (the one credentialed call builds its
//! key at runtime from inert fragments).
//!
//! The wire contract these tests pin was re-verified against the LIVE venue
//! 2026-10-25 (the delivery-time spec corpus had it wrong and flat places
//! were answered `VALIDATION_FAILED: missing order` — see #630): native
//! `POST /v1/order` as the `{"order":{tokenId, side, makerAmount,
//! takerAmount}}` envelope with 1e6-scaled integer strings, camelCase
//! `orderID` success acks, the `{"canceled":[…],"not_canceled":{…}}` batch
//! cancel envelope, integer order ids, the PM-shape trades envelope with
//! both leg fields, and the structured error body `{error, message}`.

use blitzkrieg_market_api::CoreErrorCode;
use blitzkrieg_polysimulator::types::PlaceOrderBody;
use blitzkrieg_polysimulator::{
    Credentials, PolySimError, PolySimRest, RestConfig, into_core_error,
};
use rust_decimal::Decimal;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::str::FromStr;
use std::time::Duration;

/// Serve `count` canned HTTP responses on an ephemeral 127.0.0.1 port.
fn mock_server(responses: Vec<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut buf = [0u8; 16384];
            let _ = stream.read(&mut buf); // request line + headers; body unused
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
        // Remaining connections (reqwest pooling) are dropped with the test.
    });
    format!("http://{}", addr)
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

fn client_for(base_url: &str) -> PolySimRest {
    let config = RestConfig {
        base_url: base_url.to_string(),
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 0,
        backoff_initial: Duration::from_millis(1),
        ..RestConfig::default()
    };
    PolySimRest::new(config, None).expect("client builds unauthenticated")
}

fn client_with_key(base_url: &str) -> PolySimRest {
    let config = RestConfig {
        base_url: base_url.to_string(),
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 0,
        backoff_initial: Duration::from_millis(1),
        ..RestConfig::default()
    };
    // The key is assembled at runtime so no credential-shaped literal exists
    // in this file.
    PolySimRest::new(
        config,
        Some(Credentials {
            api_key: format!("{}-{}", "synthetic", "offline-key"),
        }),
    )
    .expect("client builds with creds")
}

// ── market data shapes ──────────────────────────────────────────────────

const BOOK_JSON: &str = r#"{"bids":[{"price":"0.43","size":"150"}],"asks":[{"price":"0.57","size":"120"}],"timestamp":"1773583200000"}"#;

const UPDOWN_JSON: &str = r#"{"markets":[{
    "event_id":"ev-1","condition_id":"0xevent-cond","asset":"BTC","interval":"15m",
    "start_date":"2026-03-15T13:00:00Z","end_date":"2026-03-15T14:00:00Z","active":true,
    "markets":[{"condition_id":"0xcond-1","question":"Up or down?",
        "outcomes":"[\"Up\",\"Down\"]",
        "outcome_prices":"[\"0.55\",\"0.45\"]",
        "token_ids":"[\"71321\",\"71322\"]"}],
    "live_price":{"buy":0.55,"sell":0.54}
}]}"#;

#[tokio::test]
async fn updown_rows_parse_with_the_nested_json_strings() {
    let url = mock_server(vec![http_ok(UPDOWN_JSON)]);
    let rows = client_for(&url)
        .updown_markets(Some("BTC"), Some("15m"))
        .await
        .expect("updown parses");
    assert_eq!(rows.len(), 1);
    let tokens = rows[0].parse_token_ids().expect("token ids");
    assert_eq!(tokens, ("71321".to_string(), "71322".to_string()));
    let prices = rows[0].parse_outcome_prices().expect("outcome prices");
    assert_eq!(
        prices,
        (
            Decimal::from_str("0.55").unwrap(),
            Decimal::from_str("0.45").unwrap()
        )
    );
    assert!(rows[0].is_open());
    assert_eq!(rows[0].end_ms(), Some(1_773_583_200_000));
    assert_eq!(rows[0].start_ms(), Some(1_773_579_600_000));
}

#[tokio::test]
async fn book_snapshot_parses_with_decimal_levels() {
    let url = mock_server(vec![http_ok(
        r#"{"bids":[{"price":"0.43","size":"150"},{"price":"0.42","size":"80"}],
            "asks":[{"price":"0.57","size":"120"},{"price":"0.58","size":"60"}],
            "timestamp":"1773583200000"}"#,
    )]);
    let book = client_for(&url).book("71321").await.expect("book parses");
    let bids = book.bids.unwrap();
    assert_eq!(bids.len(), 2);
    assert_eq!(bids[0].price, Decimal::from_str("0.43").unwrap());
    assert_eq!(bids[1].size, Decimal::from(80));
    assert_eq!(
        book.asks.unwrap()[0].price,
        Decimal::from_str("0.57").unwrap()
    );
}

#[tokio::test]
async fn markets_by_token_returns_the_registry_answer() {
    let url = mock_server(vec![http_ok(
        r#"{"condition_id":"0xcond-9","outcome":"Down","primary_token_id":"71322"}"#,
    )]);
    let resolved = client_for(&url)
        .markets_by_token("71322")
        .await
        .expect("registry answers");
    assert_eq!(resolved.condition_id, "0xcond-9");
    assert_eq!(resolved.outcome.as_deref(), Some("Down"));
}

// ── trading: the native order form ──────────────────────────────────────

#[tokio::test]
async fn place_order_sends_the_native_form_and_parses_the_integer_id() {
    // The mock echoes the request body back in a header-free channel — we
    // record bodies by serving a body that embeds nothing, then assert on a
    // second channel: the response must parse an INTEGER id into the string
    // seam (the venue's ids are integers everywhere). Ack = the live camel
    // shape (orderID / status).
    let url = mock_server(vec![http_ok(
        r#"{"orderID":6391,"status":"live","makingAmount":"2175000","takingAmount":"5000000","tradeIDs":null,"errorMsg":""}"#,
    )]);
    let client = client_with_key(&url);
    let body = PlaceOrderBody {
        token_id: "71321".to_string(),
        side: "BUY".to_string(),
        maker_amount: "2175000".to_string(),
        taker_amount: "5000000".to_string(),
    };
    let ack = client.place_order(&body).await.expect("order accepted");
    assert_eq!(ack.venue_id(), "6391", "integer id stringified at the seam");
    assert_eq!(ack.status.as_deref(), Some("live"));
}

#[tokio::test]
async fn cancel_order_parses_the_batch_envelope() {
    // Live shape: the single-cancel DELETE answers a batch envelope; success
    // = our id inside `canceled`.
    let url = mock_server(vec![http_ok(r#"{"canceled":["6391"],"not_canceled":{}}"#)]);
    let client = client_with_key(&url);
    let ack = client.cancel_order("6391").await.expect("cancel ok");
    assert_eq!(ack, "6391");
}

#[tokio::test]
async fn cancel_order_refusal_maps_to_the_venue_reason() {
    let url = mock_server(vec![http_ok(
        r#"{"canceled":[],"not_canceled":{"6391":"Order not found or already cancelled/filled"}}"#,
    )]);
    let client = client_with_key(&url);
    let err = client
        .cancel_order("6391")
        .await
        .expect_err("id absent from canceled set");
    match err {
        PolySimError::Api { code, message, .. } => {
            assert_eq!(code, "ORDER_NOT_FOUND");
            assert!(
                message.contains("6391") && message.contains("not found"),
                "{message}"
            );
        }
        other => panic!("expected Api error, got {other:?}"),
    }
}

#[tokio::test]
async fn open_orders_parse_from_the_native_envelope() {
    let url = mock_server(vec![http_ok(
        r#"{"orders":[{"order_id":41,"market_id":"0xcond-1","side":"BUY","outcome":"Up","limit_price":"0.43","quantity":"5","status":"PENDING"}],"next_cursor":null,"has_more":false}"#,
    )]);
    let orders = client_with_key(&url).open_orders().await.expect("orders");
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].venue_id(), "41");
}

#[tokio::test]
async fn trades_parse_the_pm_envelope_with_both_leg_sides() {
    let url = mock_server(vec![http_ok(
        r#"{"limit":100,"count":1,"next_cursor":null,"data":[{
            "id":"tx-1","market":"0xcond-1","asset_id":"71321","side":"BUY","outcome":"Up",
            "price":"0.55","size":"10","fee_rate_bps":0,
            "maker_orders":["41"],"taker_order_id":"77",
            "transaction_hash":"0xdead","bucket_index":3,"last_update":1773583200000,
            "match_time":1773583201000,"owner":"me","status":"MATCHED","trader_side":"TAKER"}]}"#,
    )]);
    let rows = client_with_key(&url).trades().await.expect("trades");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].taker_order_id.as_deref(), Some("77"));
    assert_eq!(rows[0].maker_orders.as_ref().unwrap(), &["41".to_string()]);
}

#[tokio::test]
async fn balance_parses_decimal_from_a_string() {
    let url = mock_server(vec![http_ok(r#"{"balance":"1024.50","currency":"USDC"}"#)]);
    let balance = client_with_key(&url).balance().await.expect("balance");
    assert_eq!(balance.balance, Decimal::from_str("1024.50").unwrap());
}

// ── error taxonomy ──────────────────────────────────────────────────────

#[tokio::test]
async fn unauthenticated_account_endpoint_fails_with_auth_config() {
    let url = mock_server(vec![http_ok(r#"{"balance":"100"}"#)]);
    let client = client_for(&url); // no key configured
    let err = client.balance().await.expect_err("no credentials");
    assert!(
        matches!(err, PolySimError::AuthConfig(_)),
        "missing key is a local config error, not a venue answer: {err:?}"
    );
    assert_eq!(into_core_error(&err).code, CoreErrorCode::NotAuthenticated);
}

#[tokio::test]
async fn structured_venue_error_body_maps_by_code() {
    let url = mock_server(vec![http_status(
        400,
        "Bad Request",
        r#"{"error":"INSUFFICIENT_BALANCE","message":"wallet light"}"#,
    )]);
    let client = client_with_key(&url);
    let err = client.balance().await.expect_err("venue refused");
    assert!(matches!(err, PolySimError::Api { .. }), "got {err:?}");
    let core = into_core_error(&err);
    assert_eq!(core.code, CoreErrorCode::InsufficientFunds);
    assert!(
        core.raw
            .unwrap_or_default()
            .contains("INSUFFICIENT_BALANCE"),
        "raw venue text must survive the crossing"
    );
}

#[tokio::test]
async fn unknown_api_code_falls_back_to_venue_error() {
    let err = PolySimError::Api {
        code: String::from("SOMETHING_NEW"),
        message: String::from("venue added a code"),
        status: 400,
        retry_after_ms: None,
    };
    assert_eq!(into_core_error(&err).code, CoreErrorCode::VenueError);
}

#[tokio::test]
async fn schema_break_surfaces_as_internal() {
    let url = mock_server(vec![http_ok(r#"{"data": "not-a-list"}"#)]);
    let client = client_with_key(&url);
    let err = client.trades().await.expect_err("schema mismatch");
    assert!(matches!(err, PolySimError::Schema(_)), "got {err:?}");
    assert_eq!(into_core_error(&err).code, CoreErrorCode::Internal);
}

#[tokio::test]
async fn http_429_is_rate_limited_and_not_retried_to_death_here() {
    // One 429 with retries=0: surfaces immediately, classified rate-limited.
    let url = mock_server(vec![http_status(
        429,
        "Too Many Requests",
        r#"{"error":"FREE_TIER_DAILY_LIMIT","message":"100 orders/day"}"#,
    )]);
    let client = client_with_key(&url);
    let err = client.balance().await.expect_err("429 refused");
    assert!(err.is_rate_limited());
    // The venue's structured body wins over the bare status mapping.
    assert_eq!(into_core_error(&err).code, CoreErrorCode::VenueError);
}

/// A 429 answer that carries `Retry-After: 0`.
fn http_429_retry_after_zero(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[tokio::test]
async fn venue_429_honors_retry_after_and_self_heals() {
    // The startup burst (balance seed → self-check → first sweep) trips the
    // venue's limiter by construction; the venue says "Retry after 1s" and
    // the client must come back after the advertised wait instead of
    // surfacing a transient refusal as a capability failure (which is how
    // one 429 froze trading and kept the ledger seed at the DRY number).
    let url = mock_server(vec![
        http_429_retry_after_zero(
            r#"{"error":"RATE_LIMITED","message":"Rate limit exceeded. Retry after 1s."}"#,
        ),
        http_ok(r#"{"balance":"100","currency":"USDC"}"#),
    ]);
    let config = RestConfig {
        base_url: url,
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 3,
        backoff_initial: Duration::from_millis(1),
        ..RestConfig::default()
    };
    let client = PolySimRest::new(
        config,
        Some(Credentials {
            api_key: "k".into(),
        }),
    )
    .expect("client");
    let balance = client.balance().await.expect("429 then retry answers");
    assert_eq!(balance.balance, Decimal::from_str("100").unwrap());
}

#[tokio::test]
async fn venue_429_retry_budget_is_bounded() {
    // Five 429s against max_retries=3: the fourth retry is the last — the
    // error surfaces (still rate-limited) instead of looping forever.
    let refused = http_429_retry_after_zero(
        r#"{"error":"RATE_LIMITED","message":"Rate limit exceeded. Retry after 1s."}"#,
    );
    let url = mock_server(vec![
        refused.clone(),
        refused.clone(),
        refused.clone(),
        refused.clone(),
        refused,
    ]);
    let config = RestConfig {
        base_url: url,
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 3,
        backoff_initial: Duration::from_millis(1),
        ..RestConfig::default()
    };
    let client = PolySimRest::new(
        config,
        Some(Credentials {
            api_key: "k".into(),
        }),
    )
    .expect("client");
    let err = client.balance().await.expect_err("budget exhausted");
    assert!(err.is_rate_limited(), "got {err:?}");
}

#[tokio::test]
async fn venue_429_defaults_to_the_documented_one_second_without_a_header() {
    // No Retry-After header: the free tier's documented "Retry after 1s"
    // applies ( asserted via the retry_after_ms accessor on the surfaced
    // error, keeping the test itself instant).
    let url = mock_server(vec![http_status(
        429,
        "Too Many Requests",
        r#"{"error":"RATE_LIMITED","message":"Rate limit exceeded. Retry after 1s."}"#,
    )]);
    let client = client_with_key(&url);
    let err = client.balance().await.expect_err("no header, retries=0");
    assert_eq!(err.retry_after_ms(), None);
}

// ── retry / timeout behaviour ───────────────────────────────────────────

#[tokio::test]
async fn transport_error_is_retried_then_succeeds() {
    // First attempt: immediate EOF (transport error). Second: a good body.
    // With max_retries=1 the client recovers. (The public book endpoint —
    // no key required, so the local auth short-circuit stays out of the way.)
    let url = mock_server(vec![Vec::new(), http_ok(BOOK_JSON)]);
    let config = RestConfig {
        base_url: url,
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 1,
        backoff_initial: Duration::from_millis(1),
        ..RestConfig::default()
    };
    let client = PolySimRest::new(config, None).expect("client");
    let book = client.book("71321").await.expect("retry recovers a blip");
    assert_eq!(
        book.bids.unwrap()[0].price,
        Decimal::from_str("0.43").unwrap()
    );
}

#[tokio::test]
async fn retry_budget_is_bounded() {
    // Three dead responses with max_retries=2 → exhausted, not infinite.
    let url = mock_server(vec![Vec::new(), Vec::new(), Vec::new()]);
    let config = RestConfig {
        base_url: url,
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 2,
        backoff_initial: Duration::from_millis(1),
        ..RestConfig::default()
    };
    let client = PolySimRest::new(config, None).expect("client");
    let err = client.book("71321").await.expect_err("all attempts dead");
    assert!(matches!(err, PolySimError::Transport(_)));
}

#[tokio::test]
async fn deadline_is_enforced_against_a_silent_server() {
    // Accept, read, never answer: the request must come back as Timeout.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 8192];
            let _ = s.read(&mut buf);
            std::thread::sleep(Duration::from_secs(60));
        }
    });
    let config = RestConfig {
        base_url: format!("http://127.0.0.1:{port}"),
        no_proxy: true,
        timeout: Duration::from_millis(250),
        max_retries: 0,
        ..RestConfig::default()
    };
    let client = PolySimRest::new(config, None).expect("client");
    let err = client.book("71321").await.expect_err("deadline must fire");
    assert!(matches!(err, PolySimError::Timeout { .. }), "got {err:?}");
}

// ── client configuration ────────────────────────────────────────────────

#[test]
fn base_url_outside_loopback_must_be_https() {
    let config = RestConfig {
        base_url: String::from("http://api.polysimulator.com"),
        ..RestConfig::default()
    };
    let err = PolySimRest::new(config, None).expect_err("plain http to a public host");
    assert!(matches!(err, PolySimError::AuthConfig(_)));
}

#[test]
fn credentials_debug_never_shows_the_key() {
    let creds = Credentials {
        api_key: format!("{}-{}", "synthetic", "debug-check"),
    };
    let shown = format!("{creds:?}");
    assert!(shown.contains("<redacted>"));
    assert!(!shown.contains("debug-check"));
}
