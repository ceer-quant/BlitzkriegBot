//! Panel auth acceptance.
//!
//! Two regimes are covered, because the panel has two:
//!
//! * **Read-only mode** (`WebServer::new`) — no command route, no lifecycle
//!   verbs. Auth is opt-in via credentials; the origin gate is unconditional.
//! * **Gateway mode** (`WebServer::with_gateway`) — the surface can start and
//!   stop the trading core, so a session is mandatory, and credentials are
//!   required from the environment (the process never invents a password).
//!
//! Plus the CSRF regression from the acceptance run: a state-changing request
//! that carries *no* `Origin` header must not be treated as a trusted local
//! caller (a cross-site `<form>`/`<img>` produces exactly that shape).
//!
//! Server is driven over a real socket on an ephemeral loopback port with a tiny
//! std client.

use crate::core::ipc_client::IpcClient;
use crate::gateway::{Dispatcher, SupervisorConfig};
use crate::web::{WebServer, SESSION_IDLE_MS, SESSION_TTL_MS};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Bind :0 and drive the real handle loop on a thread; arms the given
/// credentials when `creds` is Some.
fn start(creds: Option<(&str, &str)>) -> SocketAddr {
    start_with(creds, false)
}

/// As [`start`], but `gateway` builds the server in gateway mode (auth always
/// required) and returns the chosen credentials so a test can log in.
///
/// The server binds `:0` and the test reads the port back from the panel's own
/// listener (`bound_addr`). Probing for a free port and binding it a moment
/// later loses the port to whatever else on the machine takes it in between —
/// the connect then fails with "connection refused" and the test looks like an
/// auth bug.
fn start_with(creds: Option<(&str, &str)>, gateway: bool) -> SocketAddr {
    // A socket path that is never contacted — snapshot calls degrade offline;
    // auth behavior is checked before IPC.
    let sock = std::env::temp_dir().join(format!(
        "uikit-auth-test-{}.sock",
        std::process::id() as u64
    ));
    let path = sock.to_string_lossy().to_string();
    let mut server = if gateway {
        // `lifecycle_enabled = false`: this exercises the auth gate, not process
        // control. The verbs are refused by the dispatcher's own flag either way.
        WebServer::with_gateway(
            IpcClient::new(path.clone()),
            10,
            Dispatcher::new(SupervisorConfig::from_env(path), false),
        )
    } else {
        WebServer::new(IpcClient::new(path), 10)
    };
    if let Some((u, p)) = creds {
        server.set_panel_credentials(Some(u.to_string()), Some(p.to_string()));
    }
    let server = Arc::new(server);
    let serving = Arc::clone(&server);
    thread::spawn(move || {
        let _ = serving.serve("127.0.0.1:0");
    });
    wait_for_bound(&server)
}

fn request(addr: SocketAddr, raw: &str) -> u16 {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn request_body(addr: SocketAddr, raw: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = out
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or("")
        .to_string();
    (status, body)
}

#[test]
fn login_with_wrong_password_is_rejected() {
    let addr = start(Some(("admin", "s3cret")));
    let (code, _) = request_body(
        addr,
        "POST /api/login HTTP/1.1\r\nContent-Length: 33\r\n\r\n{\"user\":\"admin\",\"password\":\"wrongx\"",
    );
    assert_eq!(code, 401, "wrong password must 401");
}

#[test]
fn login_then_session_passes_via_header_query_and_cookie() {
    let addr = start(Some(("admin", "s3cret")));
    let body = format!(
        "POST /api/login HTTP/1.1\r\nContent-Length: {}\r\n\r\n{{\"user\":\"admin\",\"password\":\"s3cret\"}}",
        "{\"user\":\"admin\",\"password\":\"s3cret\"}".len()
    );
    let (code, out) = request_body(addr, &body);
    assert_eq!(code, 200, "good credentials must 200");
    let token = serde_json::from_str::<serde_json::Value>(&out)
        .ok()
        .and_then(|v| v["token"].as_str().map(String::from))
        .expect("login must return a token");

    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nX-Auth-Token: {token}\r\n\r\n")
        ),
        200,
        "session header passes"
    );
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot?token={token} HTTP/1.1\r\n\r\n")
        ),
        200,
        "session query passes"
    );
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nCookie: bk_session={token}\r\n\r\n")
        ),
        200,
        "session cookie passes"
    );
}

