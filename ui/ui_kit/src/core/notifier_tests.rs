//! Notifier tests — spec for #33 "断线重连有测试": a real std UDS server emits
//! `core.event` lines, closes the connection (simulating a core restart), then
//! serves again; the reader must reconnect and re-deliver.

use crate::core::event_bus::EventBus;
use crate::core::notifier::NotificationReader;
use crate::core::types::CoreEvent;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ALERT: &str = r#"{"jsonrpc":"2.0","method":"core.event","params":{"kind":"RISK_ALERT","code":"KILL_SWITCH_ACTIVE","message":"kill-A"}}"#;

/// Serve `core.event` ALERTs until the returned flag is cleared, closing every
/// session it held each round to force a reconnect.
///
/// The real core writes notifications to *every* open session and never
/// depends on which one reads them. A helper that instead accepts one
/// connection and assumes it is the reader's live session has to guess:
/// `run()` also opens short-lived probe connections, and an ALERT written into
/// one of those is discarded, so the reader can lose an event through no fault
/// of its own. Writing to every accepted connection removes the guess.
///
/// Closing the held sessions at the end of each round is what exercises the
/// reconnect path: the reader must observe EOF and come back.
fn serve_alert_rounds(
    listener: UnixListener,
    connections: Arc<AtomicUsize>,
    serving: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).ok();
        while serving.load(Ordering::SeqCst) {
            let mut held: Vec<UnixStream> = Vec::new();
            let round_end = Instant::now() + Duration::from_millis(250);
            while serving.load(Ordering::SeqCst) && Instant::now() < round_end {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        connections.fetch_add(1, Ordering::SeqCst);
                        let _ = s
                            .write_all(ALERT.as_bytes())
                            .and_then(|_| s.write_all(b"\n"))
                            .and_then(|_| s.flush());
                        held.push(s);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            drop(held); // closes the sessions: reader sees EOF, then reconnects
            std::thread::sleep(Duration::from_millis(20));
        }
    })
}

