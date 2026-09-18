//! `spread_arb` (the dip buyer) — knob declarations, config overlays and the
//! tracker-config push. The DECISION logic is [`crate::signal`]'s
//! `TrendTracker` + `evaluate_spread_arb`; this module is the strategy's
//! parameter surface, shared verbatim by the kernel wrapper and every external
//! cdylib so declarations can never differ.

use crate::knobs::declare_knobs;
use crate::params::{KnobSpec, StrategyParams};
use crate::signal::{SpreadArbConfig, TrendConfig};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// The knobs `spread_arb` declares evolvable, as `(name, default, min, max)`.
/// See [`spread_arb_knobs`] for how the declaration is derived from the config
/// actually in force.
///
/// The domain is a HARD outer bound: the kernel's domain guard rejects anything
/// outside it before the ±gradient lock is even consulted, so no number of
/// successive steps can walk a knob past these edges. The bounds are wide (they
/// bracket the whole sensible trading range); the gradient lock is what keeps
/// any individual step small.
pub const SPREAD_ARB_KNOBS: [(&str, Decimal, Decimal, Decimal); 4] = [
    ("trend_min_price", dec!(0.55), dec!(0.50), dec!(0.95)),
    ("trend_entry_factor", dec!(0.98), dec!(0.80), dec!(1.00)),
    ("trend_max_entry_price", dec!(0.45), dec!(0.05), dec!(0.90)),
    ("trend_broken_price", dec!(0.35), dec!(0.01), dec!(0.50)),
];

fn knob_value(cfg: &SpreadArbConfig, name: &str) -> Decimal {
    match name {
        "trend_min_price" => cfg.trend_min_price,
        "trend_entry_factor" => cfg.trend_entry_factor,
        "trend_max_entry_price" => cfg.trend_max_entry_price,
        "trend_broken_price" => cfg.trend_broken_price,
        _ => Decimal::ZERO,
    }
}

/// This strategy's knob declaration derived from the config in force, so it
/// reports reality rather than a compiled-in default.
pub fn spread_arb_knobs(cfg: &SpreadArbConfig) -> Vec<KnobSpec> {
    declare_knobs(SPREAD_ARB_KNOBS, |name| knob_value(cfg, name))
}

/// Overlay a parameter set on a base config. Names this strategy did not declare
/// are ignored, so a stray key can never write a field it never opened.
pub fn apply_knobs(base: &SpreadArbConfig, params: &StrategyParams) -> SpreadArbConfig {
    let mut cfg = base.clone();
    for (name, v) in params.iter() {
        match name {
            "trend_min_price" => cfg.trend_min_price = v,
            "trend_entry_factor" => cfg.trend_entry_factor = v,
            "trend_max_entry_price" => cfg.trend_max_entry_price = v,
            "trend_broken_price" => cfg.trend_broken_price = v,
            _ => {}
        }
    }
    cfg
}

/// Overlay the two trend knobs on the TRACKER's config.
///
/// The confirmation floor (`min_price`) and the regime-break floor
/// (`broken_price`) are gates inside [`crate::signal::TrendTracker`], not fields
/// of [`SpreadArbConfig`] — pre-E2-c a variant's `trend_min_price` only changed
/// the entry reason string while the tracker kept confirming at the live floor,
/// so the knob was decision-neutral and any "evolution" of it measured nothing.
/// A twin (and the live instance once evolution is attached) now drives its own
/// tracker with the counterfactual value, which is what makes the declaration
/// honest.
pub fn apply_trend_knobs(base: &TrendConfig, params: &StrategyParams) -> TrendConfig {
    let mut cfg = base.clone();
    for (name, v) in params.iter() {
        match name {
            "trend_min_price" => cfg.min_price = v,
            "trend_broken_price" => cfg.broken_price = v,
            _ => {}
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_report_the_config_in_force_and_are_coherent() {
        let k = spread_arb_knobs(&SpreadArbConfig::default());
        assert_eq!(k.len(), 4);
        for spec in &k {
            assert!(spec.is_coherent(), "{spec:?}");
        }
        assert_eq!(k[0].name, "trend_min_price");
        assert_eq!(k[0].value, dec!(0.55));
        // An out-of-domain starting config widens the declared domain instead of
        // declaring a box the strategy cannot fit in.
        let cfg = SpreadArbConfig {
            trend_max_entry_price: dec!(0.99),
            ..Default::default()
        };
        let k = spread_arb_knobs(&cfg);
        let cap = k
            .iter()
            .find(|s| s.name == "trend_max_entry_price")
            .unwrap();
        assert!(cap.contains(dec!(0.99)));
        assert!(cap.is_coherent());
    }

    #[test]
    fn apply_knobs_ignores_undeclared_names() {
        let base = SpreadArbConfig::default();
        let mut p = StrategyParams::new();
        p.set("trend_max_entry_price", dec!(0.50));
        p.set("hard_stop_loss_pct", dec!(0)); // not ours
        let out = apply_knobs(&base, &p);
        assert_eq!(out.trend_max_entry_price, dec!(0.50));
        assert_eq!(out.trend_min_price, base.trend_min_price);
    }

    #[test]
    fn trend_knobs_reach_the_tracker_config() {
        let mut p = StrategyParams::new();
        p.set("trend_min_price", dec!(0.60));
        p.set("trend_broken_price", dec!(0.30));
        let out = apply_trend_knobs(&TrendConfig::default(), &p);
        assert_eq!(out.min_price, dec!(0.60));
        assert_eq!(out.broken_price, dec!(0.30));
        assert_eq!(out.confirm_sec, TrendConfig::default().confirm_sec);
    }
}
