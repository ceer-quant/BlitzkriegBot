//! E6-a auth acceptance — spec of #35: 无 token 拒绝、token 错误拒绝、
//! token 正确通过、跨域 Origin 拒绝。Server is driven over a real socket on
//! an ephemeral loopback port with a tiny std client.

use crate::core::ipc_client::IpcClient;
use crate::web::WebServer;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

/// Bind :0 and drive the real handle loop on a thread; returns addr + token.
fn start(token: bool) -> (SocketAddr, Option<String>) {
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
    let tok = if token {
        Some(server.generate_auth_token())
    } else {
        None
    };
    thread::spawn(move || {
        let _ = server.serve(&addr.to_string());
    });
    thread::sleep(Duration::from_millis(150));
    (addr, tok)
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

#[test]
fn no_token_is_rejected() {
    let (addr, _) = start(true);
    assert_eq!(request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"), 401);
}

#[test]
fn wrong_token_is_rejected() {
    let (addr, _) = start(true);
    assert_eq!(
        request(addr, "GET /api/snapshot?token=NOPE HTTP/1.1\r\n\r\n"),
        401
    );
}

#[test]
fn good_token_passes_via_query_and_header() {
    let (addr, tok) = start(true);
    let tok = tok.expect("token generated");
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot?token={tok} HTTP/1.1\r\n\r\n")
        ),
        200,
        "query token must pass"
    );
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot HTTP/1.1\r\nX-Auth-Token: {tok}\r\n\r\n")
        ),
        200,
        "header token must pass"
    );
}

#[test]
fn no_auth_configured_passes_loopback() {
    let (addr, _) = start(false);
    assert_eq!(request(addr, "GET /api/snapshot HTTP/1.1\r\n\r\n"), 200);
}

#[test]
fn cross_origin_refused_and_loopback_origin_ok() {
    let (addr, tok) = start(true);
    let tok = tok.unwrap();
    assert_eq!(
        request(
            addr,
            &format!(
                "GET /api/snapshot?token={tok} HTTP/1.1\r\nOrigin: http://evil.example\r\n\r\n"
            )
        ),
        403,
        "foreign Origin must 403"
    );
    assert_eq!(
        request(
            addr,
            &format!("GET /api/snapshot?token={tok} HTTP/1.1\r\nOrigin: http://127.0.0.1\r\n\r\n")
        ),
        200,
        "loopback origin passes"
    );
}
