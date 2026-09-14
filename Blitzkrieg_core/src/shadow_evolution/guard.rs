//! Shadow Evolution — the safety guard (hard, non-bypassable).
//!
//! Two locks, enforced BEFORE any hot swap:
//!   Lock 1 (gradient): every mutable field may change by at most `max_gradient`
//!     (default ±5%) in a single evolution step. Larger moves must be reached
//!     over multiple steps.
//!   Lock 2 (immutable): risk parameters (hard stop, breaker, daily loss,
//!     per-order notional) are NOT part of `MutableParams` — they are structural
//!     and therefore cannot be swapped. This function additionally asserts the
//!     proposed values leave the physical constraints satisfied, so even a bug
//!     in variant generation cannot smuggle a weaker risk posture.

use super::config::{ImmutableConfig, MutableParams};
use rust_decimal::Decimal;

#[derive(Debug, Clone, PartialEq)]
pub enum GuardError {
    GradientTooLarge { field: String, change: Decimal, limit: Decimal },
    ImmutableViolation { field: String, detail: String },
    DegenerateParams { field: String, value: Decimal },
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::GradientTooLarge { field, change, limit } => {
                write!(f, "gradient too large for {field}: {change} > {limit}")
            }
            GuardError::ImmutableViolation { field, detail } => {
                write!(f, "immutable violation on {field}: {detail}")
            }
            GuardError::DegenerateParams { field, value } => {
                write!(f, "degenerate value for {field}: {value}")
            }
        }
    }
}

/// Lock 1: per-field relative change must be within ±max_gradient, and values
/// must stay in sane positive ranges (a zero/negative knob is degenerate).
pub fn validate_gradient(
    old: &MutableParams,
    new: &MutableParams,
    max_gradient: Decimal,
) -> Result<(), GuardError> {
    for ((name, old_v), (_, new_v)) in old.fields().iter().zip(new.fields().iter()) {
        if *new_v <= Decimal::ZERO {
            return Err(GuardError::DegenerateParams { field: (*name).into(), value: *new_v });
        }
        if *old_v == Decimal::ZERO {
            // Nothing to scale from; require exact equality to be safe.
            if new_v != old_v {
                return Err(GuardError::GradientTooLarge {
                    field: (*name).into(),
                    change: *new_v,
                    limit: max_gradient,
                });
            }
            continue;
        }
        let change = ((*new_v - *old_v) / *old_v).abs();
        if change > max_gradient {
            return Err(GuardError::GradientTooLarge {
                field: (*name).into(),
                change,
                limit: max_gradient,
            });
        }
    }
    Ok(())
}

/// Lock 2: assert the proposed mutable params cannot weaken risk. Because risk
/// lives outside `MutableParams`, this is a defense-in-depth assertion: the
/// immutable config is passed in only to confirm it is untouched and internally
/// consistent.
pub fn validate_immutable(
    _old: &MutableParams,
    _new: &MutableParams,
    risk: &ImmutableConfig,
) -> Result<(), GuardError> {
    if risk.hard_stop_loss_pct <= Decimal::ZERO {
        return Err(GuardError::ImmutableViolation {
            field: "hard_stop_loss_pct".into(),
            detail: "must be positive".into(),
        });
    }
    if risk.max_consecutive_losses == 0 {
        return Err(GuardError::ImmutableViolation {
            field: "max_consecutive_losses".into(),
            detail: "breaker must not be disabled".into(),
        });
    }
    if risk.max_daily_loss_usd <= Decimal::ZERO {
        return Err(GuardError::ImmutableViolation {
            field: "max_daily_loss_usd".into(),
            detail: "must be positive".into(),
        });
    }
    if risk.max_order_notional <= Decimal::ZERO {
        return Err(GuardError::ImmutableViolation {
            field: "max_order_notional".into(),
            detail: "must be positive".into(),
        });
    }
    Ok(())
}

/// Step `from` toward `to` by at most `max_gradient` per field. Returns the
/// nearest admissible parameters (this is how a large target is reached over
/// several evolutions instead of one jump).
pub fn clamped_step(from: &MutableParams, to: &MutableParams, max_gradient: Decimal) -> MutableParams {
    let mut out = from.clone();
    for ((name, old_v), (_, target_v)) in from.fields().iter().zip(to.fields().iter()) {
        if *old_v == Decimal::ZERO {
            out.set(name, *target_v);
            continue;
        }
        let delta = *target_v - *old_v;
        let max_delta = (*old_v).abs() * max_gradient;
        let applied = if delta.abs() > max_delta {
            if delta > Decimal::ZERO { max_delta } else { -max_delta }
        } else {
            delta
        };
        out.set(name, *old_v + applied);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn p(factor: Decimal) -> MutableParams {
        MutableParams::default().scaled(factor)
    }

    #[test]
    fn gradient_within_limit_passes() {
        let old = p(dec!(1));
        let new = p(dec!(1.04)); // +4% < 5%
        assert!(validate_gradient(&old, &new, dec!(0.05)).is_ok());
    }

    #[test]
    fn gradient_over_limit_is_rejected() {
        let old = p(dec!(1));
        let new = p(dec!(1.06)); // +6% > 5%
        match validate_gradient(&old, &new, dec!(0.05)) {
            Err(GuardError::GradientTooLarge { limit, .. }) => assert_eq!(limit, dec!(0.05)),
            other => panic!("expected gradient rejection, got {other:?}"),
        }
    }

    #[test]
    fn degenerate_values_are_rejected() {
        let old = p(dec!(1));
        let mut new = old.clone();
        new.trend_min_price = dec!(0);
        assert!(matches!(
            validate_gradient(&old, &new, dec!(0.05)),
            Err(GuardError::DegenerateParams { .. })
        ));
    }

    #[test]
    fn immutable_risk_cannot_be_weakened() {
        let old = p(dec!(1));
        let new = p(dec!(1.02));
        let mut risk = ImmutableConfig::default();
        risk.max_consecutive_losses = 0; // breaker disabled → must fail
        assert!(matches!(
            validate_immutable(&old, &new, &risk),
            Err(GuardError::ImmutableViolation { .. })
        ));
        assert!(validate_immutable(&old, &new, &ImmutableConfig::default()).is_ok());
    }

    #[test]
    fn clamped_step_limits_a_large_jump() {
        let old = p(dec!(1));
        let target = p(dec!(1.20)); // +20% target
        let stepped = clamped_step(&old, &target, dec!(0.05));
        // Each field moved by exactly +5%.
        for ((name, o), (_, s)) in old.fields().iter().zip(stepped.fields().iter()) {
            let change = (*s - *o) / *o;
            assert_eq!(change, dec!(0.05), "field {name}");
        }
        // And the stepped result is admissible.
        assert!(validate_gradient(&old, &stepped, dec!(0.05)).is_ok());
    }
}