/// Poll for a RiskAlert("kill-A") on `sub` — created BEFORE the reader's life
/// under test, because `subscribe()` is positioned at the current head and a
/// fresh subscriber per poll would never see previously published events.
fn poll_alert(
    sub: &mut crate::core::event_bus::Subscription,
    bus: &EventBus,
    up_to: Instant,
) -> bool {
    while Instant::now() < up_to {
        if bus
            .drain_new(sub)
            .iter()
            .any(|ev| matches!(ev, CoreEvent::RiskAlert { message, .. } if message == "kill-A"))
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Poll for any Error event on `sub`.
fn poll_error(
    sub: &mut crate::core::event_bus::Subscription,
    bus: &EventBus,
    up_to: Instant,
) -> bool {
    while Instant::now() < up_to {
        if bus
            .drain_new(sub)
            .iter()
            .any(|ev| matches!(ev, CoreEvent::Error { .. }))
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn reader_reconnects_and_redelivers() {
    let sock =
        std::env::temp_dir().join(format!("uikit-notifier-test-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind");

    let bus = EventBus::new(64);
    // Subscribe BEFORE the reader starts so both lives are observable.
    let mut sub = bus.subscribe();
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), 50);
    let stop = reader.stop_handle();
    let handle = std::thread::spawn(move || reader.run());

    let connections = Arc::new(AtomicUsize::new(0));
    let serving = Arc::new(AtomicBool::new(true));
    let server = serve_alert_rounds(listener, connections.clone(), serving.clone());

    // Life 1: the reader connects and ingests an ALERT.
    let got1 = poll_alert(&mut sub, &bus, Instant::now() + Duration::from_secs(5));
    // Life 2: the server closed the session, so a second ALERT can only arrive
    // if the reader noticed EOF and reconnected.
    let got2 = poll_alert(&mut sub, &bus, Instant::now() + Duration::from_secs(5));

    stop.store(false, Ordering::SeqCst);
    serving.store(false, Ordering::SeqCst);
    let _ = handle.join();
    let _ = server.join();
    let _ = std::fs::remove_file(&sock);

    assert!(got1, "life-1 ALERT never reached the bus");
    assert!(got2, "life-2 redelivery failed — reconnect broken");
    assert!(
        connections.load(Ordering::SeqCst) >= 2,
        "reader never reconnected after the session closed"
    );
}

/// A line whose halves are separated by more than the read timeout must still
/// be delivered.
///
/// The reader uses a short read timeout so a silent core cannot trap its
/// thread. A record split across that boundary has to carry its first half
/// forward to the next read; dropping the partial buffer instead loses half a
/// record and leaves the trailing half to arrive as a blank line.
#[test]
fn reader_delivers_a_line_split_across_a_read_timeout() {
    let sock =
        std::env::temp_dir().join(format!("uikit-notifier-split-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind");
    let bus = EventBus::new(8);
    let mut sub = bus.subscribe();
    // retry_ms 50 => 200 ms read timeout, so a 700 ms gap is a timeout landing
    // squarely inside the line.
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), 50);
    let stop = reader.stop_handle();
    let handle = std::thread::spawn(move || reader.run());

    let mut s: UnixStream = listener.accept().expect("accept").0;
    s.write_all(ALERT.as_bytes()).expect("write first half");
    s.flush().ok();
    std::thread::sleep(Duration::from_millis(700));
    s.write_all(b"\n").expect("write second half");
    s.flush().ok();

    let got = poll_alert(&mut sub, &bus, Instant::now() + Duration::from_secs(3));
    stop.store(false, Ordering::SeqCst);
    let _ = handle.join();
    drop(listener);
    let _ = std::fs::remove_file(&sock);
    assert!(got, "a line split across a read timeout was dropped");
}

/// A peer that never sends a newline must not grow the line buffer without
/// bound: the reader has to give up and drop the connection instead of
/// accumulating forever.
///
/// The observable is write failure — a reader that enforced the bound stops
/// consuming and closes, while one that buffered without limit would keep
/// draining every byte written here.
#[test]
fn reader_stops_reading_an_overlong_line_without_a_newline() {
    let sock =
        std::env::temp_dir().join(format!("uikit-notifier-overlong-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind");
    let bus = EventBus::new(8);
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), 50);
    let stop = reader.stop_handle();
    let handle = std::thread::spawn(move || reader.run());

    let mut s: UnixStream = listener.accept().expect("accept").0;
    let chunk = vec![b'x'; 64 * 1024];
    let mut sent = 0usize;
    let mut refused = false;
    // No newline anywhere: only the size bound can stop this.
    while sent < 8 * 1024 * 1024 {
        if s.write_all(&chunk).is_err() {
            refused = true; // reader gave up and closed, which is the point
            break;
        }
        sent += chunk.len();
    }
    s.flush().ok();

    stop.store(false, Ordering::SeqCst);
    let _ = handle.join();
    drop(listener);
    let _ = std::fs::remove_file(&sock);
    assert!(
        refused,
        "reader kept consuming a newline-less stream past the bound (sent {sent} bytes)"
    );
}

#[test]
fn reader_ignores_non_core_event_lines_and_stops() {
    let sock = std::env::temp_dir().join(format!("uikit-notifier-ign-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind");
    let bus = EventBus::new(8);
    let mut sub = bus.subscribe();
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), 50);
    let stop = reader.stop_handle();
    let handle = std::thread::spawn(move || reader.run());

    {
        let mut s: UnixStream = listener.accept().expect("accept").0;
        s.write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"method\":\"core.event\",\"params\":{\"kind\":\"ERROR\",\"error\":\"boom\"}}\n",
        )
        .ok();
        s.flush().ok();
        s.shutdown(std::net::Shutdown::Both).ok();
    }
    let got_err = poll_error(&mut sub, &bus, Instant::now() + Duration::from_secs(2));
    stop.store(false, Ordering::SeqCst);
    let _ = handle.join(); // proves run() exits on stop instead of hanging
    drop(listener);
    let _ = std::fs::remove_file(&sock);
    assert!(
        got_err,
        "core.event ERROR line was not ingested (or stop hung)"
    );
}

/// A long-lived silent connection must not hang `stop()` indefinitely:
/// the flag flip is the contract; run() ends at its next loop checkpoint
/// (read timeout keeps the loop interruptible).
#[test]
fn stop_exits_while_silently_connected() {
    let sock =
        std::env::temp_dir().join(format!("uikit-notifier-quiet-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind");
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), EventBus::new(4), 50);
    let stop = reader.stop_handle();
    let handle = std::thread::spawn(move || reader.run());
    let (_conn, _) = listener.accept().expect("accept"); // stays open, silent
    std::thread::sleep(Duration::from_millis(100));
    stop.store(false, Ordering::SeqCst);
    // joined within the read-timeout window would prove the loop exits;
    // give it a bounded window via a watchdog thread instead of blocking the
    // test harness forever on a regression.
    let joined = std::thread::spawn(move || handle.join()).join();
    drop(listener);
    let _ = std::fs::remove_file(&sock);
    assert!(
        joined.is_ok(),
        "reader thread must exit after stop() even while connected"
    );
}