#[test]
fn api_without_session_is_rejected_when_creds_armed() {
    let addr = start(Some(("admin", "s3cret")));
    assert_eq!(request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"), 401);
    assert_eq!(
        request(addr, "GET /api/snapshot?token=NOPE HTTP/1.1\r\n\r\n"),
        401
    );
    // The static panel itself stays reachable (logged-in gate lives in the UI).
    assert_eq!(request(addr, "GET /panel HTTP/1.1\r\n\r\n"), 200);
}

#[test]
fn no_auth_configured_passes_loopback() {
    let addr = start(None);
    assert_eq!(request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"), 200);
}

#[test]
fn half_configured_credentials_do_not_arm_a_passwordless_panel() {
    // Only one half set must not silently arm a panel that anyone can open. The
    // pair is refused wholesale, leaving the read-only surface unauthenticated —
    // and gateway mode, which must have a password, refuses to start instead of
    // choosing one (see `gateway_mode_refuses_to_start_without_env_credentials`).
    let sock = std::env::temp_dir().join("uikit-auth-half.sock");
    let mut server = WebServer::new(IpcClient::new(sock.to_string_lossy().to_string()), 10);
    server.set_panel_credentials(Some("admin".to_string()), None);
    let server = Arc::new(server);
    let serving = Arc::clone(&server);
    thread::spawn(move || {
        let _ = serving.serve("127.0.0.1:0");
    });
    let addr = wait_for_bound(&server);
    assert_eq!(
        request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"),
        200,
        "a lone user half must not arm auth against an empty password"
    );
}

/// Wait for a server started on `:0` to report the port it actually bound.
fn wait_for_bound(server: &Arc<WebServer>) -> SocketAddr {
    try_wait_for_bound(server, 500).expect("the panel never reported a bound address")
}

/// As [`wait_for_bound`], but `None` after `attempts` tries — for a bind this
/// environment may refuse outright.
fn try_wait_for_bound(server: &Arc<WebServer>, attempts: usize) -> Option<SocketAddr> {
    for _ in 0..attempts {
        if let Some(addr) = server.bound_addr().and_then(|a| a.parse().ok()) {
            return Some(addr);
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Log in and return the session token.
fn login(addr: SocketAddr, user: &str, password: &str) -> String {
    let body = format!("{{\"user\":\"{user}\",\"password\":\"{password}\"}}");
    let (code, out) = request_body(
        addr,
        &format!(
            "POST /api/login HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    assert_eq!(code, 200, "login with correct credentials must 200");
    serde_json::from_str::<serde_json::Value>(&out)
        .ok()
        .and_then(|v| v["token"].as_str().map(String::from))
        .expect("login must return a token")
}

#[test]
fn cross_origin_refused_and_loopback_origin_ok() {
    let addr = start(Some(("admin", "s3cret")));
    let token = login(addr, "admin", "s3cret");

    assert_eq!(
        request(
            addr,
            &format!(
                "GET /api/snapshot?token={token} HTTP/1.1\r\nOrigin: http://evil.example\r\n\r\n"
            )
        ),
        403,
        "foreign Origin must 403"
    );
    assert_eq!(
        request(
            addr,
            &format!(
                "GET /api/snapshot?token={token} HTTP/1.1\r\nOrigin: http://127.0.0.1\r\n\r\n"
            )
        ),
        200,
        "loopback origin passes"
    );
    // Loopback spelled as a full panel URL, with port and no trailing slash.
    assert_eq!(
        request(
            addr,
            &format!(
                "GET /api/snapshot?token={token} HTTP/1.1\r\nOrigin: http://localhost:51888\r\n\r\n"
            )
        ),
        200,
        "localhost:port passes"
    );
    // A *foreign* Referer is refused even with no Origin: some browsers send only
    // this on form posts.
    assert_eq!(
        request(
            addr,
            &format!(
                "GET /api/snapshot?token={token} HTTP/1.1\r\nReferer: http://evil.example/x\r\n\r\n"
            )
        ),
        403,
        "foreign Referer must 403"
    );
}

#[test]
fn session_accepts_bearer_form() {
    let addr = start(Some(("admin", "s3cret")));
    let token = login(addr, "admin", "s3cret");
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n")
        ),
        200,
        "Bearer session form passes"
    );
}

// ── CSRF ────────────────────────────────────────────────────────────────────
//
// Reproduced live before this suite existed: a second instance started with no
// credentials answered `GET /api/command?cmd=status` with 200 and no `Origin`
// header, while `Origin: http://evil.example` correctly got 403. That shape —
// a cross-site `GET` carrying no origin header — is exactly what an
// `<img src="http://127.0.0.1:51888/api/command?cmd=stop">` on any page the
// operator visits produces, so an image tag could stop the trading core.
//
// The fix is not the origin gate: a `<form>` or `fetch(no-cors)` POST *does*
// carry a foreign `Origin` and was already refused, and refusing header-less
// requests would only break curl and the gate scripts. What closes the vector is
// that gateway mode now always requires a session, and no `<img>`, `<form>` or
// cross-site `fetch` can attach a token.

#[test]
fn cross_site_get_cannot_reach_a_lifecycle_verb() {
    // Gateway mode, armed exactly as the binary arms it (env credentials).
    let addr = start_with(Some(("ops", "s3cret")), true);
    for uri in [
        "/api/command?cmd=stop",
        "/api/command?cmd=start",
        "/api/command?cmd=status",
    ] {
        assert_eq!(
            request(addr, &format!("GET {uri} HTTP/1.1\r\n\r\n")),
            401,
            "{uri} must not be reachable from a bare cross-site GET"
        );
    }
    // A foreign Origin is refused even when a session is presented — a stolen
    // token is not a licence for another site to drive the panel.
    assert_eq!(
        request(
            addr,
            "POST /api/command HTTP/1.1\r\nOrigin: http://evil.example\r\nContent-Length: 4\r\n\r\nstop"
        ),
        403,
        "foreign Origin is refused before the session check"
    );
}

#[test]
fn absent_origin_is_allowed_so_cli_clients_still_work() {
    // The other half of the trade-off: header-less requests from curl, the gate
    // scripts and the panel's own same-origin fetches must keep working. A
    // browser cannot produce a cross-origin POST without `Origin`, so allowing
    // this costs nothing.
    let addr = start(None); // read-only, auth off
    assert_eq!(
        request(
            addr,
            "POST /api/command HTTP/1.1\r\nContent-Length: 6\r\n\r\nstatus"
        ),
        200,
        "a header-less POST from a CLI client passes the origin gate"
    );
    assert_eq!(
        request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"),
        200,
        "and so does a header-less GET"
    );
}

#[test]
fn ping_is_reachable_without_a_session_and_discloses_nothing() {
    let addr = start(Some(("admin", "s3cret")));
    let (code, body) = request_body(addr, "GET /api/ping HTTP/1.1\r\n\r\n");
    assert_eq!(code, 200, "/ping must work without a session");
    let doc: serde_json::Value = serde_json::from_str(&body).expect("ping returns JSON");
    assert_eq!(doc["ok"], serde_json::json!(true));
    assert_eq!(doc["authRequired"], serde_json::json!(true));
    // The probe must not become a snapshot backdoor.
    for leaked in [
        "balance",
        "positions",
        "trades",
        "strategyStats",
        "available",
    ] {
        assert!(
            !body.contains(leaked),
            "/ping must not leak `{leaked}`: {body}"
        );
    }
}

#[test]
fn a_credential_less_gateway_still_locks_everything_but_ping() {
    // Defence in depth. `ui_kit_web` calls `require_credentials` and exits, so a
    // credential-less gateway is unreachable through the binary — but the server
    // must not depend on the binary having done that. Even constructed directly,
    // it refuses every read and every command rather than exposing a lifecycle
    // verb to whatever reaches the port.
    let addr = start_with(None, true);
    assert_eq!(
        request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"),
        401,
        "gateway snapshot requires a session"
    );
    assert_eq!(
        request(
            addr,
            "POST /api/command HTTP/1.1\r\nOrigin: http://127.0.0.1:51888\r\nContent-Length: 6\r\n\r\nstatus"
        ),
        401,
        "gateway command requires a session even with a loopback origin"
    );
    assert_eq!(
        request(addr, "GET /api/ping HTTP/1.1\r\n\r\n"),
        200,
        "but /ping still tells the client the gateway is alive and locked"
    );
}

#[test]
fn gateway_mode_refuses_to_start_without_env_credentials() {
    // Credentials come from the environment only. Gateway mode must refuse to
    // serve rather than invent a password: a process-chosen secret is one the
    // operator cannot manage, and printing it makes the terminal the source of
    // truth for who can stop the core.
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);
    let sock = std::env::temp_dir().join("uikit-auth-env-only.sock");
    let sock = sock.to_string_lossy().to_string();
    let mut server = WebServer::with_gateway(
        IpcClient::new(sock),
        10,
        Dispatcher::new(SupervisorConfig::from_env(String::new()), false),
    );
    assert!(
        server.auth_required(),
        "gateway mode always requires a session"
    );
    assert!(!server.credentials_configured());

    let why = server
        .require_credentials()
        .expect_err("gateway mode with no env credentials must not start");
    assert!(
        why.contains("BLITZKRIEG_PANEL_USER") && why.contains("BLITZKRIEG_PANEL_PASSWORD"),
        "the refusal must name the variables to set, got: {why}"
    );
    // There is no password of the process's own making to log in with.
    assert!(server.login("admin", "admin").is_none());
    assert!(server.login("admin", "").is_none());

    // A half-configured pair is still unconfigured — guessing which half was
    // meant is how a panel ends up open.
    server.set_panel_credentials(Some("ops".to_string()), None);
    assert!(
        server.require_credentials().is_err(),
        "user without password"
    );
    server.set_panel_credentials(None, Some("pw".to_string()));
    assert!(
        server.require_credentials().is_err(),
        "password without user"
    );
    server.set_panel_credentials(Some("  ".to_string()), Some("pw".to_string()));
    assert!(server.require_credentials().is_err(), "blank user is unset");

    // A complete pair starts, and the session it issues works.
    server.set_panel_credentials(Some("ops".to_string()), Some("chosen-pw".to_string()));
    assert!(server.credentials_configured());
    server
        .require_credentials()
        .expect("a complete pair starts");
    assert!(server.login("ops", "chosen-pw").is_some());
    assert!(server.login("ops", "wrong").is_none());

    thread::spawn(move || {
        let _ = server.serve(&addr.to_string());
    });
    thread::sleep(Duration::from_millis(150));
    let token = login(addr, "ops", "chosen-pw");
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nX-Auth-Token: {token}\r\n\r\n")
        ),
        200,
        "session from the env-armed server passes"
    );
    assert_eq!(
        request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"),
        401,
        "and the same server still refuses a sessionless read"
    );
}

// ── Session lifetime ────────────────────────────────────────────────────────

#[test]
fn session_tokens_are_unpredictable_and_distinct() {
    // 160 bits from the OS, so two logins must never collide; the previous
    // timestamp/PID/address mix could repeat across restarts.
    let probes: Vec<String> = (0..64).map(|_| crate::web::random_hex(20)).collect();
    let mut unique = probes.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), probes.len(), "tokens must not repeat");
    assert!(probes
        .iter()
        .all(|t| t.len() == 40 && t.chars().all(|c| c.is_ascii_hexdigit())));
}

