//! The taker-fee schedule (#182, #203), split verbatim out of `exit_policy`
//! (issue #963: the file-size ratchet #780). The module is re-exported from
//! [`crate::exit_policy`], so every external path —
//! `crate::exit_policy::fee_schedule()` and friends — is byte-identical.
//!
//! The fee is one number with two jobs: it is the accounting basis every gate
//! reconciles against, and it is a COST PARAMETER the strategies were tuned
//! under. #182 fixed the first job (the gates now read the kernel's own quote
//! instead of restating the formula). #203 is the second: changing the
//! schedule is a cost change of the same order as the strategy's whole edge,
//! so the question "what does this schedule cost the strategy" has to be
//! answerable with a measurement rather than an argument.
//!
//! That is what the schedule below exists for. It is a process-wide, SET-ONCE
//! value: the default is the shipped schedule and the live path never changes
//! it, while a replay may be run under a different one via `--fee-model` (the
//! CLI refuses that flag without `--backtest`, exactly like `--backtest-knob`).
//! Set-once rather than a config field on purpose — a fee that can move
//! between a fill and its reconciliation is an accounting hazard, and
//! threading a parameter through every call site would touch the charge path
//! for a knob no live run may use.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ── The taker-fee schedule (#182, #203) ─────────────────────────────────────
//
// The fee is one number with two jobs: it is the accounting basis every gate
// reconciles against, and it is a COST PARAMETER the strategies were tuned
// under. #182 fixed the first job (the gates now read the kernel's own quote
// instead of restating the formula). #203 is the second: changing the schedule
// is a cost change of the same order as the strategy's whole edge, so the
// question "what does this schedule cost the strategy" has to be answerable
// with a measurement rather than an argument.
//
// That is what the schedule below exists for. It is a process-wide, SET-ONCE
// value: the default is the shipped schedule and the live path never changes
// it, while a replay may be run under a different one via `--fee-model` (the
// CLI refuses that flag without `--backtest`, exactly like `--backtest-knob`).
// Set-once rather than a config field on purpose — a fee that can move between
// a fill and its reconciliation is an accounting hazard, and threading a
// parameter through every call site would touch the charge path for a knob no
// live run may use.

/// One taker-fee schedule: `fee_per_share = rate * (p*(1-p))^exponent` USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeeSchedule {
    /// Declared name, reported by `core.feeQuote` and pinned by the gates.
    pub name: &'static str,
    /// Coefficient of the schedule.
    pub rate: Decimal,
    /// Exponent of `p*(1-p)`.
    pub exponent: u32,
    /// Where the parameters come from — the thing a reader needs to decide
    /// whether a number is authoritative. Kept in the type so a new schedule
    /// cannot be added without saying who published it.
    pub source: &'static str,
}

impl FeeSchedule {
    /// This schedule's taker fee in USD PER SHARE — `rate * (p*(1-p))^exponent`,
    /// which is how Polymarket publishes it. THE one spelling of the curve in the
    /// kernel (#234, item 2): the percentage the charge path settles in, the
    /// per-share fee `fee_quote` reports, and every fee figure a test asserts are
    /// all derived from this, so a third curve shape cannot drift into a second
    /// implementation.
    ///
    /// A price outside `(0, 1)` charges nothing. The upper bound is not
    /// decoration: with `exponent >= 1` a price above 1 makes `p*(1-p)` NEGATIVE,
    /// so the fee would be a rebate — money moving the wrong way on a live fill.
    /// The deleted `FeeModel::PolymarketCrypto` clamped exactly here for exactly
    /// that reason, and the general method has to keep the guarantee. (`p == 1`
    /// is zero only for the accidental reason that the base is 0, and the legacy
    /// curve's `exponent = 2` hides the sign by squaring it.)
    pub fn fee_per_share(&self, price: Decimal) -> Decimal {
        if price <= Decimal::ZERO || price >= Decimal::ONE {
            return Decimal::ZERO;
        }
        let base = price * (Decimal::ONE - price);
        let mut acc = Decimal::ONE;
        for _ in 0..self.exponent {
            acc *= base;
        }
        self.rate * acc
    }

    /// The same fee as a PERCENTAGE OF THE FILL PRICE — the unit the charge path
    /// settles in (`fee_usd = pct/100 * price * shares`). Dividing the per-share
    /// form by `price` converts it, so both curves keep ONE accounting convention
    /// at every call site. The `price <= 0` guard is also what keeps the division
    /// safe; which prices are chargeable at all is decided in
    /// [`FeeSchedule::fee_per_share`].
    pub fn fee_pct(&self, price: Decimal) -> Decimal {
        if price <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        (self.fee_per_share(price) / price) * Decimal::ONE_HUNDRED
    }
}

