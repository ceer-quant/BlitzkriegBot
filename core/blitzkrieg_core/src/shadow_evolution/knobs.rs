//! Shadow Evolution — the per-strategy parameter model (E2-c / #28).
//!
//! ```text
//! MutableParams                     ← the aggregate (strategy → params)
//!   └─ "alpha_strategy" → StrategyParams ← that strategy's knob values
//!   └─ "spread_arb" → StrategyParams
//! KnobSpec { name, value, min, max } ← what a strategy DECLARES evolvable
//! ```
//!
//! A strategy declares its own knobs and **domains** (`KnobSpec`); the domain is
//! a hard outer bound the value may never leave, checked before the ±gradient
//! lock. `StrategyParams` is a name → `Decimal` bag, so a strategy's parameters
//! never touch another's.
//!
//! Wire rule: decimals are JSON **strings**, never floats.
//!
//! PR-B: `KnobSpec`/`KnobDeclaration`/`StrategyParams` are THE shared types from
//! the `strategy-logic` crate — the same definitions an external strategy cdylib
//! compiles against, so a knob declaration crosses the ABI and the evolution
//! machinery unchanged. `MutableParams` (the aggregate) stays kernel-side.
//!
//! Only `StrategyParams::set_declared`, `scaled`, `domain_violation`,
//! `undeclared` and `clamp_to` are exercised by the evolution guard/evaluator;
//! their tests live with the shared crate.

pub use strategy_logic::params::{KnobDeclaration, KnobSpec, StrategyParams};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

/// The aggregate swappable object: **per-strategy named parameter sets**.
///
/// This is what the engine's hot-parameter registry and the IPC/audit surfaces
/// carry. It is a map rather than a struct precisely so that adding a strategy
/// (in-tree or external) never requires touching the kernel's parameter type.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MutableParams {
    by_strategy: BTreeMap<String, StrategyParams>,
}

impl MutableParams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_strategy.is_empty()
    }
    pub fn len(&self) -> usize {
        self.by_strategy.len()
    }
    pub fn strategies(&self) -> Vec<&str> {
        self.by_strategy.keys().map(|k| k.as_str()).collect()
    }
    pub fn for_strategy(&self, strategy: &str) -> Option<&StrategyParams> {
        self.by_strategy.get(strategy)
    }
    /// Value of one knob of one strategy (None when either is unknown).
    pub fn get(&self, strategy: &str, knob: &str) -> Option<Decimal> {
        self.by_strategy.get(strategy).and_then(|p| p.get(knob))
    }
    pub fn set_strategy(&mut self, strategy: &str, params: StrategyParams) {
        self.by_strategy.insert(strategy.to_string(), params);
    }
    pub fn remove_strategy(&mut self, strategy: &str) -> Option<StrategyParams> {
        self.by_strategy.remove(strategy)
    }
}

use rust_decimal::Decimal;

impl Serialize for MutableParams {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.by_strategy.len()))?;
        for (k, v) in &self.by_strategy {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for MutableParams {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = BTreeMap::<String, StrategyParams>::deserialize(d)?;
        Ok(Self { by_strategy: raw })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutable_params_is_per_strategy() {
        let mut p1 = StrategyParams::new();
        p1.set("trendMaxEntryPrice", rust_decimal_macros::dec!(0.45));
        let mut m = MutableParams::new();
        m.set_strategy("alpha_strategy", p1);
        assert_eq!(m.strategies(), vec!["alpha_strategy"]);
        assert_eq!(
            m.get("alpha_strategy", "trendMaxEntryPrice"),
            Some(rust_decimal_macros::dec!(0.45))
        );
        assert_eq!(m.get("spread_arb", "trendMaxEntryPrice"), None);
        let removed = m.remove_strategy("alpha_strategy");
        assert!(removed.is_some());
        assert!(m.is_empty());
    }

    #[test]
    fn mutable_params_wire_form_is_string_decimals() {
        let mut p = StrategyParams::new();
        p.set("trendMaxEntryPrice", rust_decimal_macros::dec!(0.45));
        let mut m = MutableParams::new();
        m.set_strategy("alpha_strategy", p);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"alpha_strategy":{"trendMaxEntryPrice":"0.45"}}"#);
        let back: MutableParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