#[test]
fn sessions_expire_on_ttl_and_on_idle() {
    // The eviction predicate is time-driven, so it is unit-tested against a
    // synthetic clock rather than by sleeping for hours.
    let now = 1_000_000_000_000u64;
    let mut sessions = std::collections::BTreeMap::new();
    let mk = |issued: u64, seen: u64| crate::web::Session {
        issued_ms: issued,
        seen_ms: seen,
    };
    sessions.insert("fresh".to_string(), mk(now - 60_000, now - 30_000));
    sessions.insert(
        "idle".to_string(),
        mk(now - SESSION_IDLE_MS - 1, now - SESSION_IDLE_MS - 1),
    );
    sessions.insert(
        "aged".to_string(),
        mk(now - SESSION_TTL_MS - 1, now - 1_000),
    );

    WebServer::evict_expired(&mut sessions, now);

    assert!(sessions.contains_key("fresh"), "an active session survives");
    assert!(
        !sessions.contains_key("idle"),
        "a session idle past {SESSION_IDLE_MS}ms must be dropped"
    );
    assert!(
        !sessions.contains_key("aged"),
        "a session past its absolute TTL is dropped even if recently used"
    );
}

#[test]
fn logout_revokes_the_session() {
    let addr = start(Some(("admin", "s3cret")));
    let token = login(addr, "admin", "s3cret");
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nX-Auth-Token: {token}\r\n\r\n")
        ),
        200
    );
    assert_eq!(
        request(
            addr,
            &format!("POST /api/logout?token={token} HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
        ),
        200,
        "logout is accepted"
    );
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nX-Auth-Token: {token}\r\n\r\n")
        ),
        401,
        "the revoked token must not work again"
    );
}

