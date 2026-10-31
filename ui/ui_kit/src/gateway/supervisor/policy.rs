//! The restart policy: how hard the supervisor may try to keep an owned core alive.

use std::time::Duration;

/// How hard the supervisor should try to keep an owned core alive.
///
/// Default is **off**: replacing a process the operator did not ask to replace
/// is a policy, and a supervisor should not acquire one by accident. A launcher
/// whose entire job is "keep the kernel up" turns it on explicitly, and the
/// attempt budget is finite so an instantly-dying core cannot be respawned
/// forever — the attempt counter is surfaced so the UI can say "gave up" rather
/// than showing a healthy-looking process that will never come back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub enabled: bool,
    /// Total respawns allowed for this supervisor's lifetime.
    pub max_attempts: u32,
    /// First backoff; doubles per attempt up to `max_backoff`.
    pub base_backoff: Duration,
    pub max_backoff: Duration,
}

impl RestartPolicy {
    pub const fn off() -> Self {
        Self {
            enabled: false,
            max_attempts: 0,
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(8),
        }
    }

    pub const fn keep_alive() -> Self {
        Self {
            enabled: true,
            max_attempts: 5,
            ..RestartPolicy::off()
        }
    }

    /// Backoff before the `attempt`-th respawn (0-based).
    pub(crate) fn backoff_for(&self, attempt: u32) -> Duration {
        let shift = attempt.min(16);
        let ms = self
            .base_backoff
            .as_millis()
            .saturating_mul(1u128 << shift)
            .min(self.max_backoff.as_millis());
        Duration::from_millis(ms as u64)
    }
}

#[test]
fn backoff_grows_and_is_capped() {
    let p = RestartPolicy::keep_alive();
    assert_eq!(p.backoff_for(0), Duration::from_millis(250));
    assert_eq!(p.backoff_for(1), Duration::from_millis(500));
    assert_eq!(p.backoff_for(2), Duration::from_millis(1000));
    // Far enough out it saturates at max_backoff rather than overflowing.
    assert_eq!(p.backoff_for(40), p.max_backoff);
}
