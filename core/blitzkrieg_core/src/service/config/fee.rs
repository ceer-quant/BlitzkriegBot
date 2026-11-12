//! The fee-quote readouts (#182, #203): the taker-fee model's single-spelling
//! quote family (`charged_per_share`, `charged_fee_per_share`, `fee_quote`)
//! and the round-duration readout — moved verbatim from `config.rs` (pure
//! code motion, no logic edits) per the file-size ratchet (#780).

use super::Core;
use crate::model::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ── Fee model (#182, #203) ───────────────────────────────────────────────────
//
// The taker fee was a formula copied into four places: the kernel's charge path
// and three gate/reconcile scripts. Copies cannot notice each other changing,
// and the failure is one-directional and quiet — the kernel switches its default
// schedule, the scripts keep asserting the old arithmetic, and a gate that
// passes stops meaning what it says. The scripts now take the per-share fee from
// `core.feeQuote` (below) and pin the DECLARED model, so a change here without a
// change there is a red gate instead of a wrong green one.
//
// The schedule itself lives in ONE place — `exit_policy::FeeSchedule`, whose
// `source` field names the publication each set of parameters comes from — and
// the quote below reports whatever that is, so a fee can never be charged under
// one name and declared under another. `official` (Polymarket's published
// `rate * p * (1 - p)`, exponent 1) is the succession path: switching the
// default means changing `exit_policy::legacy_quadratic_schedule` to it AND the
// pinned expectation in `scripts/lib/fee-model.mjs`, in the same change — which
// is the point. What that costs the strategies is measured, not argued:
// `scripts/fee-model-sensitivity-check.mjs` replays the frozen corpus under both
// schedules (#203).
//
// #234 finished the close-out. The curve's arithmetic has ONE spelling in the
// kernel — `FeeSchedule::fee_per_share` — so `fee_quote` no longer recomputes
// `rate * (p*(1-p))^exponent` beside it; `model_matches` now holds the charged
// fee against the PINNED parameters (`exit_policy::pinned_fee_parameters`), a
// source independent of the schedule it is checking, so a changed rate is
// reported instead of hiding behind two identical spellings. The authoritative
// cross-language self-check is `scripts/core-parity.mjs::assertPinnedFeeModel`
// against `scripts/lib/fee-model.mjs`; this one is the in-process half.

/// The fee actually charged per share at `price` under `schedule`:
/// `(schedule.fee_pct(p)/100) * p`, spelled exactly as the charge path spells it
/// (`apply_delta_effects`, `reconcile`), so a quote cannot drift from a fill.
/// Maker fills are exempt and are reported separately as zero.
pub(crate) fn charged_per_share(
    schedule: crate::exit_policy::FeeSchedule,
    price: Decimal,
) -> Decimal {
    (schedule.fee_pct(price) / Decimal::ONE_HUNDRED) * price
}

/// The fee actually charged per share at `price` under the schedule in force.
pub fn charged_fee_per_share(price: Decimal) -> Decimal {
    charged_per_share(crate::exit_policy::fee_schedule(), price)
}

/// Does the schedule in force still price what this repository PINS for its
/// declared name (#234, item 1)?
///
/// The pin is `exit_policy::pinned_fee_parameters` — deliberately NOT the
/// registry the schedule itself came from, because a check that reads the same
/// source as the thing it checks cannot fail: flipping the shipped rate from
/// 0.125 to 0.07 moved the declaration and the charge together and this reported
/// `true` while the fee moved 44%. Reading the pin makes that flip report
/// `false`, which is what the field was introduced to mean.
///
/// The pin is held against the schedule's PARAMETERS, not against the number one
/// sampled price happens to produce. `fee_quote` samples a single price (0.5
/// unless the caller asks otherwise) and two different `(rate, exponent)` pairs
/// can cross there: `0.03125*(p(1-p))^1` prices the pinned legacy curve's
/// `0.0078125` per share at p=0.5 and a different fee at every other price, so a
/// value-only comparison at the sampled point reports a wrong configuration as a
/// match (see `fee_quote_tests::model_matches_rejects_a_curve_that_only_coincides_at_the_sampled_price`).
/// Because both sides of that comparison run the same expression, it cannot see
/// a change to the expression itself either — what it can see is exactly the
/// parameters, which is what the pin describes. Drift in the arithmetic is
/// covered where the arithmetic lives (`exit_policy`'s own pricing tests) and by
/// the cross-language gate below, which recomputes the fee independently.
/// A schedule whose name this repository cannot describe is reported as NOT
/// matching, rather than as a green "nothing to check".
///
/// This is the in-process half. The authoritative, cross-language fee self-check
/// is `scripts/core-parity.mjs::assertPinnedFeeModel`, which holds this kernel's
/// `core.feeQuote` against the pinned table in `scripts/lib/fee-model.mjs` — the
/// two pins must be changed together, in the same change as the schedule.
pub(crate) fn schedule_reproduces_pin(schedule: crate::exit_policy::FeeSchedule) -> bool {
    let Some((rate, exponent)) = crate::exit_policy::pinned_fee_parameters(schedule.name) else {
        return false;
    };
    schedule.rate == rate && schedule.exponent == exponent
}

/// Read-only quote of the fee schedule (#182). `price` defaults to the widest
/// point of the schedule (0.5) so a caller that only wants the model metadata
/// does not have to invent a price.
pub fn fee_quote(price: Option<Decimal>) -> crate::schema::FeeQuoteResult {
    fee_quote_at(crate::exit_policy::fee_schedule(), price)
}

/// [`fee_quote`] with the schedule supplied, so a test can ask what a
/// DIFFERENT schedule would report — the check has to be able to say "no", and
/// the process-wide schedule can only be set once (#234).
pub(crate) fn fee_quote_at(
    schedule: crate::exit_policy::FeeSchedule,
    price: Option<Decimal>,
) -> crate::schema::FeeQuoteResult {
    let price = price.unwrap_or_else(|| dec!(0.5));
    let charged = charged_per_share(schedule, price);
    let model_matches = schedule_reproduces_pin(schedule);
    crate::schema::FeeQuoteResult {
        model: schedule.name.to_string(),
        rate: schedule.rate,
        exponent: schedule.exponent,
        price,
        fee_per_share: charged,
        fee_pct_of_price: schedule.fee_pct(price),
        maker_fee_per_share: Decimal::ZERO,
        model_matches,
    }
}

/// `round_duration.get` readout (`round_duration_readout`): assembled from the
/// LIVE config. Returns the default effective duration plus a per-account map
/// of `account_id → seconds`. Accounts absent from the map fall back to the
/// default. The `source` field reports the boot resolution of the default
/// grid; per-account overrides come from `ACCOUNT_<ID>_ROUND_DURATION_SEC`
/// env keys and are not separately sourced.
pub fn round_duration_readout(core: &Core) -> crate::schema::RoundDurationResult {
    use crate::schema::{RoundDurationOption, RoundDurationResult};
    let effective = core.config().default_round_duration_sec;
    let per_account: std::collections::BTreeMap<String, i64> = core
        .config()
        .account_round_durations
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    // The declared union the plugins own — the kernel hardcodes nothing
    // (whichever durations appear are exactly what the plugins claimed) and
    // the labels travel the same road (issue 648): the readout renders the
    // plugins' declared names, not a kernel-side decoder table.
    let mut allowed: Vec<RoundDurationOption> = core
        .config()
        .allowed_round_secs
        .iter()
        .map(|&sec| RoundDurationOption {
            sec,
            label: core.config().round_labels.get(&sec).copied(),
        })
        .collect();
    // A non-standard boot value (e.g. a test's 60s) is not in the declared
    // union — list it too, unlabeled, so the card can name what is actually
    // running instead of silently showing the nearest allowed value (and an
    // EMPTY union still answers with the effective value: the truth is
    // reported, even though every set would refuse).
    if !core.config().allowed_round_secs.contains(&effective) {
        allowed.push(RoundDurationOption {
            sec: effective,
            label: None,
        });
    }
    RoundDurationResult {
        effective_sec: effective,
        // 引擎扫描器实际运行的轮长：per-account 覆盖写入后已通过
        // `Engine::set_scanner_round_duration` 同步，故这里反映倒计时钟
        // 真实跑的网格，与 effective_sec 的 default 网格分离（#1151）。
        engine_active_sec: core
            .engine
            .as_ref()
            .map(|e| e.scanner_round_duration())
            .unwrap_or(effective),
        source: core.config().round_duration_source,
        per_account,
        allowed,
        next_session_sec: effective,
        persisted: false,
    }
}

/// One position selected by `Core::flatten`, in the order its destructuring
/// loop consumes: id, token, condition, shares, strategy, asset, direction,
/// current price, account id.
pub(crate) type FlattenTarget = (
    String,
    String,
    String,
    Decimal,
    String,
    String,
    String,
    Decimal,
    AccountId,
);

/// One due `maker_then_taker` order selected by `Core::tick`, in the order its
/// destructuring loop consumes: id, side, remaining, token, condition,
/// strategy, asset, direction, round slot, maker timeout, account id, and
/// (#963) whether the maker was a CLOSE — the escalated taker keeps the
/// close-intent status under its `exit-escalated:` key.
pub(crate) type EscalationTarget = (
    String,
    Side,
    Decimal,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    AccountId,
    bool,
);
