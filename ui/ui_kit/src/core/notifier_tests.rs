//! Notifier tests — spec for #33 "断线重连有测试": a real std UDS server emits
//! `core.event` lines, closes the connection (simulating a core restart), then
//! serves again; the reader must reconnect and re-deliver.
//!
//! Every wait here is event-driven (#201): the test blocks on
//! `EventBus::wait_for`, which the reader wakes by publishing, so how a loaded
//! machine treats the reader thread costs these tests nothing. The one wall
//! clock left is `HANG_PROBE`, a hang detector rather than a latency budget —
//! see its definition. Nothing in this file polls a stopwatch to decide
//! PASS/FAIL, and each outcome is asserted separately, so a failure names the
//! one that happened instead of sharing one "not ingested (or stop hung)" line.

use crate::core::event_bus::{EventBus, Subscription};
use crate::core::notifier::NotificationReader;
use crate::core::types::CoreEvent;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ALERT: &str = r#"{"jsonrpc":"2.0","method":"core.event","params":{"kind":"RISK_ALERT","code":"KILL_SWITCH_ACTIVE","message":"kill-A"}}"#;

/// Reader retry interval these tests construct readers with. The read timeout
/// is `max(RETRY_MS, 200)` (see `NotificationReader`), which is the loop
/// checkpoint the stop contract is measured against.
const RETRY_MS: u64 = 50;

/// How long a wait may go unsatisfied before a test calls it a HANG.
///
/// This is a hang detector, NOT a latency budget: the waits below are released
/// by the reader publishing (or by `run()` returning), so a busy or starved
/// machine only delays PASS/FAIL — it cannot turn a working reader into a
/// failure. 30 s is ~150x the reader's own 200 ms loop checkpoint, so it can
/// only fire on a reader that is genuinely stuck (or never saw the bytes). On
/// the failure path the wait ends here and the message says "hang"; on the
/// happy path this value is never consulted.
const HANG_PROBE: Duration = Duration::from_secs(30);

