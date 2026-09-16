//! Panel auth acceptance.
//!
//! Two regimes are covered, because the panel has two:
//!
//! * **Read-only mode** (`WebServer::new`) — no command route, no lifecycle
//!   verbs. Auth is opt-in via credentials; the origin gate is unconditional.
//! * **Gateway mode** (`WebServer::with_gateway`) — the surface can start and
//!   stop the trading core, so a session is mandatory whether or not credentials
//!   were configured.
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
use std::thread;
use std::time::Duration;

/// Bind :0 and drive the real handle loop on a thread; arms the given
/// credentials when `creds` is Some.
fn start(creds: Option<(&str, &str)>) -> SocketAddr {
    start_with(creds, false)
}

/// As [`start`], but `gateway` builds the server in gateway mode (auth always
/// required) and returns the chosen credentials so a test can log in.
fn start_with(creds: Option<(&str, &str)>, gateway: bool) -> SocketAddr {
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);

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
    thread::spawn(move || {
        let _ = server.serve(&addr.to_string());
    });
    thread::sleep(Duration::from_millis(150));
    addr
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
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("").to_string();
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
        request(addr, &format!("GET /api/snapshot?token={token} HTTP/1.1\r\n\r\n")),
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
    // and gateway mode, which *must* have a password, mints one instead (see
    // `gateway_mode_requires_a_session_even_without_credentials`).
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);
    let sock = std::env::temp_dir().join("uikit-auth-half.sock");
    let mut server = WebServer::new(IpcClient::new(sock.to_string_lossy().to_string()), 10);
    server.set_panel_credentials(Some("admin".to_string()), None);
    thread::spawn(move || {
        let _ = server.serve(&addr.to_string());
    });
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"),
        200,
        "a lone user half must not arm auth against an empty password"
    );
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
            &format!("GET /api/snapshot?token={token} HTTP/1.1\r\nOrigin: http://127.0.0.1\r\n\r\n")
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
    // Gateway mode with no env credentials: the configuration that was reachable.
    let addr = start_with(None, true);
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
    for leaked in ["balance", "positions", "trades", "strategyStats", "available"] {
        assert!(
            !body.contains(leaked),
            "/ping must not leak `{leaked}`: {body}"
        );
    }
}

#[test]
fn gateway_mode_requires_a_session_even_without_credentials() {
    // The important half of the fix: an operator who forgets to set the env vars
    // must get a *locked* panel, not an open command surface.
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
fn minted_credentials_lock_the_panel_and_are_usable() {
    // `ensure_credentials` is what the binary calls in gateway mode: with no env
    // credentials it must mint a password rather than leave the panel open.
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);
    let sock = std::env::temp_dir().join("uikit-auth-mint.sock");
    let sock = sock.to_string_lossy().to_string();
    let mut server = WebServer::with_gateway(
        IpcClient::new(sock.clone()),
        10,
        Dispatcher::new(SupervisorConfig::from_env(sock), false),
    );
    let minted = server.ensure_credentials().expect("must mint a password");
    assert_eq!(minted.len(), 32, "16 random bytes, hex-encoded");
    assert!(
        minted.chars().all(|c| c.is_ascii_hexdigit()),
        "minted password must be hex, got {minted}"
    );
    assert_eq!(server.generated_password(), Some(minted.as_str()));
    assert!(server.auth_required(), "gateway mode always requires a session");

    // An explicit pair supersedes the minted one — and the minted one dies with
    // it, so a password printed to a scrollback buffer cannot outlive the run.
    server.set_panel_credentials(Some("ops".to_string()), Some("chosen-pw".to_string()));
    assert_eq!(server.generated_password(), None);
    assert!(
        server.login("ops", "chosen-pw").is_some(),
        "explicit credentials work"
    );
    assert!(
        server.login("ops", &minted).is_none(),
        "the superseded one-time password must not log in"
    );

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
        "session from the re-armed server passes"
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
    assert!(probes.iter().all(|t| t.len() == 40 && t.chars().all(|c| c.is_ascii_hexdigit())));
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
        request(addr, &format!("POST /api/logout?token={token} HTTP/1.1\r\nContent-Length: 0\r\n\r\n")),
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
        "http://127.0.0.1.evil.example",   // suffix attack on a prefix check
        "http://localhost.evil.example",   // ditto for the hostname branch
        "http://evil.example/#http://127.0.0.1",
        "https://127.0.0.1@evil.example",  // userinfo trick
        "http://notlocalhost",
        "null",
    ] {
        assert!(!is_loopback_origin(bad), "{bad} must be refused");
    }
}
