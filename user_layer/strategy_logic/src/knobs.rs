//! Knob declaration helpers shared by the three strategies' `*_knobs()` functions.
//!
//! The declaration pattern is uniform: a const table of
//! `(name, default, min, max)`, the value in force read off the config, and a
//! domain that WIDENS to include the value in force (a config that starts
//! outside the default domain would otherwise make every proposal a violation).

use crate::params::KnobSpec;
use rust_decimal::Decimal;

/// Build one strategy's knob declaration from a `(name, _default, min, max)`
/// table plus a closure reading the value in force. See the module docs for the
/// widening rule.
pub fn declare_knobs<const N: usize>(
    table: [(&str, Decimal, Decimal, Decimal); N],
    value_in_force: impl Fn(&str) -> Decimal,
) -> Vec<KnobSpec> {
    table
        .iter()
        .map(|(name, _default, min, max)| {
            let v = value_in_force(name);
            // A config that starts outside the default domain widens it to
            // include the value in force: declaring a box the strategy does not
            // fit in would make every proposal a domain violation.
            KnobSpec::new(*name, v, (*min).min(v), (*max).max(v))
        })
        .collect()
}
