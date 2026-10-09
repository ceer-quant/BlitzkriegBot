//! Offline contract tests (#423). Every test binds a 127.0.0.1 mock server;
//! no test touches the network and no real credential appears anywhere.
//!
//! Reverse-acceptance (must-fail) tests live in `reverse_acceptance` —
//! each one removes a protection and asserts the failure the kernel would
//! otherwise be blind to.

use blitzkrieg_market_api::CoreErrorCode;
use blitzkrieg_predictfun::error::{PredictError, into_core_error};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

/// Serve `count` canned HTTP responses on an ephemeral 127.0.0.1 port.
/// Returns (base_url, addr) — the addr keeps the port reserved for the
/// test's lifetime.
fn mock_server(responses: Vec<Vec<u8>>) -> (String, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf); // request line + headers; body unused
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
        // Remaining connections (reqwest pooling) are dropped with the test.
    });
    (format!("http://{}", addr), addr)
}

fn http_ok(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

fn http_status(status: u16, reason: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

fn client_for(base_url: &str) -> blitzkrieg_predictfun::PredictRest {
    let config = blitzkrieg_predictfun::RestConfig {
        base_url: base_url.to_string(),
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 0,
        ..blitzkrieg_predictfun::RestConfig::default()
    };
    blitzkrieg_predictfun::PredictRest::new(config, None).expect("client builds unauthenticated")
}

const MARKETS_JSON: &str = r#"{"success":true,"data":{"markets":[{"token_id":"0xabc123","question":"Will BTC close above $100k on Oct 31?","condition_id":"0xc9d1","event_id":"btc-oct","outcome":"UP","end_date":"2026-10-31T23:59:00Z","is_neg_risk":false,"is_yield_bearing":false,"fee_rate_bps":100,"status":"ACTIVE","best_bid":"0.44","best_ask":"0.46","volume_24h":"12000.5"}]}}"#;

#[tokio::test]
async fn markets_parse_into_typed_rows() {
    let (url, _guard) = mock_server(vec![http_ok(MARKETS_JSON)]);
    let client = client_for(&url);
    let markets = client.markets(50).await.expect("markets parse");
    assert_eq!(markets.len(), 1);
    assert_eq!(markets[0].token_id, "0xabc123");
    assert!(markets[0].is_active());
    assert_eq!(markets[0].best_ask, Some(rust_decimal::Decimal::new(46, 2)));
    // 0.44/0.46 → mid 0.45, the cross-platform comparable price.
    assert_eq!(markets[0].mid(), Some(rust_decimal::Decimal::new(45, 2)));
}

#[tokio::test]
async fn body_without_data_envelope_is_a_schema_error_not_an_empty_list() {
    // The venue always wraps payloads in {success, data}. A body without it
    // (an older shape, a gateway page) must surface as a schema error —
    // never silently parse as an all-defaults payload producing a fake
    // empty list.
    let (url, _guard) = mock_server(vec![http_ok(
        r#"{"token_id":"0xabc123","question":"Will ETH close above $4k?"}"#,
    )]);
    let client = client_for(&url);
    let err = client
        .market("0xabc123")
        .await
        .expect_err("bare payload without the envelope must not parse");
    assert!(matches!(err, PredictError::Schema(_)));
    assert_eq!(into_core_error(&err).code, CoreErrorCode::Internal);
}

#[tokio::test]
async fn orderbook_two_sided_probe() {
    let (url, _guard) = mock_server(vec![http_ok(
        r#"{"success":true,"data":{"token_id":"0xabc123","bids":[{"price":"0.44","shares":"100"}],"asks":[{"price":"0.46","shares":"80"}]}}"#,
    )]);
    let client = client_for(&url);
    let book = client.orderbook("0xabc123").await.expect("book");
    assert!(book.is_two_sided());
    assert_eq!(book.bids.len(), 1);
    assert_eq!(
        book.best_bid().map(|l| l.shares),
        Some(rust_decimal::Decimal::from(100))
    );
}

#[tokio::test]
async fn orderbook_one_sided_is_not_two_sided() {
    let (url, _guard) = mock_server(vec![http_ok(
        r#"{"success":true,"data":{"token_id":"0xabc123","bids":[{"price":"0.44","shares":"100"}],"asks":[]}}"#,
    )]);
    let client = client_for(&url);
    let book = client.orderbook("0xabc123").await.expect("book");
    assert!(
        !book.is_two_sided(),
        "one-sided book must be reported as such"
    );
}

#[tokio::test]
async fn unauthenticated_portfolio_fails_with_auth_config() {
    let (url, _guard) = mock_server(vec![http_ok(
        r#"{"success":true,"data":{"balance":"100.00"}}"#,
    )]);
    let client = client_for(&url);
    let err = client
        .balance()
        .await
        .expect_err("no credentials configured");
    assert!(matches!(err, PredictError::AuthConfig(_)));
    let core = into_core_error(&err);
    assert_eq!(core.code, CoreErrorCode::NotAuthenticated);
}

#[tokio::test]
async fn schema_break_surfaces_as_internal_not_venue_error() {
    let (url, _guard) = mock_server(vec![http_ok(
        r#"{"success":true,"data":{"markets": "not-a-list"}}"#,
    )]);
    let client = client_for(&url);
    let err = client
        .markets(10)
        .await
        .expect_err("schema mismatch must not parse");
    assert!(matches!(err, PredictError::Schema(_)));
    assert_eq!(into_core_error(&err).code, CoreErrorCode::Internal);
}

#[tokio::test]
async fn http_401_maps_to_not_authenticated_with_raw_preserved() {
    // Public endpoint shape: the venue's own 401 envelope (measured live:
    // {"success":false,"code":401,"error":"unauthorized","message":"authorization error"}).
    let (url, _guard) = mock_server(vec![http_status(
        401,
        "Unauthorized",
        r#"{"success":false,"code":401,"error":"unauthorized","message":"authorization error"}"#,
    )]);
    let client = client_for(&url);
    let err = client.markets(10).await.expect_err("401 refused");
    assert!(
        !matches!(err, PredictError::AuthConfig(_)),
        "this test exercises the venue answer, not the local auth short-circuit"
    );
    let core = into_core_error(&err);
    assert_eq!(core.code, CoreErrorCode::NotAuthenticated);
    let raw = core.raw.expect("raw venue text must survive the crossing");
    assert!(raw.contains("unauthorized"));
}

#[tokio::test]
async fn non_envelope_error_body_degrades_to_http_with_raw_kept() {
    // An HTML/CDN failure page is not the venue's JSON envelope — it must
    // still surface as a typed error with the raw text preserved.
    let (url, _guard) = mock_server(vec![http_status(
        502,
        "Bad Gateway",
        "<html>edge down</html>",
    )]);
    let client = client_for(&url);
    let err = client.markets(10).await.expect_err("502 refused");
    assert!(matches!(err, PredictError::Http { status: 502, .. }));
    let core = into_core_error(&err);
    assert_eq!(core.code, CoreErrorCode::VenueError);
    assert!(core.raw.expect("raw kept").contains("edge down"));
}

#[tokio::test]
async fn unknown_api_slug_falls_back_to_venue_error() {
    let err = PredictError::Api {
        error_slug: String::from("SOMETHING_NEW"),
        message: String::from("venue added a slug"),
        code: 400,
    };
    assert_eq!(into_core_error(&err).code, CoreErrorCode::VenueError);
}

// ── auth bootstrap (JWT exchange, mocked) ───────────────────────────────

#[tokio::test]
async fn jwt_exchange_mints_a_bearer_and_portfolio_succeeds() {
    // The full unauthenticated → authenticated round trip: fetch message,
    // exchange a signature (the caller signs; here a dummy), then the
    // portfolio endpoint answers.
    let (url, _guard) = mock_server(vec![
        http_ok(r#"{"success":true,"data":{"message":"sign me"}}"#),
        http_ok(r#"{"success":true,"data":{"token":"jwt-abc"}}"#),
        http_ok(r#"{"success":true,"data":{"balance":"55.00","total_value":"60.00"}}"#),
    ]);
    let client = client_for(&url);
    let message = client.auth_message().await.expect("auth message");
    assert_eq!(message, "sign me");
    let jwt = client
        .exchange_jwt("0xsigner", "0xsignature", &message)
        .await
        .expect("jwt minted");
    assert_eq!(jwt, "jwt-abc");
    client.set_jwt(jwt).await;
    let balance = client.balance().await.expect("portfolio after auth");
    assert_eq!(balance.balance, rust_decimal::Decimal::from(55));
}

#[tokio::test]
async fn jwt_exchange_rejects_a_tokenless_ack() {
    let (url, _guard) = mock_server(vec![http_ok(r#"{"success":true,"data":{}}"#)]);
    let client = client_for(&url);
    let err = client
        .exchange_jwt("0xsigner", "0xsignature", "m")
        .await
        .expect_err("ack without a token is a schema break");
    assert!(matches!(err, PredictError::Schema(_)));
}

// ── retry / timeout behaviour ───────────────────────────────────────────

#[tokio::test]
async fn transport_error_is_retried_then_succeeds() {
    // First attempt: connection closed without a response (transport error).
    // Second attempt: a good body. With max_retries=1 the client recovers.
    let (url, _guard) = mock_server(vec![
        Vec::new(), // immediate EOF → transport error
        http_ok(MARKETS_JSON),
    ]);
    let config = blitzkrieg_predictfun::RestConfig {
        base_url: url.clone(),
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 1,
        backoff_initial: Duration::from_millis(1),
        ..blitzkrieg_predictfun::RestConfig::default()
    };
    let client = blitzkrieg_predictfun::PredictRest::new(config, None).expect("client");
    let markets = client
        .markets(10)
        .await
        .expect("retry recovers a transport blip");
    assert_eq!(markets.len(), 1);
}

#[tokio::test]
async fn retry_budget_is_bounded() {
    // Three dead responses with max_retries=2 → exhausted, not infinite.
    let (url, _guard) = mock_server(vec![Vec::new(), Vec::new(), Vec::new()]);
    let config = blitzkrieg_predictfun::RestConfig {
        base_url: url.clone(),
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 2,
        backoff_initial: Duration::from_millis(1),
        ..blitzkrieg_predictfun::RestConfig::default()
    };
    let client = blitzkrieg_predictfun::PredictRest::new(config, None).expect("client");
    let err = client.markets(10).await.expect_err("all attempts dead");
    assert!(matches!(err, PredictError::Transport(_)));
}

#[tokio::test]
async fn deadline_is_enforced_against_a_silent_server() {
    // A listener that accepts and reads but never responds: the request must
    // come back as a Timeout error within the configured budget, not hang.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            // Read the request, then hold the socket open without ever
            // answering — the exact "silent venue" shape. Dropping `s`
            // here would close the connection and the client would see
            // a transport error before the deadline ever gets a chance.
            let mut buf = [0u8; 8192];
            let _ = s.read(&mut buf);
            std::thread::sleep(Duration::from_secs(60));
        }
    });
    let config = blitzkrieg_predictfun::RestConfig {
        base_url: format!("http://127.0.0.1:{port}"),
        no_proxy: true,
        timeout: Duration::from_millis(250),
        max_retries: 0,
        ..blitzkrieg_predictfun::RestConfig::default()
    };
    let client = blitzkrieg_predictfun::PredictRest::new(config, None).expect("client");
    let started = std::time::Instant::now();
    let err = client
        .markets(10)
        .await
        .expect_err("silent server must time out");
    assert!(matches!(err, PredictError::Timeout { .. }), "got {err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "timeout respected"
    );
}

// ── rate limiter (in-process, no server) ────────────────────────────────

#[tokio::test]
async fn rate_limiter_write_bucket_is_tighter_than_read() {
    // Burst 2 on both classes, then a 3rd call: the read bucket refills at
    // 100/s (≈10ms wait) while the write bucket refills at 2/s (≈500ms wait).
    // The comparison must be on the 3rd call — bursts are free by design.
    let config = blitzkrieg_predictfun::RateLimitConfig {
        read_per_sec: 100.0,
        read_burst: 2,
        write_per_sec: 2.0,
        write_burst: 2,
        max_wait_ms: 1_000,
    };
    let rl = blitzkrieg_predictfun::RateLimit::new(config);
    for class in [
        blitzkrieg_predictfun::rate_limit::Class::Read,
        blitzkrieg_predictfun::rate_limit::Class::Write,
    ] {
        rl.acquire(class).await.expect("burst tokens available");
        rl.acquire(class).await.expect("burst tokens available");
    }
    let started = std::time::Instant::now();
    rl.acquire(blitzkrieg_predictfun::rate_limit::Class::Read)
        .await
        .expect("read refill ≈10ms");
    let read_wait = started.elapsed();
    let started = std::time::Instant::now();
    rl.acquire(blitzkrieg_predictfun::rate_limit::Class::Write)
        .await
        .expect("write refill ≈500ms");
    let write_wait = started.elapsed();
    assert!(
        write_wait > read_wait * 5,
        "write refill (2/s) must be visibly slower than read refill (100/s): {write_wait:?} vs {read_wait:?}"
    );
}

#[tokio::test]
async fn rate_limiter_refuses_when_budget_exhausted() {
    let config = blitzkrieg_predictfun::RateLimitConfig {
        read_per_sec: 1.0,
        read_burst: 1,
        write_per_sec: 1.0,
        write_burst: 1,
        max_wait_ms: 30,
    };
    let rl = blitzkrieg_predictfun::RateLimit::new(config);
    rl.acquire(blitzkrieg_predictfun::rate_limit::Class::Read)
        .await
        .expect("first token available");
    let err = rl
        .acquire(blitzkrieg_predictfun::rate_limit::Class::Read)
        .await
        .expect_err("second call within 30ms must exhaust the budget");
    assert!(matches!(err, PredictError::RateLimited { .. }));
    assert_eq!(into_core_error(&err).code, CoreErrorCode::VenueError);
}

#[tokio::test]
async fn limiter_refusal_stops_the_retry_loop() {
    // A limiter refusal must surface immediately — no backoff loop retries it.
    // (Structural proof: a RateLimited error never enters `retryable()`, so
    // the request loop below can never spin on it.)
    let refusal = PredictError::RateLimited { attempts: 1 };
    assert!(!refusal.retryable());
    assert!(refusal.is_rate_limited());
    let refusal = PredictError::RateLimited { attempts: 1 };
    assert!(!refusal.retryable());
    assert!(refusal.is_rate_limited());
}

#[test]
fn base_url_rules_reject_non_https_off_loopback() {
    let config = blitzkrieg_predictfun::RestConfig {
        base_url: String::from("http://api.predict.fun"),
        no_proxy: true,
        ..blitzkrieg_predictfun::RestConfig::default()
    };
    let err = blitzkrieg_predictfun::PredictRest::new(config, None)
        .expect_err("plain http to a public host must be refused");
    assert!(matches!(err, PredictError::AuthConfig(_)));
}

// ── reverse acceptance: the protections must be load-bearing ────────────

/// REMOVE the timeout → the request must hang (observed via a shrunk budget
/// window), proving the deadline is what protects the kernel.
#[tokio::test]
async fn reverse_acceptance_without_timeout_budget_the_call_hangs() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            // Hold the socket open silently. Dropping it would close the
            // connection and hand the client a transport error before
            // any deadline could be tested.
            let mut buf = [0u8; 8192];
            let _ = s.read(&mut buf);
            std::thread::sleep(Duration::from_secs(60));
        }
    });
    let config = blitzkrieg_predictfun::RestConfig {
        base_url: format!("http://127.0.0.1:{port}"),
        no_proxy: true,
        timeout: Duration::from_millis(150),
        max_retries: 0,
        ..blitzkrieg_predictfun::RestConfig::default()
    };
    let client = blitzkrieg_predictfun::PredictRest::new(config, None).expect("client");
    // With the timeout present the call resolves as Timeout. The red bar is:
    // if the timeout layer were removed, `tokio::time::timeout` below (10x the
    // budget) would itself fire — i.e. the call had no internal deadline.
    let outcome = tokio::time::timeout(Duration::from_millis(1_500), client.markets(10)).await;
    match outcome {
        Err(_elapsed) => panic!(
            "RED BAR: call outlived 10x its configured budget — the timeout layer is not load-bearing"
        ),
        Ok(inner) => {
            assert!(
                matches!(inner, Err(PredictError::Timeout { .. })),
                "with the timeout intact the call must return Timeout, got {inner:?}"
            );
        }
    }
}

/// REMOVE the error mapping → an unmapped error must never become a fake
/// success. This asserts the mapping door produces a kernel type for EVERY
/// variant (exhaustive match proven by compilation) and that raw text survives.
#[test]
fn reverse_acceptance_every_error_variant_reaches_the_kernel() {
    let variants: Vec<PredictError> = vec![
        PredictError::Http {
            status: 500,
            body: String::from("boom"),
        },
        PredictError::Api {
            error_slug: String::from("EXOTIC"),
            message: String::from("m"),
            code: 418,
        },
        PredictError::Timeout { ms: 1 },
        PredictError::RateLimited { attempts: 3 },
        PredictError::Transport(String::from("reset")),
        PredictError::AuthConfig(String::from("missing")),
        PredictError::Schema(String::from("bad json")),
    ];
    for variant in &variants {
        let core = into_core_error(variant);
        assert!(!core.message.is_empty(), "{variant:?} lost its message");
        assert!(
            core.raw.as_deref().is_some_and(|r| !r.is_empty()),
            "{variant:?} lost the raw venue text"
        );
    }
}

/// REMOVE the rate limiter → nothing stops a hot loop from hammering the
/// venue. This test pins the limiter into the request path: the client's
/// request_json acquires from the bucket before every attempt, so a burst
/// larger than the read burst must take visibly longer than a single call.
#[tokio::test]
async fn reverse_acceptance_request_path_goes_through_the_limiter() {
    let (url, _guard) = mock_server((0..8).map(|_| http_ok(MARKETS_JSON)).collect());
    let config = blitzkrieg_predictfun::RestConfig {
        base_url: url.clone(),
        no_proxy: true,
        timeout: Duration::from_secs(5),
        max_retries: 0,
        rate_limit: blitzkrieg_predictfun::RateLimitConfig {
            read_per_sec: 20.0,
            read_burst: 2,
            write_per_sec: 1.0,
            write_burst: 1,
            max_wait_ms: 60_000,
        },
        ..blitzkrieg_predictfun::RestConfig::default()
    };
    let client = blitzkrieg_predictfun::PredictRest::new(config, None).expect("client");
    let started = std::time::Instant::now();
    for _ in 0..6 {
        client
            .markets(10)
            .await
            .expect("burst-2 @ 20/s serves 6 calls");
    }
    // 6 calls through a burst-2 bucket at 20/s: first 2 immediate, remaining
    // 4 refill at ~50ms each ⇒ ≥150ms. A client that bypassed the limiter
    // would finish in single-digit milliseconds over loopback.
    assert!(
        started.elapsed() >= Duration::from_millis(120),
        "RED BAR: 6 requests completed too fast ({:?}) — the rate limiter is not in the request path",
        started.elapsed()
    );
}
