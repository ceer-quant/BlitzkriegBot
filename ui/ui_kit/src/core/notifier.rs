//! Notification reader — ONE dedicated connection that only reads the core's
//! push stream (`core.event`) and publishes decoded events to the EventBus.
//!
//! Why a second connection: the core writes notifications to every open session
//! (spawn_session broadcasts on `bus_tx`), so a socket can either serve the
//! request/response loop or act as a pure event listener — the latter never
//! sends anything, blocks on read, and publishes everything it sees to the bus.
//! Backpressure lives in the ring (bounded, oldest-dropped); a fast core can
//! never block on the UI.
//!
//! Reconnects on EOF/error with the caller's interval; surfaces connection
//! state through the bus (`CoreEvent::Error` with a text payload) sparingly —
//! only on transitions, not on every failed attempt.

use crate::core::event_bus::EventBus;
use crate::core::types::CoreEvent;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Upper bound on a single notification line. The core writes one compact
/// JSON-RPC object per line, so this is orders of magnitude above any real
/// event; it exists only so a peer streaming without newlines cannot grow the
/// reader's line buffer without bound.
pub(crate) const MAX_LINE_BYTES: usize = 1 << 20;

/// Sell one reader thread as a daemon. Returns an `Arc<AtomicBool>` flag that
/// flips false when the thread is asked to stop (`stop()`).
pub struct NotificationReader {
    socket_path: String,
    bus: EventBus,
    running: Arc<AtomicBool>,
    /// Complete lines this reader has handed to the bus (see `lines_handle`).
    lines: Arc<AtomicUsize>,
    /// Milliseconds between reconnect attempts (and how long a connect probe
    /// is allowed to block).
    retry_ms: u64,
}

impl NotificationReader {
    pub fn new(socket_path: impl Into<String>, bus: EventBus, retry_ms: u64) -> Self {
        Self {
            socket_path: socket_path.into(),
            bus,
            running: Arc::new(AtomicBool::new(true)),
            lines: Arc::new(AtomicUsize::new(0)),
            retry_ms,
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Clone of the internal running flag, for tests / supervisors that hold
    /// the reader thread's join handle and need to end it.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    /// Count of complete lines this reader has handed to the bus.
    ///
    /// A stalled event stream looks identical from the outside across three
    /// different causes — the bytes never arrived, they arrived but did not
    /// decode, or they decoded but the bus was never told — and a report that
    /// cannot separate them cannot say what to fix. Paired with `EventBus::seq`
    /// this counter is what tells "the reader never saw the line" from "the
    /// reader saw it and nothing came out", which is exactly the distinction
    /// the notifier tests need when one of them fails.
    pub fn lines_handle(&self) -> Arc<AtomicUsize> {
        self.lines.clone()
    }

    /// Run the read loop (call on a dedicated thread). Loop exits when
    /// `stop()` is called or the socket path disappears permanently.
    pub fn run(self) {
        let mut connected = false;
        while self.running.load(Ordering::SeqCst) {
            match self.read_until_closed() {
                Ok(()) => {
                    // Clean EOF: core went away. Announce once, then keep
                    // probing (the panel's own polling loop will still draw).
                    if connected {
                        connected = false;
                        self.bus.publish(CoreEvent::Error {
                            error: serde_json::json!("event stream closed"),
                        });
                    }
                }
                Err(e) => {
                    if connected {
                        connected = false;
                        self.bus.publish(CoreEvent::Error {
                            error: serde_json::json!(format!("event stream error: {e}")),
                        });
                    }
                }
            }
            // Service-aware probe: only treat `connect` success as a
            // transition event — the next successful session announces ready.
            match UnixStream::connect(&self.socket_path) {
                Ok(s) => {
                    connected = true;
                    drop(s);
                }
                Err(_) => {
                    // The reader thread is the only sender of this
                    // transition; skip spam while the core is down.
                    let _ = self.retry_ms;
                    std::thread::sleep(std::time::Duration::from_millis(self.retry_ms.max(200)));
                }
            }
        }
    }

    /// Block reading notification lines until EOF, a hard error, or the stop
    /// flag. A short read timeout keeps the loop interruptible — a silent
    /// (but connected) core must not trap the thread forever.
    ///
    /// Bytes are assembled here rather than via `read_line` because a timeout
    /// can land in the middle of a record: `read_line` hands back the partial
    /// prefix on the error path, so the next attempt would have to either
    /// resume from it or lose it, and it offers no way to cap how far it grows
    /// while it waits for a delimiter. Reading fixed-size chunks keeps a
    /// half-received record intact across a timeout and puts the size limit
    /// somewhere it can actually be enforced.
    fn read_until_closed(&self) -> Result<(), String> {
        let mut stream = UnixStream::connect(&self.socket_path).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(
                self.retry_ms.max(200),
            )))
            .map_err(|e| e.to_string())?;
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8 * 1024];
        loop {
            if !self.running.load(Ordering::SeqCst) {
                return Ok(());
            }
            match stream.read(&mut chunk) {
                Ok(0) => {
                    // EOF: core went away. Flush a trailing record that was
                    // never newline-terminated rather than discarding it.
                    self.publish_line(&mut pending);
                    return Ok(());
                }
                Ok(n) => {
                    let mut start = 0;
                    for (i, &b) in chunk[..n].iter().enumerate() {
                        if b == b'\n' {
                            pending.extend_from_slice(&chunk[start..i]);
                            self.publish_line(&mut pending);
                            start = i + 1;
                        }
                    }
                    pending.extend_from_slice(&chunk[start..n]);
                    if pending.len() > MAX_LINE_BYTES {
                        return Err(format!(
                            "notification line exceeded {MAX_LINE_BYTES} bytes without a newline"
                        ));
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // Just the stop-flag checkpoint: a record split across the
                    // timeout keeps its first half in `pending`, which the
                    // read arm above has already size-checked.
                    continue;
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    /// Publish one complete line and reset the accumulator. A non-UTF-8
    /// record is dropped rather than killing the stream, so one malformed line
    /// cannot blind the UI to everything that follows it.
    fn publish_line(&self, pending: &mut Vec<u8>) {
        if let Ok(text) = std::str::from_utf8(pending) {
            let text = text.trim();
            if !text.is_empty() {
                // Counted BEFORE the bus sees it, so a line that was read but
                // never reached the bus (a decode drop, a swallowed publish)
                // still shows up as read.
                self.lines.fetch_add(1, Ordering::SeqCst);
                self.bus.ingest_notification(text);
            }
        }
        pending.clear();
    }
}