/// Bind a fresh socket for one test, clearing a stale file from an earlier run
/// (a reused PID with a leftover path is a trap, not a fixture).
fn bind_socket(tag: &str) -> (PathBuf, UnixListener) {
    let sock =
        std::env::temp_dir().join(format!("uikit-notifier-{tag}-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind");
    (sock, listener)
}

fn cleanup(sock: &Path) {
    let _ = std::fs::remove_file(sock);
}

/// A reader under test, on its own thread, with the observables a test needs in
/// order to avoid a stopwatch: the stop flag, the count of lines it handled,
/// and a signal for "`run()` has returned".
///
/// `run()`'s exit travels through a channel with a bound instead of
/// `JoinHandle::join`, because joining a reader that never returns wedges the
/// whole test binary. That is not hypothetical: the old "watchdog" in
/// `stop_exits_while_silently_connected` joined a thread that joined the reader,
/// so a regression blocked the suite forever instead of failing, and the one
/// message in the flaky test ("not ingested (or stop hung)") could not say
/// which of the two had happened.
struct ReaderFixture {
    stop: Arc<AtomicBool>,
    lines: Arc<AtomicUsize>,
    done: Receiver<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ReaderFixture {
    fn start(reader: NotificationReader) -> Self {
        let stop = reader.stop_handle();
        let lines = reader.lines_handle();
        let (done_tx, done) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            reader.run();
            let _ = done_tx.send(());
        });
        Self {
            stop,
            lines,
            done,
            handle: Some(handle),
        }
    }

    /// Complete lines the reader has handed to the bus so far.
    fn lines(&self) -> usize {
        self.lines.load(Ordering::SeqCst)
    }

    /// Is the reader thread still running? Part of a hang report: "the reader
    /// is alive but silent" and "the reader died" are different bugs.
    fn running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Set the stop flag and wait, bounded, for `run()` to return. `true` means
    /// the loop left; `false` means `HANG_PROBE` elapsed first, which the caller
    /// must REPORT — never inherit, or a stop-path regression hangs the suite.
    /// A thread that did not return is left running (the harness exits the
    /// process at the end of the run; a wedged reader must not take the report
    /// down with it).
    fn stop_and_wait(&mut self) -> bool {
        self.stop.store(false, Ordering::SeqCst);
        let exited = self.done.recv_timeout(HANG_PROBE).is_ok();
        if exited {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
        exited
    }
}

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

/// Wait for the ALERT naming `message`. Event-driven (see `EventBus::wait_for`)
/// — `HANG_PROBE` bounds only a reader that publishes nothing at all. `sub`
/// must have been created BEFORE the reader's life under test, because
/// `subscribe()` is positioned at the current head and a fresh subscriber per
/// wait would never see previously published events.
fn wait_alert(sub: &mut Subscription, bus: &EventBus, message: &str) -> bool {
    bus.wait_for(
        sub,
        HANG_PROBE,
        |ev| matches!(ev, CoreEvent::RiskAlert { message: m, .. } if m == message),
    )
    .is_some()
}

/// Wait for a `core.event` ERROR whose payload is exactly `payload`.
///
/// The payload is matched, not merely the variant: the READER ITSELF publishes
/// `CoreEvent::Error` when a stream closes ("event stream closed"), so a
/// predicate as loose as `matches!(CoreEvent::Error { .. })` can be satisfied
/// by the reader's own transition message while the line under test was never
/// decoded — a green that means nothing.
fn wait_error(sub: &mut Subscription, bus: &EventBus, payload: &str) -> bool {
    bus.wait_for(
        sub,
        HANG_PROBE,
        |ev| matches!(ev, CoreEvent::Error { error } if error.as_str() == Some(payload)),
    )
    .is_some()
}

#[test]
fn reader_reconnects_and_redelivers() {
    let (sock, listener) = bind_socket("test");

    let bus = EventBus::new(64);
    // Subscribe BEFORE the reader starts so both lives are observable.
    let mut sub = bus.subscribe();
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), RETRY_MS);
    let mut fixture = ReaderFixture::start(reader);

    let connections = Arc::new(AtomicUsize::new(0));
    let serving = Arc::new(AtomicBool::new(true));
    let server = serve_alert_rounds(listener, connections.clone(), serving.clone());

    // Life 1: the reader connects and ingests an ALERT.
    let got1 = wait_alert(&mut sub, &bus, "kill-A");
    // Life 2: the server closed the session, so a second ALERT can only arrive
    // if the reader noticed EOF and reconnected.
    let got2 = wait_alert(&mut sub, &bus, "kill-A");

    let exited = fixture.stop_and_wait();
    serving.store(false, Ordering::SeqCst);
    let _ = server.join();
    cleanup(&sock);

    assert!(got1, "life-1 ALERT never reached the bus");
    assert!(got2, "life-2 redelivery failed — reconnect broken");
    assert!(
        connections.load(Ordering::SeqCst) >= 2,
        "reader never reconnected after the session closed"
    );
    assert!(
        exited,
        "hang: run() did not return within {HANG_PROBE:?} of stop() (lines read: {}, \
         reader thread still running: {})",
        fixture.lines(),
        fixture.running()
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
    let (sock, listener) = bind_socket("split");
    let bus = EventBus::new(8);
    let mut sub = bus.subscribe();
    // retry_ms 50 => 200 ms read timeout, so a 700 ms gap is a timeout landing
    // squarely inside the line. That gap is FIXTURE PACING, not a pass
    // criterion: it forces the split this test is about, while the assertion
    // below still waits on the bus signal, so a starved reader cannot fail it.
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), RETRY_MS);
    let mut fixture = ReaderFixture::start(reader);

    let mut s: UnixStream = listener.accept().expect("accept").0;
    s.write_all(ALERT.as_bytes()).expect("write first half");
    s.flush().ok();
    std::thread::sleep(Duration::from_millis(700));
    s.write_all(b"\n").expect("write second half");
    s.flush().ok();

    let got = wait_alert(&mut sub, &bus, "kill-A");
    let exited = fixture.stop_and_wait();
    drop(listener);
    cleanup(&sock);
    assert!(got, "a line split across a read timeout was dropped");
    assert!(
        exited,
        "hang: run() did not return within {HANG_PROBE:?} of stop() (lines read: {}, \
         reader thread still running: {})",
        fixture.lines(),
        fixture.running()
    );
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
    let (sock, listener) = bind_socket("overlong");
    let reader = NotificationReader::new(
        sock.to_string_lossy().to_string(),
        EventBus::new(8),
        RETRY_MS,
    );
    let mut fixture = ReaderFixture::start(reader);

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

    let exited = fixture.stop_and_wait();
    drop(listener);
    cleanup(&sock);
    assert!(
        refused,
        "reader kept consuming a newline-less stream past the bound (sent {sent} bytes)"
    );
    assert!(
        exited,
        "hang: run() did not return within {HANG_PROBE:?} of stop() (lines read: {})",
        fixture.lines()
    );
}

