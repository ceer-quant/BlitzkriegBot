//! Event bus — subscribe/dispatch of `core.event` notifications.
//!
//! The core's push channel and the UI's render loop are decoupled: a background
//! reader (or the adapter's own poll loop) pushes decoded `CoreEvent`s here, and
//! any number of adapters subscribe. Keeping this in the shared core means the
//! web, TUI and native-app adapters all observe the SAME event stream.

use crate::core::types::CoreEvent;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A bounded, cloneable, multi-subscriber event ring.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Mutex<Inner>>,
    /// Signalled on every publish, so `wait_for` is woken BY the publisher
    /// rather than by a poll interval it would have to guess at.
    signal: Arc<Condvar>,
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
            inner: Arc::new(Mutex::new(Inner {
                seq: 0,
                events: VecDeque::new(),
                cap: cap.max(1),
            })),
            signal: Arc::new(Condvar::new()),
        }
    }

    /// Publish a decoded event; returns its sequence number.
    pub fn publish(&self, ev: CoreEvent) -> u64 {
        let seq = {
            let mut g = self.inner.lock().unwrap();
            g.seq += 1;
            let seq = g.seq;
            g.events.push_back((seq, ev));
            while g.events.len() > g.cap {
                g.events.pop_front();
            }
            seq
        };
        // Notify AFTER releasing the lock: a waiter woken while this call still
        // held it would go straight back to sleep on the mutex.
        self.signal.notify_all();
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
        collect_new(&mut self.inner.lock().unwrap(), sub, |_| true)
    }

    /// Block until an event newer than `sub.cursor` satisfies `pred`, and return
    /// it. Woken by the publisher (`publish` signals the condvar), so the caller
    /// costs nothing while the bus is quiet and returns the instant the awaited
    /// event lands — no poll interval to tune, no wall-clock budget for the
    /// caller to get wrong.
    ///
    /// `timeout` is a HANG PROBE, not a latency budget: it bounds how long we
    /// are willing to wait for a publisher that never comes. It cannot cut a
    /// live wait short, because the scan runs AGAIN after a timed-out wait, so
    /// an event published as the deadline expires is still returned.
    ///
    /// Consumed like `drain_new`: the cursor advances past the whole batch
    /// (matching or not), so a caller that needs the other events must drain
    /// first. Returns the first match in sequence order, or `None` on timeout.
    pub fn wait_for<F>(
        &self,
        sub: &mut Subscription,
        timeout: Duration,
        pred: F,
    ) -> Option<CoreEvent>
    where
        F: Fn(&CoreEvent) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let mut guard = self.inner.lock().unwrap();
        loop {
            if let Some(ev) = collect_new(&mut guard, sub, &pred).into_iter().next() {
                return Some(ev);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            // Spurious wakeups are harmless: the loop re-scans and re-decides.
            guard = self.signal.wait_timeout(guard, deadline - now).unwrap().0;
        }
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

/// Events newer than `sub.cursor` that `pred` accepts, advancing the cursor to
/// the batch head. ONE implementation behind `drain_new` and `wait_for`, so the
/// two can never disagree about what "new" means or how far the cursor moves.
fn collect_new(
    g: &mut Inner,
    sub: &mut Subscription,
    pred: impl Fn(&CoreEvent) -> bool,
) -> Vec<CoreEvent> {
    let oldest = g.events.front().map(|(s, _)| *s).unwrap_or(g.seq + 1);
    if sub.cursor + 1 < oldest {
        sub.cursor = oldest.saturating_sub(1);
    }
    let out: Vec<CoreEvent> = g
        .events
        .iter()
        .filter(|(s, e)| *s > sub.cursor && pred(e))
        .map(|(_, e)| e.clone())
        .collect();
    if let Some((s, _)) = g.events.back() {
        sub.cursor = *s;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk_alert(msg: &str) -> CoreEvent {
        CoreEvent::RiskAlert {
            code: serde_json::json!("Internal"),
            message: msg.into(),
        }
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
        // A subscriber that only observes (never drains) must not break ingestion.
        let s = bus.subscribe();
        // Cursor started after publish, so subscribe first is the realistic order:
        let bus2 = EventBus::new(8);
        let mut s2 = bus2.subscribe();
        bus2.ingest_notification(line);
        let got = bus2.drain_new(&mut s2);
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], CoreEvent::RiskAlert { .. }));
        let _ = (s, bus);
    }

    /// The point of `wait_for`: the caller is released by the PUBLISH, so the
    /// wait ends as soon as the event lands rather than when a poll wakes up.
    /// The 10 s cap is a hang probe (a wait that ignored the signal and sat out
    /// its 60 s timeout would trip it) — it is not a latency budget.
    #[test]
    fn wait_for_is_released_by_the_publish_not_by_its_timeout() {
        let bus = EventBus::new(8);
        let mut sub = bus.subscribe();
        let publisher = {
            let bus = bus.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                bus.publish(risk_alert("late"));
            })
        };

        let started = Instant::now();
        let got = bus.wait_for(
            &mut sub,
            Duration::from_secs(60),
            |ev| matches!(ev, CoreEvent::RiskAlert { message, .. } if message == "late"),
        );
        let waited = started.elapsed();
        publisher.join().unwrap();

        assert!(got.is_some(), "the published event was never returned");
        assert!(
            waited < Duration::from_secs(10),
            "wait_for sat on its timeout instead of waking on the publish ({waited:?})"
        );
    }

    /// A wait with no publisher must still END (the caller's hang probe), and
    /// must say so with `None` rather than an empty event.
    #[test]
    fn wait_for_times_out_with_none_when_nothing_matches() {
        let bus = EventBus::new(8);
        let mut sub = bus.subscribe();
        bus.publish(risk_alert("not-the-one"));

        let started = Instant::now();
        let got = bus.wait_for(
            &mut sub,
            Duration::from_millis(100),
            |ev| matches!(ev, CoreEvent::RiskAlert { message, .. } if message == "absent"),
        );
        assert!(got.is_none(), "no matching event was published");
        // Monotonic clock, and the deadline is measured from before the call,
        // so this direction can only be violated by a wait that ignored it.
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(bus.seq() == 1);
    }

    /// `wait_for` consumes what it looked at, exactly like `drain_new`: the
    /// match is returned once, non-matching events in the same batch are
    /// consumed with it, and a second wait sees only later publishes.
    #[test]
    fn wait_for_consumes_the_batch_like_drain_new() {
        let bus = EventBus::new(8);
        let mut sub = bus.subscribe();
        bus.publish(risk_alert("first"));
        bus.publish(risk_alert("wanted"));

        let got = bus.wait_for(
            &mut sub,
            Duration::from_secs(1),
            |ev| matches!(ev, CoreEvent::RiskAlert { message, .. } if message == "wanted"),
        );
        assert!(got.is_some(), "the matching event must be returned");
        assert!(
            bus.drain_new(&mut sub).is_empty(),
            "the returned event (and its batch) must not be seen twice"
        );
        assert!(
            bus.wait_for(&mut sub, Duration::from_millis(50), |_| true)
                .is_none(),
            "a later wait must see only events published after the first one"
        );
    }
}
