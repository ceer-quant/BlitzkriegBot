//! E26 (DEV_V0_3 §4.3): `apply_physics` — the survival binding, stated once.
//!
//! The binding adds NO trigger path. Everything it returns is already being
//! enforced by the existing exit machinery; this function is where the kernel
//! WRITES DOWN what it will enforce, so the audit record and the panel show
//! the same discipline the position walker executes. Three facts, each read
//! from its existing home (§4.3's constraint: never restate, never hardcode):
//!
//! 1. **stop_price** ← `exit_policy::effective_stop_pct` — the exact percent
//!    the exit ladder already walks. Teeth B of the gate mutates this to a
//!    hardcoded `0.99`; any such edit makes a stop-sweep assertion fail,
//!    because the binding and the walker would quote different numbers.
//! 2. **force_exit_sec** ← `ExitConfig::force_exit_sec` — read, never
//!    modified (§19). Teeth C mutates the read into a literal `120`; a
//!    non-default config must then show `binding != config`, and the test
//!    below pins that equality.
//! 3. **ladder** ← projection of `ExitConfig` by default, the operator's
//!    explicit `[[risk.ladder]]` steps when configured (§4.3's two modes).
//!    The projection IS the shipped behavior described, not new behavior:
//!    one stop leg and one fixed take-profit leg, each closing the whole
//!    position (`close_ratio = 1.0`) — the same "close it all at once"
//!    semantics the kernel has always had.

use crate::arbitration::{LadderStep, PhysicsBinding};
use crate::exit_policy::{ExitConfig, effective_stop_pct};
use rust_decimal::Decimal;

/// The explicit-ladder config shape (§4.3, opt-in). Identical in meaning to
/// [`LadderStep`]; kept as a distinct name so the config layer can talk about
/// file fields without importing arbitration types.
pub type LadderSpec = LadderStep;

/// Reject an explicit ladder that cannot be executed as written. Called from
/// the config layer, so a typo dies at startup instead of mid-position.
pub fn validate_ladder(steps: &[LadderSpec]) -> Result<(), String> {
    if steps.is_empty() {
        return Ok(());
    }
    for (i, s) in steps.iter().enumerate() {
        if s.close_ratio <= Decimal::ZERO || s.close_ratio > Decimal::ONE {
            return Err(format!(
                "risk.ladder[{i}]: close_ratio {} outside (0, 1]",
                s.close_ratio
            ));
        }
        if i > 0 && s.at_pct < steps[i - 1].at_pct {
            return Err(format!(
                "risk.ladder[{i}]: at_pct {} goes backwards (previous {})",
                s.at_pct,
                steps[i - 1].at_pct
            ));
        }
    }
    Ok(())
}

