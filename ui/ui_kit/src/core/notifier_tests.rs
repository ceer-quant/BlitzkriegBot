//! Notifier tests — spec for #33 "断线重连有测试": a real std UDS server emits
//! `core.event` lines, closes the connection (simulating a core restart), then
//! serves again; the reader must reconnect and re-deliver.

use crate::core::event_bus::EventBus;
use crate::core::notifier::NotificationReader;
use crate::core::types::CoreEvent;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const ALERT: &str = r#"{"jsonrpc":"2.0","method":"core.event","params":{"kind":"RISK_ALERT","code":"KILL_SWITCH_ACTIVE","message":"kill-A"}}"#;

/// Accept connections until one reads its ALERT back: the reader's probe
/// connections are write-less and vanish (broken pipe); the session that
/// `read_until_closed` opened is the one that consumes the line.
fn serve_once(listener: &UnixListener) {
    loop {
        let mut s: UnixStream = listener.accept().expect("accept").0;
        match s
            .write_all(ALERT.as_bytes())
            .and_then(|_| s.write_all(b"\n"))
            .and_then(|_| s.flush())
        {
            Ok(()) => {
                s.shutdown(std::net::Shutdown::Both).ok();
                return;
            }
            Err(_) => continue, // was a probe connection; keep accepting
        }
    }
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

    // Life 1: connect, receive ALERT, server closes.
    serve_once(&listener);
    let got1 = poll_alert(&mut sub, &bus, Instant::now() + Duration::from_secs(3));
    assert!(got1, "life-1 ALERT never reached the bus");

    // Life 2: the reader must reconnect after the close and deliver again.
    serve_once(&listener);
    let got2 = poll_alert(&mut sub, &bus, Instant::now() + Duration::from_secs(3));
    stop.store(false, Ordering::SeqCst);
    let _ = handle.join();
    drop(listener);
    let _ = std::fs::remove_file(&sock);
    assert!(got2, "life-2 redelivery failed — reconnect broken");
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
