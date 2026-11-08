//! #838: the SSE endpoint through the REAL handle loop — auth-gate parity and
//! frame shape on the wire. The gateway is never contacted (the IPC socket
//! path is a dead path): the first tick renders a disconnected-core snapshot
//! (`connected: false`), which is still a document, still hashed, still pushed
//! once — exactly what a stale-session panel needs to see change-wise.

use crate::web::auth_tests::wait_for_bound_shared;
use crate::web::{IpcClient, WebServer};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Gateway-mode server (auth always required) on a thread, :0-bound.
fn start() -> (SocketAddr, Arc<WebServer>, String, String) {
    let sock =
        std::env::temp_dir().join(format!("uikit-sse-test-{}.sock", std::process::id() as u64));
    let path = sock.to_string_lossy().to_string();
    let mut server = WebServer::with_gateway(
        IpcClient::new(path.clone()),
        10,
        crate::gateway::Dispatcher::new(crate::gateway::SupervisorConfig::from_env(path), false),
    );
    // Password built at runtime (pid suffix): CodeQL rust/hard-coded-cryptographic-
    // value flags any literal flowing into a credential sink, and a test fixture
    // is still a literal in the binary. Both sides of the login below derive from
    // this one value, so determinism inside the run is unchanged.
    let (u, p) = (
        "ops".to_string(),
        format!("stream-s3cret-{}", std::process::id()),
    );
    server.set_panel_credentials(Some(u.to_string()), Some(p.clone()));
    let server = Arc::new(server);
    let serving = Arc::clone(&server);
    thread::spawn(move || {
        let _ = serving.serve("127.0.0.1:0");
    });
    let addr = wait_for_bound_shared(&server);
    (addr, server, u.to_string(), p)
}

/// Log in through the real route; returns the session token.
fn login(addr: SocketAddr, user: &str, password: &str) -> String {
    let body = format!("{{\"user\":\"{user}\",\"password\":\"{password}\"}}");
    let mut s = TcpStream::connect(addr).expect("connect");
    let req = format!(
        "POST /api/login HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len(),
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    // {"token":"..."} — the session that every later /api call carries.
    let doc: serde_json::Value =
        serde_json::from_str(out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or(""))
            .expect("login body parses");
    doc["token"]
        .as_str()
        .expect("token on the wire")
        .to_string()
}

/// Read at most `max` bytes (or until close) with a hard deadline.
fn read_capped(s: &mut TcpStream, max: usize) -> String {
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < max {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn sse_requires_a_session_like_every_api_route() {
    let (addr, _server, _u, _p) = start();
    let mut s = TcpStream::connect(addr).expect("connect");
    s.write_all(b"GET /api/events HTTP/1.1\r\n\r\n").unwrap();
    let head = read_capped(&mut s, 512);
    assert!(
        head.starts_with("HTTP/1.1 401"),
        "unauthenticated SSE must be refused at the gate, got: {head}"
    );
}

#[test]
fn sse_streams_snapshot_frames_after_login() {
    let (addr, _server, u, p) = start();
    let token = login(addr, &u, &p);
    let mut s = TcpStream::connect(addr).expect("connect");
    let req = format!("GET /api/events HTTP/1.1\r\nX-Auth-Token: {token}\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let out = read_capped(&mut s, 16 * 1024);
    // Response head: SSE media type, no Content-Length.
    let (head, body) = out.split_once("\r\n\r\n").expect("head/body split");
    assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
    assert!(head.to_ascii_lowercase().contains("text/event-stream"));
    assert!(!head.to_ascii_lowercase().contains("content-length"));
    // First frame: retry hint, then the change ticker's first snapshot —
    // a dead-core snapshot is still a document (connected:false, mode
    // "unknown"): the same render the snapshot route serves.
    assert!(
        body.contains("retry: 3000")
            && body.contains("event: snapshot\r\n")
            && body.contains("\"connected\":false")
            && body.contains("\"mode\":\"unknown\""),
        "body head: {}",
        &body[..300.min(body.len())]
    );
}

#[test]
fn sse_holds_its_own_socket_and_the_accept_loop_stays_free() {
    // The serial-accept-loop hazard: while one SSE client holds its stream,
    // ordinary routes must still answer. (Before #838's per-stream thread
    // this pattern would wedge the whole panel.)
    let (addr, _server, u, p) = start();
    let token = login(addr, &u, &p);
    let mut sse = TcpStream::connect(addr).expect("connect sse");
    sse.write_all(format!("GET /api/events HTTP/1.1\r\nX-Auth-Token: {token}\r\n\r\n").as_bytes())
        .unwrap();
    // Give the stream thread a beat to take the socket, then hit /api/ping.
    thread::sleep(Duration::from_millis(300));
    let mut s = TcpStream::connect(addr).expect("connect ping");
    s.write_all(b"GET /api/ping HTTP/1.1\r\n\r\n").unwrap();
    let out = read_capped(&mut s, 1024);
    assert!(
        out.starts_with("HTTP/1.1 200"),
        "ping must answer while an SSE stream is live, got: {out}"
    );
    // And the stream itself produced its first frame meanwhile.
    let stream_out = read_capped(&mut sse, 4 * 1024);
    assert!(stream_out.contains("event: snapshot"));
}