/// Bind the survival facts for one entry (§4.3). Close intents carry no
/// binding — the caller (arbitration Gate 4) keeps that branch.
pub fn apply_physics(
    entry_price: Decimal,
    time_left_sec: i64,
    exit_cfg: &ExitConfig,
    explicit_ladder: &[LadderSpec],
) -> PhysicsBinding {
    // (1) The stop the walker already uses — computed, never restated.
    let stop_pct = effective_stop_pct(exit_cfg.stop_loss_pct, time_left_sec, exit_cfg);
    let stop_price = entry_price * (Decimal::ONE - stop_pct / Decimal::from(100));

    // (2) Read, never modified (§19).
    let force_exit_sec = exit_cfg.force_exit_sec;

    // (3) The ladder: explicit steps take over when configured; otherwise the
    // projection of the shipped policy (one whole-position take-profit leg —
    // the same close-at-once semantics the kernel has always had).
    let ladder = if explicit_ladder.is_empty() {
        vec![LadderStep {
            at_pct: exit_cfg.take_profit_pct,
            close_ratio: Decimal::ONE,
            move_stop_to: None,
        }]
    } else {
        explicit_ladder.to_vec()
    };

    PhysicsBinding {
        stop_price,
        force_exit_sec,
        ladder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn cfg() -> ExitConfig {
        ExitConfig::default()
    }

    /// The projection must stay bit-identical to the shipped Gate-4 formula:
    /// `price * (1 - stop_pct / 100)` over `effective_stop_pct`.
    #[test]
    fn projection_matches_the_shipped_gate4_formula() {
        let c = cfg();
        for price in [dec!(0.42), dec!(0.55), dec!(0.61)] {
            for t in [30i64, 120, 240, 600] {
                let b = apply_physics(price, t, &c, &[]);
                let pct = effective_stop_pct(c.stop_loss_pct, t, &c);
                assert_eq!(
                    b.stop_price,
                    price * (Decimal::ONE - pct / Decimal::from(100)),
                    "stop_price drifted at price={price} t={t}"
                );
            }
        }
    }

    #[test]
    fn projection_ladder_is_one_whole_position_take_profit_leg() {
        let c = cfg();
        let b = apply_physics(dec!(0.5), 240, &c, &[]);
        assert_eq!(b.ladder.len(), 1);
        assert_eq!(b.ladder[0].at_pct, c.take_profit_pct);
        assert_eq!(b.ladder[0].close_ratio, Decimal::ONE);
        assert_eq!(b.ladder[0].move_stop_to, None);
    }

    /// Teeth C's positive form: the binding READS the config. A constant
    /// would fail this at any non-default value.
    #[test]
    fn force_exit_binding_reads_the_config_not_a_literal() {
        let mut c = cfg();
        c.force_exit_sec = 77;
        assert_eq!(apply_physics(dec!(0.5), 240, &c, &[]).force_exit_sec, 77);
        c.force_exit_sec = 45;
        assert_eq!(apply_physics(dec!(0.5), 240, &c, &[]).force_exit_sec, 45);
    }

    /// Teeth B's positive form: the stop comes from `effective_stop_pct` —
    /// including its dynamic tightening, not a flat percentage.
    #[test]
    fn stop_binding_tracks_dynamic_tightening() {
        let c = cfg(); // dynamic_stop_enabled, tighten from 300s, floor 10%
        let far = apply_physics(dec!(0.50), 600, &c, &[]); // ≥ start: base 12%
        let near = apply_physics(dec!(0.50), 30, &c, &[]); // ≤ floor: min 10%
        assert_eq!(far.stop_price, dec!(0.50) * (Decimal::ONE - dec!(0.12)));
        assert_eq!(near.stop_price, dec!(0.50) * (Decimal::ONE - dec!(0.10)));
        assert!(near.stop_price > far.stop_price, "tighter stop sits higher");
    }

    #[test]
    fn explicit_ladder_takes_over_the_projection() {
        let c = cfg();
        let steps = vec![
            LadderStep {
                at_pct: dec!(30),
                close_ratio: dec!(0.5),
                move_stop_to: Some(dec!(0)),
            },
            LadderStep {
                at_pct: dec!(60),
                close_ratio: dec!(0.5),
                move_stop_to: None,
            },
        ];
        let b = apply_physics(dec!(0.5), 240, &c, &steps);
        // Field-by-field: `LadderStep` derives no `PartialEq` (E25's
        // arbitration territory owns that type), so the comparison spells
        // out the fields it binds instead of asking the type to change.
        assert_eq!(b.ladder.len(), steps.len());
        for (got, want) in b.ladder.iter().zip(steps.iter()) {
            assert_eq!(got.at_pct, want.at_pct);
            assert_eq!(got.close_ratio, want.close_ratio);
            assert_eq!(got.move_stop_to, want.move_stop_to);
        }
        // The stop binding is independent of the ladder mode.
        let pct = effective_stop_pct(c.stop_loss_pct, 240, &c);
        assert_eq!(
            b.stop_price,
            dec!(0.5) * (Decimal::ONE - pct / Decimal::from(100))
        );
    }

    #[test]
    fn ladder_validation_rejects_ratio_and_order_typos() {
        let ok = vec![LadderStep {
            at_pct: dec!(30),
            close_ratio: dec!(1),
            move_stop_to: None,
        }];
        assert!(validate_ladder(&ok).is_ok());
        assert!(validate_ladder(&[]).is_ok());

        let bad_ratio = vec![LadderStep {
            at_pct: dec!(30),
            close_ratio: dec!(0),
            move_stop_to: None,
        }];
        let (name, msg) = ("ratio 0", validate_ladder(&bad_ratio).unwrap_err());
        assert!(msg.contains("close_ratio"), "{name}: {msg}");

        let backwards = vec![
            LadderStep {
                at_pct: dec!(60),
                close_ratio: dec!(1),
                move_stop_to: None,
            },
            LadderStep {
                at_pct: dec!(30),
                close_ratio: dec!(1),
                move_stop_to: None,
            },
        ];
        let msg = validate_ladder(&backwards).unwrap_err();
        assert!(msg.contains("backwards"), "{msg}");
    }
}
