//! #838: server-sent events — one push channel for the event-shaped readings
//! the 15s polling loop delivers late (banners, order terminal states, crash
//! alerts).
//!
//! Shape (deliberately minimal, no new dependencies):
//!
//!   GET /api/events   → `text/event-stream`, authorized like every /api route
//!
//! The stream is a **change ticker, not a firehose**: a 1s tick takes a fresh
//! snapshot through the SAME shared `IpcClient` the polling routes use,
//! serializes it with the same `render_json` the `/api/snapshot` route serves,
//! and pushes an `event: snapshot` frame only when the document actually
//! changed (hash-gated). There is no kernel event bus to hook — the gateway
//! polls the kernel either way — so "real-time" here means the panel learns of
//! a change within one tick instead of within one poll interval, and the
//! frame carries everything the regular snapshot carries (the frontend feeds
//! it straight into the store it already has).
//!
//! Lifecycle: one thread per connected stream (the accept loop is serial — a
//! stream that held the loop would blind every other client). The thread
//! exits when the peer disconnects (first failed write, or the read half
//! EOFs). A `: ping` comment every 15s keeps intermediaries from reaping the
//! idle connection.
//!
//! Frontend contract: `EventSource` cannot send custom headers, and #460
//! removed the `?token=` query channel (it leaks into logs/Referer), so the
//! panel consumes this with a streaming `fetch` carrying the same
//! `X-Auth-Token` every other /api call uses — same gate, same session, no
//! new credential channel. On stream death the panel falls back to the
//! existing polling loop (polling is never removed — a dead push stream must
//! never blind the panel).
use super::*;

/// Serve one SSE connection. Called from the route table after the auth gate
/// has passed; writes the response head and hands the stream to the ticker
/// thread — the accept loop is free again immediately.
pub(super) fn serve_events(server: &WebServer, stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
    let _ = stream.set_nodelay(true);
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut out = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    // Response head. `Connection: close` (this socket serves exactly this
    // stream); no Content-Length (the body is unbounded by design).
    if out
        .write_all(
            b"HTTP/1.1 200 OK\r\n\
              Content-Type: text/event-stream\r\n\
              Cache-Control: no-store\r\n\
              Connection: close\r\n\
              \r\n",
        )
        .is_err()
    {
        return;
    }
    let _ = out.flush();

    // The ticker holds the same shared handles the routes hold: the snapshot
    // client and (gateway mode) the dispatcher, so its render is byte-identical
    // to what /api/snapshot serves this instant.
    let snapshot_src = server.snapshot_src.clone();
    let dispatcher = server.dispatcher.clone();
    let trade_limit = server.trade_limit;
    let ticker_closed = closed.clone();
    std::thread::spawn(move || {
        let _ = out.write_all(b"retry: 3000\r\n\r\n");
        let _ = out.flush();
        let mut last_hash: u64 = 0;
        let mut last_ping = std::time::Instant::now();
        loop {
            if ticker_closed.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let doc = {
                let snap = match snapshot_src.lock() {
                    Ok(mut c) => c.snapshot(trade_limit),
                    Err(_) => UiSnapshot::default(),
                };
                let lifecycle = dispatcher.as_ref().and_then(|d| {
                    let mut d = d.lock().ok()?;
                    d.pump();
                    let health = d.health();
                    Some(LifecycleView {
                        enabled: d.lifecycle_enabled(),
                        managed: health.managed,
                        pid: health.pid,
                        socket: d.socket_path().to_string(),
                        restarts: health.restarts,
                        restart_given_up: health.restart_given_up,
                        last_exit: health.last_exit.as_ref().map(LifecycleExit::from),
                        give_up: health.alert.as_ref().map(LifecycleGiveUp::from),
                    })
                });
                render_json(&snap, lifecycle.as_ref())
            };
            let hash = fnv1a(&doc);
            if hash != last_hash {
                last_hash = hash;
                let frame = format!("event: snapshot\r\ndata: {doc}\r\n\r\n");
                if out.write_all(frame.as_bytes()).is_err() || out.flush().is_err() {
                    // Peer gone (page closed, tab asleep): stop ticking. The
                    // next reconnect starts a fresh stream; polling covers
                    // the gap meanwhile.
                    break;
                }
            }
            if last_ping.elapsed() >= std::time::Duration::from_secs(15) {
                last_ping = std::time::Instant::now();
                if out.write_all(b": ping\r\n\r\n").is_err() || out.flush().is_err() {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        ticker_closed.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    // The read half detects the disconnect sooner than the next write would:
    // when the browser goes away the read EOFs/errors and we mark the ticker
    // closed. No data is ever expected on this socket — a fetch-based reader
    // and EventSource both only ever read.
    let mut reader = BufReader::new(stream);
    let mut sink = String::new();
    loop {
        match reader.read_line(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => sink.clear(),
        }
    }
    closed.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// FNV-1a: change detection for the ticker. Two lines, no dependency, good
/// enough to gate a push (a hash collision just costs one redundant frame).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_is_order_sensitive_and_stable() {
        let a = fnv1a("{\"connected\":true}");
        let b = fnv1a("{\"connected\":false}");
        let a2 = fnv1a("{\"connected\":true}");
        assert_ne!(a, b);
        assert_eq!(a, a2);
    }
}
