//! Event bus — subscribe/dispatch of `core.event` notifications.
//!
//! The core's push channel and the UI's render loop are decoupled: a background
//! reader (or the adapter's own poll loop) pushes decoded `CoreEvent`s here, and
//! any number of adapters subscribe. Keeping this in the shared core means the
//! web, TUI and native-app adapters all observe the SAME event stream.

use crate::core::types::CoreEvent;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// A bounded, cloneable, multi-subscriber event ring.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    seq: u64,
    events: VecDeque<(u64, CoreEvent)>,
    cap: usize,
}

/// A cursor into the bus. `drain_new` returns events published since the last
/// call, so several adapters can each read independently.
#[derive(Debug, Clone)]
pub struct Subscription {
    cursor: u64,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl EventBus {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner { seq: 0, events: VecDeque::new(), cap: cap.max(1) })),
        }
    }

    /// Publish a decoded event; returns its sequence number.
    pub fn publish(&self, ev: CoreEvent) -> u64 {
        let mut g = self.inner.lock().unwrap();
        g.seq += 1;
        let seq = g.seq;
        g.events.push_back((seq, ev));
        while g.events.len() > g.cap {
            g.events.pop_front();
        }
        seq
    }

    /// Current sequence (latest published).
    pub fn seq(&self) -> u64 {
        self.inner.lock().unwrap().seq
    }

    /// Create an independent subscription positioned at the current head, so a
    /// new subscriber only sees events published *after* it subscribes.
    pub fn subscribe(&self) -> Subscription {
        Subscription { cursor: self.seq() }
    }

    /// All events newer than `sub.cursor`, advancing it. Events older than the
    /// ring have been evicted; the cursor is then fast-forwarded to the oldest.
    pub fn drain_new(&self, sub: &mut Subscription) -> Vec<CoreEvent> {
        let g = self.inner.lock().unwrap();
        let oldest = g.events.front().map(|(s, _)| *s).unwrap_or(g.seq + 1);
        if sub.cursor + 1 < oldest {
            sub.cursor = oldest.saturating_sub(1);
        }
        let out: Vec<CoreEvent> = g
            .events
            .iter()
            .filter(|(s, _)| *s > sub.cursor)
            .map(|(_, e)| e.clone())
            .collect();
        if let Some((s, _)) = g.events.back() {
            sub.cursor = *s;
        }
        out
    }

    /// Decode a raw `core.event` notification line (`{jsonrpc,method,params}`)
    /// and publish it. Returns the envelope method if it was recognised.
    pub fn ingest_notification(&self, line: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let method = v.get("method")?.as_str()?.to_string();
        if method != "core.event" {
            return None;
        }
        if let Some(params) = v.get("params") {
            if let Ok(ev) = serde_json::from_value::<CoreEvent>(params.clone()) {
                self.publish(ev);
            }
        }
        Some(method)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk_alert(msg: &str) -> CoreEvent {
        CoreEvent::RiskAlert { code: serde_json::json!("Internal"), message: msg.into() }
    }

    #[test]
    fn subscribers_see_independent_cursors() {
        let bus = EventBus::new(16);
        let mut a = bus.subscribe();
        bus.publish(risk_alert("one"));
        let mut b = bus.subscribe(); // b starts after "one"
        bus.publish(risk_alert("two"));

        let ea = bus.drain_new(&mut a);
        let eb = bus.drain_new(&mut b);
        assert_eq!(ea.len(), 2, "a saw both");
        assert_eq!(eb.len(), 1, "b saw only the event after it subscribed");
        assert_eq!(bus.drain_new(&mut a).len(), 0, "no repeat for a");
    }

    #[test]
    fn ring_eviction_fast_forwards_lagging_subscriber() {
        let bus = EventBus::new(3);
        let mut s = bus.subscribe();
        for i in 0..5 {
            bus.publish(risk_alert(&format!("e{i}")));
        }
        // Only the last 3 survive; the lagging cursor is fast-forwarded.
        let got = bus.drain_new(&mut s);
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn ingests_raw_core_event_notification() {
        let bus = EventBus::new(8);
        let line = r#"{"jsonrpc":"2.0","method":"core.event","params":{"kind":"RISK_ALERT","code":"Internal","message":"hi"}}"#;
        assert_eq!(bus.ingest_notification(line).as_deref(), Some("core.event"));
        let mut s = bus.subscribe();
        // Cursor started after publish, so subscribe first is the realistic order:
        let bus2 = EventBus::new(8);
        let mut s2 = bus2.subscribe();
        bus2.ingest_notification(line);
        let got = bus2.drain_new(&mut s2);
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], CoreEvent::RiskAlert { .. }));
        let _ = (s, bus);
    }
}
