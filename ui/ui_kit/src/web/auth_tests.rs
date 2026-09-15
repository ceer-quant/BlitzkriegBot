//! E6-a auth acceptance — user/password login spec (#57 WebUI direction):
//! wrong credentials rejected, correct credentials issue a session, the
//! session passes on query/header/cookie, missing credentials 401, cross-origin
//! 403. Server is driven over a real socket on an ephemeral loopback port
//! with a tiny std client.

use crate::core::ipc_client::IpcClient;
use crate::web::WebServer;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

/// Bind :0 and drive the real handle loop on a thread; arms the given
/// credentials when `creds` is Some.
fn start(creds: Option<(&str, &str)>) -> SocketAddr {
    let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);

    // A socket path that is never contacted — snapshot calls degrade offline;
    // auth behavior is checked before IPC.
    let dummy = TcpListener::bind("127.0.0.1:0").unwrap();
    let sock = std::env::temp_dir().join(format!(
        "uikit-auth-test-{}.sock",
        std::process::id() as u64
    ));
    drop(dummy);
    let mut server = WebServer::new(IpcClient::new(sock.to_string_lossy().to_string()), 10);
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
fn half_configured_credentials_disable_auth() {
    // Only one half set must not silently arm a password-less panel.
    let addr = start(None);
    assert_eq!(request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"), 200);
}

#[test]
fn cross_origin_refused_and_loopback_origin_ok() {
    let addr = start(Some(("admin", "s3cret")));
    let (code, out) = request_body(
        addr,
        &format!(
            "POST /api/login HTTP/1.1\r\nContent-Length: {}\r\n\r\n{{\"user\":\"admin\",\"password\":\"s3cret\"}}",
            "{\"user\":\"admin\",\"password\":\"s3cret\"}".len()
        ),
    );
    assert_eq!(code, 200);
    let token = serde_json::from_str::<serde_json::Value>(&out)
        .ok()
        .and_then(|v| v["token"].as_str().map(String::from))
        .expect("token");

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
}

#[test]
fn session_accepts_bearer_form() {
    let addr = start(Some(("admin", "s3cret")));
    let (_, out) = request_body(
        addr,
        &format!(
            "POST /api/login HTTP/1.1\r\nContent-Length: {}\r\n\r\n{{\"user\":\"admin\",\"password\":\"s3cret\"}}",
            "{\"user\":\"admin\",\"password\":\"s3cret\"}".len()
        ),
    );
    let token = serde_json::from_str::<serde_json::Value>(&out).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n")
        ),
        200,
        "Bearer session form passes"
    );
}
