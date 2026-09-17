//! Spec for the `connected` flag: it means "a core answered", not "we hold a
//! socket object".
//!
//! The defect this pins was found by the E12(c) gateway gate, not by reasoning:
//! after the core was SIGKILLed, one snapshot still reported `connected: true`
//! with empty arrays, and the panel drew that as a running engine with no
//! positions. The cause is that `connect()` short-circuits while a handle
//! exists — a handle outliving its peer is invisible until a *call* is made, so
//! the flag has to come from the call, not from the handle.

use crate::core::ipc_client::IpcClient;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

/// A one-shot UDS server: answers `replies` requests then closes the session,
/// which is what a killed core looks like from the client's side.
fn serve_then_close(sock: PathBuf, replies: usize) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(&sock).expect("bind test socket");
    std::thread::spawn(move || {
        // One persistent session is what the client keeps, so the loop below
        // serves each request on the SAME stream until the budget runs out.
        if let Ok((stream, _)) = listener.accept() {
            let mut writer = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            for _ in 0..replies {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let id: serde_json::Value = serde_json::from_str(line.trim())
                    .ok()
                    .and_then(|v: serde_json::Value| v.get("id").cloned())
                    .unwrap_or(serde_json::json!(1));
                let reply = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "version": "0.1.0", "mode": "dry" },
                });
                let mut out = reply.to_string();
                out.push('\n');
                if writer
                    .write_all(out.as_bytes())
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            // Drop the session without ceremony: this is the crash, and it must
            // be the *next call* that notices, not an earlier one.
        }
    })
}

fn unique_sock(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bk-ipc-{}-{}.sock", tag, std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn a_snapshot_reports_connected_only_while_the_core_answers() {
    let sock = unique_sock("snap");
    // Three replies: the first snapshot needs `core.ready` plus one state call
    // to reach the point where `connected` is set; the second snapshot needs a
    // `ready` attempt that hits a closed session.
    let server = serve_then_close(sock.clone(), 3);

    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    let first = client.snapshot(0);
    assert!(
        first.connected,
        "a core that answers must be reported as connected (lastError={:?})",
        first.last_error
    );
    assert!(
        first.ready.is_some(),
        "the handshake payload must ride along"
    );

    // Let the server finish its budget and drop the session.
    std::thread::sleep(std::time::Duration::from_millis(200));

    let second = client.snapshot(0);
    assert!(
        !second.connected,
        "a cached handle whose core is gone must NOT be reported as connected — \
         this is the exact false positive the E12(c) gate caught"
    );
    assert!(
        second.last_error.is_some(),
        "an unreachable core must come with a reason the panel can show"
    );
    assert!(
        second.balance.is_none() && second.stats.is_none(),
        "and no half-filled state may be presented as live"
    );

    server.join().ok();
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn connecting_to_a_missing_socket_is_not_connected() {
    // The other direction, and the one a read-only panel hits: nothing is
    // listening. `connect()` fails outright, so this must stay false.
    let sock = unique_sock("absent");
    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    let s = client.snapshot(0);
    assert!(!s.connected);
    assert!(s.last_error.is_some());
}

#[test]
fn a_dead_session_is_dropped_so_the_next_call_reconnects() {
    // Recovery depends on this: once a call fails, the handle must be gone, or
    // a restarted core could never be reached through the same client (the flag
    // would recover but every subsequent call would keep writing to the corpse).
    let sock = unique_sock("reconnect");
    let _ = std::fs::remove_file(&sock);
    {
        let listener = UnixListener::bind(&sock).expect("bind");
        // Hold and close immediately: the client connects, then loses the peer.
        std::thread::spawn(move || {
            if let Ok((_s, _)) = listener.accept() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
    }

    let mut client = IpcClient::new(sock.to_string_lossy().to_string());
    assert!(client.connect().is_ok(), "the first connect succeeds");
    assert!(client.is_connected());
    // This call meets the closed peer.
    let _ = client.ready();
    assert!(
        !client.is_connected(),
        "a failed call must drop the handle, or a restarted core is unreachable"
    );
    let _ = std::fs::remove_file(&sock);
}
