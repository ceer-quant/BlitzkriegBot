//! Shadow Evolution — the safety guard (hard, non-bypassable).
//!
//! Four locks, all enforced BEFORE any hot swap. This numbering is the canonical
//! one — `user_layer/configs/shadow_evolution.toml` and `docs/rust-core/
//! SHADOW_EVOLUTION.md` both defer to it (#269; they used to call the gradient
//! lock "Lock 1" and the immutable one "Lock 2", one off, because this line used
//! to say "three" while listing four):
//!   Lock 0 (declaration): only knobs the strategy DECLARED evolvable may carry
//!     a value. An undeclared name is not merely ignored — a proposal that
//!     mentions one is rejected, so nothing can be smuggled in through the
//!     parameter bag.
//!   Lock 1 (domain): every value must stay inside the domain the strategy
//!     declared for that knob. The domain is the OUTER bound: unlike the
//!     gradient lock it cannot be reached by any number of successive steps.
//!   Lock 2 (gradient): every declared field may change by at most
//!     `max_gradient` (default ±5%) in a single evolution step. Larger moves must
//!     be reached over multiple steps.
//!   Lock 3 (immutable): risk parameters (hard stop, breaker, daily loss,
//!     per-order notional) are NOT part of the swapped object — they are
//!     structural and therefore cannot be swapped. This function additionally
//!     asserts the physical constraints remain satisfied, so even a bug in
//!     variant generation cannot smuggle a weaker risk posture.

use super::config::ImmutableConfig;
use super::knobs::{KnobSpec, StrategyParams};
use rust_decimal::Decimal;