/// The schedule the deployment line charges: `0.125*(p*(1-p))^2`. Its
/// provenance is NOT a published schedule (see `source`); it is the value the
/// kernel has always charged, and #203's finding is that it is 2.3x-5.3x
/// CHEAPER than the published crypto schedule at the prices traded.
pub fn legacy_quadratic_schedule() -> FeeSchedule {
    FeeSchedule {
        name: "legacy_quadratic",
        rate: dec!(0.125),
        exponent: 2,
        source: "history: unchanged since the fee was first charged; no external \
                 publication states this curve (see #203, fee-model.mjs)",
    }
}

/// Polymarket's published crypto taker schedule, `0.07 * p * (1-p)` USD per
/// share. Primary source (quoted in `scripts/lib/fee-model.mjs`, which is what
/// the gates read): Polymarket docs, fees page — `fee = C * feeRate * p * (1-p)`
/// with `feeRate = 0.07` for the Crypto category, maker side 0.
pub fn official_schedule() -> FeeSchedule {
    FeeSchedule {
        name: "official",
        rate: dec!(0.07),
        exponent: 1,
        source: "Polymarket docs, fees: fee = C x feeRate x p x (1-p); Crypto feeRate = 0.07, maker 0",
    }
}

/// Every schedule a replay may be asked for, by name.
pub const FEE_SCHEDULE_NAMES: &[&str] = &["legacy_quadratic", "official"];

/// Look a schedule up by its declared name.
pub fn fee_schedule_by_name(name: &str) -> Option<FeeSchedule> {
    match name {
        "legacy_quadratic" => Some(legacy_quadratic_schedule()),
        "official" => Some(official_schedule()),
        _ => None,
    }
}

/// The `(rate, exponent)` this repository PINS for a schedule name (#234, item 1).
///
/// Deliberately a SECOND, hand-maintained copy of the numbers the constructors
/// above build — not a view onto them. It is the only thing that lets the kernel
/// answer, in-process and without node: "is the curve I am charging still the
/// curve this repository believes it charges?" A check that read the registry
/// would answer `yes` by construction: flipping `legacy_quadratic`'s rate from
/// 0.125 to 0.07 moves the declaration and the charge together and the check
/// stays green while the fee moves 44% (`service.rs::fee_quote`). Reading the pin
/// instead makes that same flip report `modelMatches: false`.
///
/// So changing a schedule's parameters means changing them here too, deliberately,
/// in the same change as `scripts/lib/fee-model.mjs` (whose `TAKER_FEE_MODELS` is
/// the cross-language pin the gates read). Do NOT add a test asserting this table
/// equals the registry: that equality is exactly what the check must be able to
/// lose. The authoritative, cross-language fee self-check remains
/// `scripts/core-parity.mjs::assertPinnedFeeModel`, which holds the kernel's
/// `core.feeQuote` against `scripts/lib/fee-model.mjs`.
pub fn pinned_fee_parameters(name: &str) -> Option<(Decimal, u32)> {
    match name {
        "legacy_quadratic" => Some((dec!(0.125), 2)),
        "official" => Some((dec!(0.07), 1)),
        _ => None,
    }
}

/// The schedule in force. Defaults to the shipped one; only a replay changes it.
static ACTIVE_FEE_SCHEDULE: std::sync::OnceLock<FeeSchedule> = std::sync::OnceLock::new();

/// The taker-fee schedule the kernel is charging right now.
pub fn fee_schedule() -> FeeSchedule {
    *ACTIVE_FEE_SCHEDULE
        .get()
        .unwrap_or(&legacy_quadratic_schedule_ref())
}

/// `OnceLock<FeeSchedule>` needs a `'static` reference; `legacy_quadratic_schedule()`
/// builds a fresh value, so the default is memoised here instead.
fn legacy_quadratic_schedule_ref() -> FeeSchedule {
    static DEFAULT: std::sync::OnceLock<FeeSchedule> = std::sync::OnceLock::new();
    *DEFAULT.get_or_init(legacy_quadratic_schedule)
}

/// Install a schedule for this process. Refuses a second call: set-once is the
/// point (a fee that moves mid-run cannot be reconciled against), and it means
/// the only way to change what is charged is to restart with a different flag.
pub fn set_fee_schedule(schedule: FeeSchedule) -> Result<(), String> {
    ACTIVE_FEE_SCHEDULE
        .set(schedule)
        .map_err(|_| "the taker-fee schedule is already set for this process".to_string())
}

/// The taker fee in force, as a percentage of the fill price: the active
/// schedule's own arithmetic ([`FeeSchedule::fee_pct`]). One owner for the
/// accounting basis every gate reconciles against and the cost parameter the
/// strategies were tuned under (#182, #203).
pub fn taker_fee_pct(price: Decimal) -> Decimal {
    // The price bounds are owned by `FeeSchedule::fee_pct` — restating them here
    // would be a second place for the charge policy to drift apart (#234).
    fee_schedule().fee_pct(price)
}
