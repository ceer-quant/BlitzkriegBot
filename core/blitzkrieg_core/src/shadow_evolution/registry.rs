//! Shadow Evolution — the per-strategy hot-parameter registry.
//!
//! Before E2-c the engine held ONE `Arc<ArcSwap<MutableParams>>` and handed the
//! same handle to every strategy, so every strategy read spread_arb's four
//! fields. Now the engine holds an `Arc<ParamRegistry>`: a strategy name →
//! `Arc<ArcSwap<StrategyParams>>` map. Each strategy resolves **its own** cell
//! once (at wire time) and then reads it lock-free on the hot path, exactly as
//! before — the only change is that the cell is namespaced by strategy.
//!
//! Isolation is structural: two strategies cannot observe or overwrite each
//! other's parameters because they do not share a cell.

use super::knobs::StrategyParams;
use arc_swap::ArcSwap;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

pub struct ParamRegistry {
    /// Created/looked up behind an `RwLock` (wire-time only). Reads on the hot
    /// path go through the returned `ArcSwap`, which is lock-free.
    cells: RwLock<BTreeMap<String, Arc<ArcSwap<StrategyParams>>>>,
}

impl Default for ParamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ParamRegistry {
    pub fn new() -> Self {
        Self { cells: RwLock::new(BTreeMap::new()) }
    }

    /// Publish the initial parameter set for `strategy`, creating its cell.
    /// Re-publishing keeps the SAME cell (and therefore every already-wired
    /// strategy reference) and only replaces the value.
    pub fn publish(&self, strategy: &str, params: StrategyParams) -> Arc<ArcSwap<StrategyParams>> {
        let mut cells = self.cells.write().expect("param registry poisoned");
        let cell = cells
            .entry(strategy.to_string())
            .or_insert_with(|| Arc::new(ArcSwap::from_pointee(StrategyParams::new())))
            .clone();
        cell.store(Arc::new(params));
        cell
    }

    /// The cell for `strategy`, if that strategy declared itself evolvable.
    /// `None` = not evolvable (it declares no knobs) — the strategy then never
    /// receives hot parameters, which is an explicit declaration, not an error.
    pub fn handle_for(&self, strategy: &str) -> Option<Arc<ArcSwap<StrategyParams>>> {
        self.cells.read().expect("param registry poisoned").get(strategy).cloned()
    }

    /// Strategy names that have a cell (i.e. declared evolvable).
    pub fn names(&self) -> Vec<String> {
        self.cells.read().expect("param registry poisoned").keys().cloned().collect()
    }

    /// Drop the cell for a strategy that no longer exists / no longer declares
    /// knobs, so a stale handle cannot outlive its unit.
    pub fn remove(&self, strategy: &str) {
        self.cells.write().expect("param registry poisoned").remove(strategy);
    }

    /// Current value of one knob, if published (observability/tests).
    pub fn get(&self, strategy: &str, knob: &str) -> Option<rust_decimal::Decimal> {
        let cell = self.handle_for(strategy)?;
        let p = cell.load();
        p.get(knob)
    }

    /// Snapshot of every strategy's parameters (observability/tests).
    pub fn snapshot(&self) -> super::knobs::MutableParams {
        let mut out = super::knobs::MutableParams::new();
        for name in self.names() {
            if let Some(cell) = self.handle_for(&name) {
                out.set_strategy(&name, (**cell.load()).clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::knobs::KnobSpec;
    use super::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn knobs(v: Decimal) -> StrategyParams {
        StrategyParams::from_knobs(&[KnobSpec::new("k", v, dec!(0), dec!(10))])
    }

    #[test]
    fn cells_are_isolated_per_strategy() {
        let r = ParamRegistry::new();
        let a = r.publish("a", knobs(dec!(1)));
        let b = r.publish("b", knobs(dec!(5)));
        assert_ne!(Arc::as_ptr(&a), Arc::as_ptr(&b), "each strategy gets its own cell");
        a.store(Arc::new(knobs(dec!(2))));
        assert_eq!(r.get("a", "k"), Some(dec!(2)));
        assert_eq!(r.get("b", "k"), Some(dec!(5)), "writing A must not move B");
    }

    #[test]
    fn republishing_keeps_the_same_cell() {
        let r = ParamRegistry::new();
        let first = r.publish("s", StrategyParams::new());
        let second = r.publish("s", StrategyParams::new());
        assert_eq!(Arc::as_ptr(&first), Arc::as_ptr(&second), "wired handles must stay valid");
        assert_eq!(r.names(), vec!["s".to_string()]);
        r.remove("s");
        assert!(r.handle_for("s").is_none());
        assert!(r.names().is_empty());
    }
}
