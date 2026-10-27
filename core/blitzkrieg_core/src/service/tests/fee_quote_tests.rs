#![cfg(test)]

use super::*;
use crate::exit_policy::{FeeSchedule, legacy_quadratic_schedule, official_schedule};

fn core_with(fee_schedule_replay: bool) -> Core {
    Core::new(CoreConfig {
        fee_schedule_replay,
        ..CoreConfig::default()
    })
}

/// #234, item 1 — the check has to be able to say NO. Both sides used to read
/// `fee_schedule()`, so changing the shipped rate moved the declaration and the
/// charge together and `model_matches` stayed `true` while the fee moved 44%.
/// Here the charge side is the shipped curve with exactly that rate change.
#[test]
fn model_matches_reports_a_changed_rate() {
    let shipped = legacy_quadratic_schedule();
    // The shipped curve is what this repository pins, so it matches...
    assert!(fee_quote_at(shipped, Some(dec!(0.5))).model_matches);
    // ...and 0.125 -> 0.07 on the same curve must not be reported as a match.
    let repriced = FeeSchedule {
        rate: dec!(0.07),
        ..shipped
    };
    assert_eq!(repriced.exponent, 2, "only the rate moves here");
    let quote = fee_quote_at(repriced, Some(dec!(0.5)));
    assert!(
        !quote.model_matches,
        "rate 0.125 -> 0.07 is a 44% fee change and the quote must say so"
    );
    // The quote still reports what it CHARGES: the flag is the split between
    // the charge and the pin, not a reason to hide the number.
    assert_eq!(quote.fee_per_share, dec!(0.004375));
    assert_eq!(quote.fee_pct_of_price, dec!(0.875));
    assert_eq!(quote.model, "legacy_quadratic");
}

/// #234, item 1 — a curve that only COINCIDES with the pinned one at the
/// sampled price is still not the pinned curve. `fee_quote` samples ONE price
/// (0.5 unless the caller asks otherwise), and different `(rate, exponent)`
/// pairs cross there: `0.03125*(p(1-p))^1` prices the pinned legacy curve's
/// `0.0078125` per share at p=0.5 and a different fee at every other price,
/// so a comparison made only at the sampled number reports a wrong
/// configuration as a match.
#[test]
fn model_matches_rejects_a_curve_that_only_coincides_at_the_sampled_price() {
    let pinned = legacy_quadratic_schedule();
    let crossed = FeeSchedule {
        rate: dec!(0.03125),
        exponent: 1,
        ..pinned
    };
    // The coincidence, stated as the arithmetic it is: the same number at
    // 0.5, a different one an eighth of a price away. Without both halves the
    // test would also pass on two curves that never cross at all.
    assert_eq!(
        charged_per_share(crossed, dec!(0.5)),
        charged_per_share(pinned, dec!(0.5)),
        "the two curves must actually cross at the sampled price"
    );
    assert_ne!(
        crossed.fee_per_share(dec!(0.375)),
        pinned.fee_per_share(dec!(0.375)),
        "…and must diverge elsewhere, or they are the same curve"
    );

    // It claims the pinned name while charging a different curve.
    assert_eq!(crossed.name, "legacy_quadratic");
    for p in [dec!(0.1), dec!(0.375), dec!(0.5), dec!(0.75), dec!(0.9)] {
        assert!(
            !fee_quote_at(crossed, Some(p)).model_matches,
            "a configuration that is not the pinned one must be refused at p={p}"
        );
    }
    // …and the refusal did not turn into "nothing ever matches".
    assert!(fee_quote_at(pinned, Some(dec!(0.5))).model_matches);
}

/// The exponent is a parameter of the pin too, and a schedule this repository
/// cannot describe is not one it can vouch for.
#[test]
fn model_matches_reports_a_changed_exponent_and_an_unknown_name() {
    let shipped = legacy_quadratic_schedule();
    let re_exponented = FeeSchedule {
        exponent: 1,
        ..shipped
    };
    assert!(!fee_quote_at(re_exponented, Some(dec!(0.5))).model_matches);
    let unknown = FeeSchedule {
        name: "not_a_schedule",
        ..shipped
    };
    assert!(
        !fee_quote_at(unknown, Some(dec!(0.5))).model_matches,
        "an undescribed schedule must report 'no', not 'nothing to check'"
    );
}

/// The production entry point reads the schedule in force and agrees with the
/// pinned numbers — the anchor #224's裁决 rests on: legacy @0.50 = 1.5625%.
#[test]
fn the_shipped_quote_matches_the_pin_and_the_anchor() {
    let quote = fee_quote(None);
    assert_eq!(quote.model, "legacy_quadratic");
    assert_eq!(quote.rate, dec!(0.125));
    assert_eq!(quote.exponent, 2);
    assert_eq!(quote.price, dec!(0.5));
    assert_eq!(quote.fee_per_share, dec!(0.0078125));
    assert_eq!(quote.fee_pct_of_price, dec!(1.5625));
    assert!(quote.model_matches);
    // The public helper the quote is built from spells the charge the same way.
    assert_eq!(charged_fee_per_share(dec!(0.5)), quote.fee_per_share);
    // A counterfactual curve is checked against the pin FOR ITS OWN NAME, so a
    // replay's official schedule matches here — `model_matches` answers "does
    // this curve price what this repository says that curve prices", not "is
    // this the shipped default". Which schedule is the default is a separate
    // assertion (`exit_policy::tests::default_fee_schedule_is_the_shipped_one`,
    // and PINNED_DEFAULT_MODEL on the gate side); confusing the two would make
    // this flag unable to answer either question.
    let official = fee_quote_at(official_schedule(), Some(dec!(0.5)));
    assert!(official.model_matches);
    assert_eq!(
        official.fee_pct_of_price,
        dec!(3.5),
        "official @0.50 = 3.5% of price (#224 anchor)"
    );
    assert_eq!(official.fee_per_share, dec!(0.0175));
}

/// #234, item 4 — the live charge sites' invariant: only a core that declared
/// itself a replay may charge anything but the shipped schedule. The predicate
/// is tested directly because the process-wide schedule is set-once, so a test
/// cannot install a second one.
#[test]
fn only_a_replay_may_charge_a_counterfactual_schedule() {
    let live = core_with(false);
    assert!(live.may_charge(legacy_quadratic_schedule()));
    assert!(
        !live.may_charge(official_schedule()),
        "a live core must refuse the counterfactual schedule"
    );
    let repriced = FeeSchedule {
        rate: dec!(0.07),
        ..legacy_quadratic_schedule()
    };
    assert!(
        !live.may_charge(repriced),
        "a repriced legacy curve keeps the name and is still not the shipped one"
    );
    let replay = core_with(true);
    assert!(replay.may_charge(official_schedule()));

    // The guard does not obstruct the shipped deployment: the live charge
    // sites still price the schedule in force (this call is the assert's
    // happy path, and would panic if the default config tripped it).
    assert_eq!(live.live_taker_fee_pct(dec!(0.5)), dec!(1.5625));
    assert_eq!(replay.live_taker_fee_pct(dec!(0.5)), dec!(1.5625));
}