#[derive(Debug, Clone, PartialEq)]
pub enum GuardError {
    GradientTooLarge {
        field: String,
        change: Decimal,
        limit: Decimal,
    },
    DomainViolation {
        field: String,
        value: Decimal,
        min: Decimal,
        max: Decimal,
    },
    UndeclaredField {
        field: String,
    },
    ImmutableViolation {
        field: String,
        detail: String,
    },
    DegenerateParams {
        field: String,
        value: Decimal,
    },
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::GradientTooLarge {
                field,
                change,
                limit,
            } => {
                write!(f, "gradient too large for {field}: {change} > {limit}")
            }
            GuardError::DomainViolation {
                field,
                value,
                min,
                max,
            } => {
                write!(
                    f,
                    "out of domain for {field}: {value} not in [{min}, {max}]"
                )
            }
            GuardError::UndeclaredField { field } => {
                write!(
                    f,
                    "field {field} was not declared evolvable by this strategy"
                )
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

/// Lock 0: the proposal may only mention knobs the strategy declared. A knob that
/// was never declared is a rejection, not a silent no-op, because silently
/// dropping it would report "applied" for a change that never happened.
pub fn validate_declared(proposal: &StrategyParams, specs: &[KnobSpec]) -> Result<(), GuardError> {
    match proposal.undeclared(specs).into_iter().next() {
        Some(field) => Err(GuardError::UndeclaredField { field }),
        None => Ok(()),
    }
}

/// Lock 1: every value must sit inside its declared domain, and must be strictly
/// positive (a zero/negative knob is degenerate regardless of domain).
pub fn validate_domain(proposal: &StrategyParams, specs: &[KnobSpec]) -> Result<(), GuardError> {
    for spec in specs {
        let Some(v) = proposal.get(&spec.name) else {
            continue;
        };
        if v <= Decimal::ZERO {
            return Err(GuardError::DegenerateParams {
                field: spec.name.clone(),
                value: v,
            });
        }
        if !spec.contains(v) {
            return Err(GuardError::DomainViolation {
                field: spec.name.clone(),
                value: v,
                min: spec.min,
                max: spec.max,
            });
        }
    }
    Ok(())
}

/// Lock 2: per-field relative change must be within ±max_gradient.
///
/// Only fields present in BOTH sets are compared; a field the proposal does not
/// mention is unchanged by definition (and one it mentions must have passed
/// Lock 0 first).
pub fn validate_gradient(
    old: &StrategyParams,
    new: &StrategyParams,
    max_gradient: Decimal,
) -> Result<(), GuardError> {
    for (name, new_v) in new.iter() {
        let Some(old_v) = old.get(name) else { continue };
        if new_v <= Decimal::ZERO {
            return Err(GuardError::DegenerateParams {
                field: name.to_string(),
                value: new_v,
            });
        }
        if old_v == Decimal::ZERO {
            // Nothing to scale from; require exact equality to be safe.
            if new_v != old_v {
                return Err(GuardError::GradientTooLarge {
                    field: name.to_string(),
                    change: new_v,
                    limit: max_gradient,
                });
            }
            continue;
        }
        let change = ((new_v - old_v) / old_v).abs();
        if change > max_gradient {
            return Err(GuardError::GradientTooLarge {
                field: name.to_string(),
                change,
                limit: max_gradient,
            });
        }
    }
    Ok(())
}

/// Lock 3: assert the proposed mutable params cannot weaken risk. Because risk
/// lives outside the parameter object, this is a defense-in-depth assertion: the
/// immutable config is passed in only to confirm it is untouched and internally
/// consistent.
pub fn validate_immutable(risk: &ImmutableConfig) -> Result<(), GuardError> {
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

/// Step `from` toward `to` by at most `max_gradient` per field, then clamp into
/// the declared domains. Returns the nearest admissible parameters — this is how
/// a large target is reached over several evolutions instead of one jump.
pub fn clamped_step(
    from: &StrategyParams,
    to: &StrategyParams,
    max_gradient: Decimal,
    specs: &[KnobSpec],
) -> StrategyParams {
    let mut out = from.clone();
    for (name, target_v) in to.iter() {
        let old_v = from.get(name).unwrap_or(Decimal::ZERO);
        if old_v == Decimal::ZERO {
            out.set(name, target_v);
            continue;
        }
        let delta = target_v - old_v;
        let max_delta = old_v.abs() * max_gradient;
        let applied = if delta.abs() > max_delta {
            if delta > Decimal::ZERO {
                max_delta
            } else {
                -max_delta
            }
        } else {
            delta
        };
        out.set(name, old_v + applied);
    }
    out.clamp_to(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn specs() -> Vec<KnobSpec> {
        vec![
            KnobSpec::new("cap", dec!(0.45), dec!(0.05), dec!(0.90)),
            KnobSpec::new("factor", dec!(0.98), dec!(0.80), dec!(1.00)),
        ]
    }

    fn base() -> StrategyParams {
        StrategyParams::from_knobs(&specs())
    }

    fn scaled(f: Decimal) -> StrategyParams {
        base().scaled(f)
    }

    #[test]
    fn gradient_within_limit_passes() {
        assert!(validate_gradient(&base(), &scaled(dec!(1.04)), dec!(0.05)).is_ok());
    }

    #[test]
    fn gradient_over_limit_is_rejected() {
        match validate_gradient(&base(), &scaled(dec!(1.06)), dec!(0.05)) {
            Err(GuardError::GradientTooLarge { limit, .. }) => assert_eq!(limit, dec!(0.05)),
            other => panic!("expected gradient rejection, got {other:?}"),
        }
    }

    #[test]
    fn domain_is_the_outer_bound_no_step_count_can_cross() {
        // Walk 100 successive +5% steps: the gradient lock permits every one of
        // them individually, yet the domain must stop the value at its edge.
        let specs = specs();
        let mut p = base();
        for _ in 0..100 {
            let next = p.scaled(dec!(1.05));
            if validate_domain(&next, &specs).is_err() {
                p = next.clamp_to(&specs);
                break;
            }
            p = next;
        }
        // factor's ceiling is 1.00 → the walk must have stopped there.
        assert_eq!(
            p.get("factor"),
            Some(dec!(1.00)),
            "domain must bound the walk"
        );
        assert!(validate_domain(&p, &specs).is_ok());
        // And a direct jump far outside is refused with the domain named.
        let mut wild = base();
        wild.set("cap", dec!(0.99));
        match validate_domain(&wild, &specs) {
            Err(GuardError::DomainViolation { field, max, .. }) => {
                assert_eq!(field, "cap");
                assert_eq!(max, dec!(0.90));
            }
            other => panic!("expected a domain violation, got {other:?}"),
        }
    }

    #[test]
    fn undeclared_fields_are_rejected_not_ignored() {
        let mut p = base();
        p.set("hard_stop_loss_pct", dec!(0));
        assert!(matches!(
            validate_declared(&p, &specs()),
            Err(GuardError::UndeclaredField { .. })
        ));
        assert!(validate_declared(&base(), &specs()).is_ok());
    }

    #[test]
    fn degenerate_values_are_rejected() {
        let mut new = base();
        new.set("cap", dec!(0));
        assert!(matches!(
            validate_domain(&new, &specs()),
            Err(GuardError::DegenerateParams { .. })
        ));
    }

    #[test]
    fn immutable_risk_cannot_be_weakened() {
        let risk = ImmutableConfig {
            max_consecutive_losses: 0,
            ..Default::default()
        };
        assert!(matches!(
            validate_immutable(&risk),
            Err(GuardError::ImmutableViolation { .. })
        ));
        assert!(validate_immutable(&ImmutableConfig::default()).is_ok());
    }

    #[test]
    fn clamped_step_limits_a_large_jump_and_stays_in_domain() {
        let target = scaled(dec!(1.20)); // +20% target
        let stepped = clamped_step(&base(), &target, dec!(0.05), &specs());
        for (name, s) in stepped.iter() {
            let o = base().get(name).unwrap();
            let change = (s - o) / o;
            // Never more than the gradient...
            assert!(change <= dec!(0.05), "field {name} moved {change} > 5%");
            assert!(
                change > Decimal::ZERO,
                "field {name} must still step toward the target"
            );
            // ...and `factor`'s ceiling (1.00) is closer than +5%, so the DOMAIN
            // is what stops it. The domain is the outer bound; the gradient only
            // bounds an individual step.
            if name == "factor" {
                assert_eq!(s, dec!(1.00), "a ceiling nearer than the gradient wins");
            } else {
                assert_eq!(change, dec!(0.05), "field {name}");
            }
        }
        assert!(validate_gradient(&base(), &stepped, dec!(0.05)).is_ok());
        assert!(validate_domain(&stepped, &specs()).is_ok());
        // The target is only reachable over several steps, never in one.
        assert_ne!(stepped.get("cap"), target.get("cap"));
    }
}
