//! `spread_arb` (the dip buyer) — knob declarations, config overlays and the
//! tracker-config push. The DECISION logic is [`crate::signal`]'s
//! `TrendTracker` + `evaluate_spread_arb`; this module is the strategy's
//! parameter surface, shared verbatim by the kernel wrapper and every external
//! cdylib so declarations can never differ.

use crate::knobs::declare_knobs;
use crate::params::{KnobSpec, StrategyParams};
use crate::signal::{SpreadArbConfig, TrendConfig};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
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
///
/// NOTE on `trend_entry_factor`: the shipped 0.88 is the tuned anti-adverse-
/// selection discount — a resting bid this far under the mid only fills on a
/// real flush, which is where the win rate comes from (see
/// `SpreadArbConfig::default` for the tuning note). The declared floor is 0.80,
/// so evolution can still walk it to the deepest legal discount.
///
/// The `entry_*` HFT filters (`entry_min_obi`, `entry_max_spread_pct`,
/// `entry_dip_max_pct`, `entry_bounce_min_pct`, `entry_bounce_window_sec`) are
/// deliberately NOT declared here: they ship OFF (0), and the evolution guard
/// requires every declared knob to hold a strictly positive value. They remain
/// first-class config — settable through the `on_params` hot bag, applied by
/// [`apply_knobs`], reported by the strategy's `config_view` — and can be
/// declared evolvable later with positive tuned defaults if a session adopts
/// them.
pub const SPREAD_ARB_KNOBS: [(&str, Decimal, Decimal, Decimal); 4] = [
    ("trend_min_price", dec!(0.55), dec!(0.50), dec!(0.95)),
    ("trend_entry_factor", dec!(0.88), dec!(0.80), dec!(1.00)),
    ("trend_max_entry_price", dec!(0.45), dec!(0.05), dec!(0.90)),
    ("trend_broken_price", dec!(0.35), dec!(0.01), dec!(0.50)),
];

fn knob_value(cfg: &SpreadArbConfig, name: &str) -> Decimal {
    match name {
        "trend_min_price" => cfg.trend_min_price,
        "trend_entry_factor" => cfg.trend_entry_factor,
        "trend_max_entry_price" => cfg.trend_max_entry_price,
        "trend_broken_price" => cfg.trend_broken_price,
        "entry_min_obi" => cfg.entry_min_obi,
        "entry_max_spread_pct" => cfg.entry_max_spread_pct,
        "entry_dip_max_pct" => cfg.entry_dip_max_pct,
        "entry_bounce_min_pct" => cfg.entry_bounce_min_pct,
        "entry_bounce_window_sec" => Decimal::from(cfg.entry_bounce_window_sec),
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
            "entry_min_obi" => cfg.entry_min_obi = v,
            "entry_max_spread_pct" => cfg.entry_max_spread_pct = v,
            "entry_dip_max_pct" => cfg.entry_dip_max_pct = v,
            "entry_bounce_min_pct" => cfg.entry_bounce_min_pct = v,
            // The window is a whole number of seconds in the config but travels
            // as a decimal like every other knob; truncate so a fractional
            // proposal can never produce a non-integral window.
            "entry_bounce_window_sec" => {
                if let Some(secs) = v.trunc().to_i64()
                    && secs > 0
                {
                    cfg.entry_bounce_window_sec = secs;
                }
            }
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
            // The evolution guard requires strictly positive knob values.
            assert!(spec.value > Decimal::ZERO, "{spec:?}");
        }
        assert_eq!(k[0].name, "trend_min_price");
        assert_eq!(k[0].value, dec!(0.55));
        // The tuned anti-adverse-selection discount ships at 0.88, inside the
        // declared domain whose floor is the deepest legal discount.
        let factor = k.iter().find(|s| s.name == "trend_entry_factor").unwrap();
        assert_eq!(factor.value, dec!(0.88));
        assert_eq!(factor.min, dec!(0.80));
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
    fn apply_knobs_ignores_undeclared_names_and_keeps_the_window_integral() {
        let base = SpreadArbConfig::default();
        let mut p = StrategyParams::new();
        p.set("trend_max_entry_price", dec!(0.50));
        p.set("entry_min_obi", dec!(0.30));
        p.set("entry_bounce_min_pct", dec!(2));
        p.set("entry_bounce_window_sec", dec!(7.9));
        p.set("hard_stop_loss_pct", dec!(0)); // not ours
        let out = apply_knobs(&base, &p);
        assert_eq!(out.trend_max_entry_price, dec!(0.50));
        assert_eq!(out.entry_min_obi, dec!(0.30));
        assert_eq!(out.entry_bounce_min_pct, dec!(2));
        assert_eq!(
            out.entry_bounce_window_sec, 7,
            "a fractional window truncates"
        );
        assert_eq!(out.trend_min_price, base.trend_min_price);
        // A non-positive window can never be written through the overlay.
        let mut bad = StrategyParams::new();
        bad.set("entry_bounce_window_sec", dec!(0));
        assert_eq!(
            apply_knobs(&base, &bad).entry_bounce_window_sec,
            base.entry_bounce_window_sec
        );
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
