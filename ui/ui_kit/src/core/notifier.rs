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
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Sell one reader thread as a daemon. Returns an `Arc<AtomicBool>` flag that
/// flips false when the thread is asked to stop (`stop()`).
pub struct NotificationReader {
    socket_path: String,
    bus: EventBus,
    running: Arc<AtomicBool>,
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
    fn read_until_closed(&self) -> Result<(), String> {
        let stream = UnixStream::connect(&self.socket_path).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(
                self.retry_ms.max(200),
            )))
            .map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            if !self.running.load(Ordering::SeqCst) {
                return Ok(());
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return Ok(()), // EOF: core went away
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => {
                    self.bus.ingest_notification(line.trim_end());
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue; // just the stop-flag checkpoint
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}