/// The reader must ignore envelopes that are not `core.event`, ingest the ones
/// that are, and still stop on command.
///
/// The ERROR is the SECOND line on purpose: a reader that died on (or threw
/// away its buffer at) the unknown `ping` method would never deliver it, so the
/// wait below pins the ignore semantics too.
///
/// Both halves are observed on a SIGNAL, not on a stopwatch (#201). The wait is
/// released by `EventBus::wait_for` the moment the reader publishes, so a
/// loaded machine costs this test nothing; `HANG_PROBE` bounds only a reader
/// that never publishes. The two outcomes are asserted separately, so a failure
/// names the one that happened — the old shared message, "core.event ERROR line
/// was not ingested (or stop hung)", could not tell a reader that missed the
/// line from one that never returned, and the 2 s -> 10 s budget bump only
/// moved the threshold the race ran against.
#[test]
fn reader_ignores_non_core_event_lines_and_stops() {
    let (sock, listener) = bind_socket("ign");
    let bus = EventBus::new(8);
    let mut sub = bus.subscribe();
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), RETRY_MS);
    let mut fixture = ReaderFixture::start(reader);

    {
        let mut s: UnixStream = listener.accept().expect("accept").0;
        s.write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"method\":\"core.event\",\"params\":{\"kind\":\"ERROR\",\"error\":\"boom\"}}\n",
        )
        .ok();
        s.flush().ok();
        s.shutdown(std::net::Shutdown::Both).ok();
    }

    // 1. Ingestion: blocks until THIS line's event is published.
    let ingested = wait_error(&mut sub, &bus, "boom");
    // 2. Stop: an independent claim, with its own budget and its own message.
    let exited = fixture.stop_and_wait();
    // Snapshot the reader's state before the thread is joined: "still running"
    // is exactly what a hang report has to say, and a join hides it.
    let running = fixture.running();
    let lines = fixture.lines();
    let published = bus.seq();
    drop(listener);
    cleanup(&sock);

    assert!(
        ingested,
        "NOT INGESTED: the core.event ERROR line never reached the bus within the {HANG_PROBE:?} \
         hang probe — lines read: {lines}, events published: {published}, reader thread still \
         running: {running}, run() returned on stop: {exited}"
    );
    assert!(
        exited,
        "HANG: run() did not return within {HANG_PROBE:?} of stop() being set — the stop path is \
         stuck, the line under test is not at fault (lines read: {lines}, events published: \
         {published})"
    );
}

/// A long-lived silent connection must not hang `stop()` indefinitely:
/// the flag flip is the contract; run() ends at its next loop checkpoint
/// (read timeout keeps the loop interruptible). The wait for `run()` to return
/// is bounded, so a regression is REPORTED here instead of wedging the suite.
#[test]
fn stop_exits_while_silently_connected() {
    let (sock, listener) = bind_socket("quiet");
    let reader = NotificationReader::new(
        sock.to_string_lossy().to_string(),
        EventBus::new(4),
        RETRY_MS,
    );
    let mut fixture = ReaderFixture::start(reader);
    let (_conn, _) = listener.accept().expect("accept"); // stays open, silent
    std::thread::sleep(Duration::from_millis(100));
    let exited = fixture.stop_and_wait();
    let running = fixture.running();
    drop(listener);
    cleanup(&sock);
    assert!(
        exited,
        "HANG: run() did not return within {HANG_PROBE:?} of stop() while a silent peer held the \
         connection open (reader thread still running: {running})"
    );
}