#[test]
fn live_sessions_are_capped() {
    let addr = start(Some(("admin", "s3cret")));
    let first = login(addr, "admin", "s3cret");
    // MAX_SESSIONS is 64; 80 logins must not grow the set without bound, and the
    // newest session must still work afterwards.
    for _ in 0..80 {
        let _ = login(addr, "admin", "s3cret");
    }
    let last = login(addr, "admin", "s3cret");
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nX-Auth-Token: {last}\r\n\r\n")
        ),
        200,
        "the most recent session always works"
    );
    // Oldest-first eviction means the very first session is gone.
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nX-Auth-Token: {first}\r\n\r\n")
        ),
        401,
        "the oldest session is evicted once the cap is hit"
    );
}

#[test]
fn basic_auth_still_accepts_the_legacy_token_form() {
    // E6-a shipped a `Basic <token>` form; existing saved clients keep working.
    let addr = start(Some(("admin", "s3cret")));
    let token = login(addr, "admin", "s3cret");
    let b64 = b64_encode(format!("{token}:").as_bytes());
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nAuthorization: Basic {b64}\r\n\r\n")
        ),
        200,
        "legacy Basic token form passes"
    );
}

/// Standard base64, for building the legacy `Basic` header above. The server
/// only ships a decoder (`data_encoding_free_base64`), so the encoder lives
/// here — encoding is a test concern.
fn b64_encode(input: &[u8]) -> String {
    const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let quads = [
            TBL[(n >> 18) as usize & 63],
            TBL[(n >> 12) as usize & 63],
            TBL[(n >> 6) as usize & 63],
            TBL[n as usize & 63],
        ];
        for (i, q) in quads.iter().enumerate() {
            // Pad only the bytes that were not present.
            if i <= chunk.len() {
                out.push(*q as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn origin_matching_accepts_only_loopback_spellings() {
    use crate::web::is_loopback_origin;
    for ok in [
        "http://127.0.0.1",
        "http://127.0.0.1:51888",
        "https://localhost",
        "http://localhost:51888/",
        "HTTP://127.0.0.1:1",
        "http://[::1]:51888",
        "http://127.0.0.15:8080",
    ] {
        assert!(is_loopback_origin(ok), "{ok} must be accepted");
    }
    for bad in [
        "http://evil.example",
        "http://127.0.0.1.evil.example", // suffix attack on a prefix check
        "http://localhost.evil.example", // ditto for the hostname branch
        "http://evil.example/#http://127.0.0.1",
        "https://127.0.0.1@evil.example", // userinfo trick
        "http://notlocalhost",
        "null",
    ] {
        assert!(!is_loopback_origin(bad), "{bad} must be refused");
    }
}

#[test]
fn same_origin_on_a_public_ip_is_allowed_the_server_deployment_case() {
    // The server deployment: the panel binds 0.0.0.0 and a browser visits it
    // via the machine's real address. The browser attaches
    // `Referer: http://<that-address>:51888/…` and `Host: <that-address>:51888`
    // — a same-origin pair that must pass the gate WITHOUT any allowlist
    // entry, or every non-loopback deployment 403s its own panel.
    let addr = start(Some(("admin", "s3cret")));
    let token = login(addr, "admin", "s3cret");
    // The raw request names a NON-loopback Host on purpose: the server must
    // judge same-origin against the Host the request declares, not against
    // the interface it happens to be bound to.
    assert_eq!(
        request(
            addr,
            &format!(
                "GET /api/snapshot?token={token} HTTP/1.1\r\n\
                 Host: 203.0.113.7:51888\r\n\
                 Referer: http://203.0.113.7:51888/panel/\r\n\r\n"
            )
        ),
        200,
        "same-origin (Referer authority == Host header) must pass on a public IP"
    );
    // Origin instead of Referer — the shape a same-origin fetch produces.
    assert_eq!(
        request(
            addr,
            &format!(
                "GET /api/snapshot?token={token} HTTP/1.1\r\n\
                 Host: 203.0.113.7:51888\r\n\
                 Origin: http://203.0.113.7:51888\r\n\r\n"
            )
        ),
        200,
        "same-origin Origin header must pass too"
    );
    // Port matters: a DIFFERENT port on the same machine is a different origin.
    assert_eq!(
        request(
            addr,
            &format!(
                "GET /api/snapshot?token={token} HTTP/1.1\r\n\
                 Host: 203.0.113.7:51888\r\n\
                 Referer: http://203.0.113.7:9999/panel/\r\n\r\n"
            )
        ),
        403,
        "same host on another port is a different origin and must 403"
    );
    // The loosening must not weaken the cross-origin rule: a hostile page's
    // origin cannot match the Host header of a request to OUR server.
    assert_eq!(
        request(
            addr,
            &format!(
                "GET /api/snapshot?token={token} HTTP/1.1\r\n\
                 Host: 203.0.113.7:51888\r\n\
                 Origin: http://evil.example\r\n\r\n"
            )
        ),
        403,
        "cross-origin stays refused even with a Host header present"
    );
}

#[test]
fn origin_authority_normalizes_scheme_case_and_default_ports() {
    use crate::web::origin_authority;
    let (h, p) = origin_authority("HTTP://Panel.Example.COM").expect("authority");
    assert_eq!((h.as_str(), p.as_str()), ("panel.example.com", "80"));
    let (h, p) = origin_authority("https://panel.example.com:51888/panel/").expect("authority");
    assert_eq!((h.as_str(), p.as_str()), ("panel.example.com", "51888"));
    let (h, p) = origin_authority("https://[2001:db8::1]:51888").expect("authority");
    assert_eq!((h.as_str(), p.as_str()), ("[2001:db8::1]", "51888"));
    assert!(origin_authority("http://").is_none(), "no authority at all");
    // user@host — credentials in the URL are ignored, the host is not.
    assert_eq!(
        origin_authority("http://user@host.example/path").expect("authority"),
        ("host.example".to_string(), "80".to_string())
    );
}

#[test]
fn allowlisted_origin_is_accepted_without_being_same_origin() {
    // The reverse-proxy shape: the operator fronts the panel with a hostname
    // the proxy rewrites, so the visible origin never equals the Host the
    // server sees. `--allowed-origin` (BLITZKRIEG_ALLOWED_ORIGINS) lists it.
    let client = IpcClient::new("/nonexistent-panel-allowlist.sock");
    let mut server = WebServer::with_gateway(
        client,
        0,
        Dispatcher::new(
            SupervisorConfig::from_env("/nonexistent-panel-allowlist.sock".into()),
            true,
        ),
    );
    server.set_allowed_origins(vec!["http://panel.example.com".to_string()]);

    let req = |origin: &str| crate::web::HttpRequest {
        method: "GET".into(),
        target: "/api/snapshot".into(),
        body: String::new(),
        headers: vec![
            ("host".into(), "internal.host:51888".into()),
            ("origin".into(), origin.into()),
        ],
    };
    assert!(
        server.origin_allowed(&req("http://panel.example.com")),
        "the allowlisted origin passes"
    );
    assert!(
        server.origin_allowed(&req("http://panel.example.com/panel/")),
        "a path under an allowlisted origin passes"
    );
    assert!(
        !server.origin_allowed(&req("http://other.example")),
        "any other foreign origin stays refused"
    );
}

// ---------------------------------------------------------------------------
// #185 — the panel's own exposure: login throttle and non-loopback binds
// ---------------------------------------------------------------------------

/// One raw request → (status, header block, body).
fn request_full(addr: SocketAddr, raw: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(raw.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((out.as_str(), ""));
    (status, head.to_string(), body.to_string())
}

/// One login attempt → (status, header block, body).
fn login_attempt(addr: SocketAddr, user: &str, password: &str) -> (u16, String, String) {
    let body = format!("{{\"user\":\"{user}\",\"password\":\"{password}\"}}");
    request_full(
        addr,
        &format!(
            "POST /api/login HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

#[test]
fn repeated_failed_logins_lock_the_client_out() {
    // #185 acceptance: the panel used to answer an unlimited number of guesses
    // at full speed. Ten failures must spend the budget and lock the client out.
    let addr = start(Some(("admin", "s3cret")));
    for i in 1..=crate::web::MAX_LOGIN_FAILURES {
        let (code, _, body) = login_attempt(addr, "admin", "wrong-password");
        assert_eq!(
            code, 401,
            "failure {i} is under the threshold: 401 expected"
        );
        assert!(body.contains("用户名或密码错误"), "body: {body}");
    }

    // The threshold is spent: the next attempt is refused WITHOUT the
    // credentials being examined at all.
    let (code, head, body) = login_attempt(addr, "admin", "wrong-password");
    assert_eq!(code, 429, "past the threshold the client is locked out");
    assert!(
        head.to_ascii_lowercase().contains("retry-after:"),
        "a lockout must say when to come back: {head}"
    );

    // The lockout answer does not depend on the credentials. The CORRECT
    // password gets the same status and the same body — otherwise the panel
    // would confirm a guessed password while claiming to be locked, which is
    // exactly the leak the audit asked to avoid.
    let (code_ok, _, body_ok) = login_attempt(addr, "admin", "s3cret");
    assert_eq!(
        code_ok, 429,
        "the correct password is refused too, by design"
    );
    assert_eq!(
        body, body_ok,
        "locked responses must not vary with the credentials supplied"
    );
    assert!(
        !body.contains("用户名或密码错误"),
        "a lockout must not report the password as wrong: {body}"
    );
}

#[test]
fn a_successful_login_clears_the_failure_counter() {
    // A client that gets the password right is not half-way to a lockout: the
    // counter is per-client state about FAILURES, not a permanent score.
    let addr = start(Some(("admin", "s3cret")));
    for _ in 0..3 {
        assert_eq!(login_attempt(addr, "admin", "nope").0, 401);
    }
    assert_eq!(login_attempt(addr, "admin", "s3cret").0, 200);

    // Nine more failures. If the counter had NOT been reset, the running total
    // (12) would already be past the threshold and one of these would 429.
    for i in 1..=9 {
        let (code, _, _) = login_attempt(addr, "admin", "nope");
        assert_eq!(
            code, 401,
            "attempt {i} after a success must be a plain 401, not a lockout"
        );
    }
}

#[test]
fn the_loopback_default_is_reported_as_local_only() {
    // The default the audit asks for: the panel is reachable from this machine
    // and says so on the wire, with no exposure warning.
    let addr = start(None);
    let (code, _, body) = request_full(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n");
    assert_eq!(code, 200);
    let doc: serde_json::Value = serde_json::from_str(&body).expect("snapshot json");
    assert_eq!(
        doc["security"]["loopbackOnly"],
        serde_json::json!(true),
        "a loopback bind must be reported as local-only"
    );
    assert!(
        doc["security"]["warning"].is_null(),
        "and must not raise an exposure warning"
    );
    assert_eq!(
        doc["security"]["login"]["maxFailures"],
        serde_json::json!(crate::web::MAX_LOGIN_FAILURES)
    );
}

#[test]
fn a_non_loopback_bind_is_recorded_and_shown_on_the_panel() {
    // The deliberate-exposure case. It must be loud in three places: the
    // startup log, the JSON a panel reads, and the built-in HTML panel itself.
    let server = Arc::new(WebServer::new(
        IpcClient::new("/nonexistent-wildcard-bind.sock"),
        10,
    ));
    let shared = Arc::clone(&server);
    thread::spawn(move || {
        let _ = shared.serve("0.0.0.0:0");
    });
    let Some(bound) = try_wait_for_bound(&server, 300) else {
        eprintln!("skipping: this environment does not allow a wildcard bind");
        return;
    };
    assert!(
        !bound.ip().is_loopback(),
        "bound {bound} must not be loopback"
    );

    let warning = server
        .bind_warning()
        .expect("a wildcard bind must produce an exposure warning");
    assert!(warning.contains("非本地地址"), "warning: {warning}");
    assert!(
        warning.contains("127.0.0.1:51888"),
        "the warning must say how to get back to loopback: {warning}"
    );
    assert!(
        warning.contains("明文"),
        "the warning must name the plaintext-HTTP exposure: {warning}"
    );

    let doc = server.security_doc();
    assert_eq!(doc["loopbackOnly"], serde_json::json!(false));
    assert!(
        doc["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("非本地地址"),
        "the JSON must carry the same warning the log printed"
    );

    // And the panel a human actually looks at renders it as a red banner.
    let html = crate::web::render_html_full(
        &crate::core::types::UiSnapshot::default(),
        false,
        None,
        server.bind_warning().as_deref(),
    );
    assert!(
        html.contains("面板正在监听非本地地址"),
        "the built-in panel must show the exposure"
    );
}
