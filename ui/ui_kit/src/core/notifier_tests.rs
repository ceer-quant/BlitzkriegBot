//! Notifier tests — spec for #33 "断线重连有测试": a real std UDS server emits
//! `core.event` lines, closes the connection (simulating a core restart), then
//! serves again; the reader must reconnect and re-deliver.
//!
//! Two rules keep a red from meaning something other than a regression (#201):
//!
//! 1. Every wait is event-driven. The test blocks on `EventBus::wait_for`, which
//!    the reader wakes by publishing, so how a loaded machine treats the reader
//!    thread costs these tests nothing. The one wall clock left is `HANG_PROBE`,
//!    a hang detector rather than a latency budget — see its definition. Nothing
//!    in this file polls a stopwatch to decide PASS/FAIL, and each outcome is
//!    asserted separately, so a failure names the one that happened instead of
//!    sharing one "not ingested (or stop hung)" line.
//! 2. No fixture guesses which connection the reader reads. The real core writes
//!    notifications to EVERY open session, and the reader opens short-lived PROBE
//!    connections of its own; bytes written into a session nobody reads are gone
//!    for good, and no wait budget can recover them (see `serve_lines_rounds`).
//!    Every server helper here hands its bytes to every accepted connection.

use crate::core::event_bus::{EventBus, Subscription};
use crate::core::notifier::{NotificationReader, MAX_LINE_BYTES};
use crate::core::types::CoreEvent;
use std::io::{ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ALERT: &str = r#"{"jsonrpc":"2.0","method":"core.event","params":{"kind":"RISK_ALERT","code":"KILL_SWITCH_ACTIVE","message":"kill-A"}}"#;

/// An unknown envelope followed by the `core.event` ERROR that must be
/// ingested. The ERROR is the SECOND line on purpose: a reader that died on (or
/// threw away its buffer at) the unknown `ping` method would never deliver it,
/// so waiting for `boom` pins the ignore semantics too.
const PING_THEN_ERROR: &str = "{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"method\":\"core.event\",\"params\":{\"kind\":\"ERROR\",\"error\":\"boom\"}}\n";

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
///
/// The same value bounds the three other waits that must not be inherited:
/// `stop_and_wait` (a stop-path regression is reported, not joined forever),
/// `accept_within` (a reader that never connects is reported, not inherited by
/// a blocking `accept`), and the write timeout in the overlong-line test (a
/// reader that stops draining without closing is reported as such). In each
/// case reaching the bound means zero progress for 30 s, which no scheduling
/// delay on a live thread produces; the assertions say which one happened.
const HANG_PROBE: Duration = Duration::from_secs(30);

/// Gap the split-line test leaves between a record's halves: longer than the
/// reader's 200 ms read timeout, so a timeout lands inside the line.
const SPLIT_GAP: Duration = Duration::from_millis(700);

/// How many newline-less bytes the overlong-line test is willing to push into one
/// session before calling it "the reader kept consuming" (the cap the stopwatch
/// version used). A reader that enforces its bound never lets the writer get
/// here: it closes the session a little past `MAX_LINE_BYTES`.
const OVERLONG_CAP: usize = 8 * 1024 * 1024;

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

/// Make an accepted connection the blocking, bounded stream these fixtures write
/// through.
///
/// On macOS/BSD, `accept()` inherits `O_NONBLOCK` from the listener, so a
/// payload larger than the socket buffer comes back as `WouldBlock` instead of
/// being delivered in full — a 100-byte line still fits in the 8 KiB default
/// send buffer and hides it, while a 64 KiB chunk does not. Clearing the flag
/// and adding a write timeout keeps `write_all` honest without letting a reader
/// that stops draining wedge the server thread (the write fails, the round ends).
fn make_blocking(s: &UnixStream) {
    s.set_nonblocking(false).ok();
    s.set_write_timeout(Some(HANG_PROBE)).ok();
}

/// Accept one connection, waiting up to `within`. `None` means no peer
/// connected in time, which the caller REPORTS instead of inheriting: the old
/// shape called a blocking `accept()` and wedged the suite if the reader never
/// connected. The deadline is a hang probe on the same terms as `HANG_PROBE`
/// (a running reader connects in milliseconds; the bound is only consulted when
/// nothing arrives at all). The listener must be non-blocking.
fn accept_within(listener: &UnixListener, within: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + within;
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                make_blocking(&s);
                return Some(s);
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

/// Serve `line` to EVERY accepted connection until `serving` is cleared,
/// closing every session it held at the end of each round.
///
/// The real core writes notifications to *every* open session and never depends
/// on which one reads them; the reader in turn opens short-lived PROBE
/// connections of its own (`run()` connects and drops one between sessions). A
/// helper that accepts a single connection and assumes it is the reader's live
/// session is therefore guessing, and when it loses that guess the bytes are
/// gone for good — no wait budget can recover them. That is not theoretical: it
/// is #201's other failure mode, reproduced by failing the reader's first
/// connect so the probe lands in `accept()` while the line goes to a session
/// nobody reads. The reader then reports "lines read: 0" — it never saw the
/// line at all, which is also why raising the budget 2 s -> 10 s could not help.
/// Writing to every accepted connection removes the guess.
///
/// Closing the held sessions at the end of each round is what exercises the
/// reconnect path: the reader must observe EOF and come back.
fn serve_lines_rounds(
    listener: UnixListener,
    line: Arc<Vec<u8>>,
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
                        make_blocking(&s);
                        let _ = s.write_all(line.as_slice()).and_then(|_| s.flush());
                        held.push(s);
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
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

/// Serve one line split across a read timeout to EVERY accepted connection: the
/// first half, then `gap`, then the terminating newline, then close. Same
/// no-guessing rule as `serve_lines_rounds` — the halves must not be handed to a
/// probe connection the reader is not reading, or the test would go red for
/// fixture ordering rather than for a lost partial record. A probe write is
/// detected (its second write fails) and the next connection is served, so the
/// reader's live session still gets its halves.
fn serve_split_line(
    listener: UnixListener,
    first: Arc<Vec<u8>>,
    gap: Duration,
    connections: Arc<AtomicUsize>,
    serving: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).ok();
        while serving.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut s, _)) => {
                    connections.fetch_add(1, Ordering::SeqCst);
                    make_blocking(&s);
                    if s.write_all(first.as_slice())
                        .and_then(|_| s.flush())
                        .is_ok()
                    {
                        std::thread::sleep(gap);
                        let _ = s.write_all(b"\n").and_then(|_| s.flush());
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
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
    wait_error_matching(sub, bus, |e| e == payload)
}

/// Wait for a `core.event` ERROR whose payload contains `needle` (for
/// diagnostics the reader composes, e.g. the overlong-line bound trip).
fn wait_error_containing(sub: &mut Subscription, bus: &EventBus, needle: &str) -> bool {
    wait_error_matching(sub, bus, |e| e.contains(needle))
}

/// Wait for a `core.event` ERROR accepted by `pred`. Event-driven (see
/// `EventBus::wait_for`) — `HANG_PROBE` bounds only a reader that publishes
/// nothing at all.
fn wait_error_matching(
    sub: &mut Subscription,
    bus: &EventBus,
    pred: impl Fn(&str) -> bool,
) -> bool {
    bus.wait_for(
        sub,
        HANG_PROBE,
        |ev| matches!(ev, CoreEvent::Error { error } if error.as_str().is_some_and(&pred)),
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
    let server = serve_lines_rounds(
        listener,
        Arc::new(format!("{ALERT}\n").into_bytes()),
        connections.clone(),
        serving.clone(),
    );

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
    // (A reader starved past the gap can read both halves in one go and pass
    // without exercising the split — a coverage loss under extreme load, never
    // a false red; the stopwatch version had the same property.)
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), RETRY_MS);
    let mut fixture = ReaderFixture::start(reader);

    let serving = Arc::new(AtomicBool::new(true));
    let server = serve_split_line(
        listener,
        Arc::new(ALERT.as_bytes().to_vec()),
        SPLIT_GAP,
        Arc::new(AtomicUsize::new(0)),
        serving.clone(),
    );

    let got = wait_alert(&mut sub, &bus, "kill-A");
    let exited = fixture.stop_and_wait();
    serving.store(false, Ordering::SeqCst);
    let _ = server.join();
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
/// Two observables, both signals rather than a stopwatch. The reader REPORTS the
/// bound trip on the bus (`event stream error: notification line exceeded ...
/// without a newline`), and the session it was reading REFUSES writes once it
/// closes. A refusal only counts when that session had already taken
/// `MAX_LINE_BYTES`: the reader drains what it accepts, so reaching the bound
/// proves the bytes went to a live reader, whereas a refusal on a connection
/// nobody is draining (one of the reader's probes, or a session the reader has
/// already left) arrives after a socket buffer's worth — 8 KiB by default on
/// macOS, and 0 bytes once the peer is closed — and proves nothing. Accepting
/// such a refusal is how this test could pass with the reader never seeing a
/// byte, which is why the first accept may be retried.
#[test]
fn reader_stops_reading_an_overlong_line_without_a_newline() {
    let (sock, listener) = bind_socket("overlong");
    let bus = EventBus::new(8);
    let mut sub = bus.subscribe();
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), RETRY_MS);
    let mut fixture = ReaderFixture::start(reader);
    listener.set_nonblocking(true).ok();

    let chunk = vec![b'x'; 64 * 1024];
    // Warm-up session: `run()` announces a session's failure only after it has
    // announced a connection (it tracks `connected` and stays quiet on the very
    // first transition). Closing one accepted connection here moves the overlong
    // feed onto a later session, so the bound trip is reported at all. That
    // report is what the assertion below pins — the reader's own diagnostic,
    // which is what the panel shows, not merely a socket closing.
    if let Some(s) = accept_within(&listener, HANG_PROBE) {
        drop(s); // EOF on the reader's first session, silently
    }
    let mut sessions = 0usize;
    let mut max_sent = 0usize;
    let mut refused_at: Option<usize> = None;
    let mut stalled = false;
    let mut never_connected = false;
    // At most 4 sessions: the first accept() can hand back one of the reader's
    // short-lived probes, whose refusal is not the observable.
    while refused_at.is_none() && !stalled && sessions < 4 && !never_connected {
        let Some(mut s) = accept_within(&listener, HANG_PROBE) else {
            never_connected = true;
            break;
        };
        sessions += 1;
        let mut sent = 0usize;
        let mut refused = false;
        while sent < OVERLONG_CAP {
            // `write`, not `write_all`: bytes accepted before the refusal are
            // counted one syscall at a time, which is what makes the
            // `MAX_LINE_BYTES` guard below a measurement instead of a guess.
            match s.write(&chunk) {
                Ok(n) if n > 0 => sent += n,
                // A non-empty buffer that accepts nothing is no progress; a peer
                // that stopped reading shows up here, bounded by the write
                // timeout. It never decides PASS/FAIL for a live reader, which
                // drains (see the report).
                Ok(_) => {
                    stalled = true;
                    break;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    stalled = true;
                    break;
                }
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        max_sent = max_sent.max(sent);
        if refused && sent >= MAX_LINE_BYTES {
            refused_at = Some(sent);
        }
    }

    let reported = wait_error_containing(&mut sub, &bus, "without a newline");
    // Snapshot liveness BEFORE the stop flag: after `stop_and_wait` the reader
    // is gone by design, and a dead-by-design reader in a hang report would say
    // nothing.
    let running = fixture.running();
    let exited = fixture.stop_and_wait();
    let lines = fixture.lines();
    drop(listener);
    cleanup(&sock);

    assert!(
        !never_connected,
        "HANG: the reader never connected within {HANG_PROBE:?} of the listener being bound \
         (lines read: {lines}, reader thread still running: {running})"
    );
    assert!(
        !stalled,
        "HANG: the reader accepted a session, took {max_sent} bytes, then stopped reading without \
         closing — the write stalled for {HANG_PROBE:?} on a full buffer (reader thread still \
         running: {running}, sessions served: {sessions})"
    );
    assert!(
        reported,
        "the reader never reported the overlong-line bound on the bus (sessions served: {sessions}, \
         bytes accepted by one session before the refusal: {refused_at:?}, reader thread still \
         running: {running})"
    );
    assert!(
        refused_at.is_some(),
        "reader kept consuming a newline-less stream past the bound (best session accepted \
         {max_sent} of {OVERLONG_CAP} bytes before the cap ended the run, sessions served: \
         {sessions})"
    );
    assert!(
        exited,
        "hang: run() did not return within {HANG_PROBE:?} of stop() (lines read: {lines})"
    );
}

/// The reader must ignore envelopes that are not `core.event`, ingest the ones
/// that are, and still stop on command.
///
/// Both halves are observed on a SIGNAL, not on a stopwatch (#201). The wait is
/// released by `EventBus::wait_for` the moment the reader publishes, so a
/// loaded machine costs this test nothing; `HANG_PROBE` bounds only a reader
/// that never publishes. The two outcomes are asserted separately, so a failure
/// names the one that happened — the old shared message, "core.event ERROR line
/// was not ingested (or stop hung)", could not tell a reader that missed the
/// line from one that never returned, and the 2 s -> 10 s budget bump only
/// moved the threshold the race ran against.
///
/// The line is served to every accepted connection (`serve_lines_rounds`), not
/// written into the one connection `accept()` happened to return: the reader
/// opens short-lived probe connections, and #201's intermittency also had that
/// second cause — a line written into a session nobody reads is lost before any
/// wait begins, however long the budget is.
#[test]
fn reader_ignores_non_core_event_lines_and_stops() {
    let (sock, listener) = bind_socket("ign");
    let bus = EventBus::new(8);
    let mut sub = bus.subscribe();
    let reader = NotificationReader::new(sock.to_string_lossy().to_string(), bus.clone(), RETRY_MS);
    let mut fixture = ReaderFixture::start(reader);

    let connections = Arc::new(AtomicUsize::new(0));
    let serving = Arc::new(AtomicBool::new(true));
    let server = serve_lines_rounds(
        listener,
        Arc::new(PING_THEN_ERROR.as_bytes().to_vec()),
        connections.clone(),
        serving.clone(),
    );

    // 1. Ingestion: blocks until THIS line's event is published.
    let ingested = wait_error(&mut sub, &bus, "boom");
    // 2. Stop: an independent claim, with its own budget and its own message.
    let exited = fixture.stop_and_wait();
    // Snapshot the reader's state before the thread is joined: "still running"
    // is exactly what a hang report has to say, and a join hides it.
    let running = fixture.running();
    let lines = fixture.lines();
    let published = bus.seq();
    let sessions = connections.load(Ordering::SeqCst);
    serving.store(false, Ordering::SeqCst);
    let _ = server.join();
    cleanup(&sock);

    assert!(
        ingested,
        "NOT INGESTED: the core.event ERROR line never reached the bus within the {HANG_PROBE:?} \
         hang probe — lines read: {lines}, events published: {published}, sessions served: \
         {sessions}, reader thread still running: {running}, run() returned on stop: {exited}"
    );
    assert!(
        exited,
        "HANG: run() did not return within {HANG_PROBE:?} of stop() being set — the stop path is \
         stuck, the line under test is not at fault (lines read: {lines}, events published: \
         {published}, sessions served: {sessions})"
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
    listener.set_nonblocking(true).ok();
    // Bounded accept: without it, a reader that never connects wedges the suite
    // right here, which is the failure mode this file must not have.
    let conn = accept_within(&listener, HANG_PROBE); // stays open, silent
    let connected = conn.is_some();
    std::thread::sleep(Duration::from_millis(100));
    let exited = fixture.stop_and_wait();
    let running = fixture.running();
    let lines = fixture.lines();
    drop(conn);
    drop(listener);
    cleanup(&sock);
    assert!(
        connected,
        "HANG: the reader never connected within {HANG_PROBE:?} of the listener being bound \
         (lines read: {lines}, reader thread still running: {running})"
    );
    assert!(
        exited,
        "HANG: run() did not return within {HANG_PROBE:?} of stop() while a silent peer held the \
         connection open (reader thread still running: {running})"
    );
}
