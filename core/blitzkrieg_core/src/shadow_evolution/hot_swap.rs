//! Shadow Evolution — hot swap of the live mutable parameters.
//!
//! `ArcSwap` gives a lock-free, nanosecond read on the strategy hot path and an
//! atomic store for the evolution path. The engine holds a clone of the same
//! `Arc<ArcSwap<MutableParams>>`, so the next tick observes the new values with
//! no restart, no WebSocket churn and no effect on open positions or in-flight
//! orders.

use super::config::MutableParams;
use arc_swap::ArcSwap;
use std::sync::Arc;

pub struct HotSwap {
    inner: Arc<ArcSwap<MutableParams>>,
}

impl HotSwap {
    pub fn new(initial: MutableParams) -> Self {
        Self { inner: Arc::new(ArcSwap::from_pointee(initial)) }
    }

    /// Shared handle for the strategy hot path (cheap clone).
    pub fn handle(&self) -> Arc<ArcSwap<MutableParams>> {
        self.inner.clone()
    }

    /// Current parameters (lock-free load).
    pub fn current(&self) -> Arc<MutableParams> {
        self.inner.load_full()
    }

    /// Atomically publish new parameters.
    pub fn store(&self, params: MutableParams) {
        self.inner.store(Arc::new(params));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn store_is_visible_to_shared_handle() {
        let hs = HotSwap::new(MutableParams::default());
        let shared = hs.handle();
        let before = shared.load_full().trend_max_entry_price;
        hs.store(MutableParams::default().scaled(dec!(1.04)));
        let after = shared.load_full().trend_max_entry_price;
        assert_ne!(before, after);
        assert_eq!(after, before * dec!(1.04));
    }
}
