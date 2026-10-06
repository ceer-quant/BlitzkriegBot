//! Exit policy — pure, replayable exit decisions (Rust port of
//! `src/strategies/crypto-hft/exit-policy.ts`, deleted with the Node source
//! layer in `62b16c88`).
//!
//! Single source of truth for exit logic. The TS original this was ported from
//! no longer exists, so the "mirror" is gone and this is the only
//! implementation — offline replay goes through the core. Triggers and the
//! high-water mark are valued on the EXECUTABLE bid (what a long can be sold
//! for), never the mid: a token can show mid 0.82 while the bid is 0.59.
//!
//! Protective stops additionally require the bid not to be a dislocated wick
//! (bid within maxBidWickPct of mid); profit-side triggers don't need that.
//! The guard judges on the mid when the bid is a wick and on the last known
//! price when the book is dark — it must never be the reason a stop is skipped
//! in a waterfall (P0 #177): every trigger it withholds is reported as a
//! [`SuppressedStop`], and every exit evaluation carries that back to the
//! caller in an [`ExitVerdict`].

use crate::model::{ExitReason, OrderbookSnapshot};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Issue #390: the scalar book surface the exit path reads, implemented by
/// both the full [`OrderbookSnapshot`] and the allocation-free [`BookView`]
/// so every function below can take either. `best_live_ask` must see the
/// emptiness flags, not just the sentinel-carrying top — a snapshot with a
/// real ask priced at exactly 1.00 is NOT the same as no ask side.
pub trait BookScalarView {
    fn best_bid(&self) -> Decimal;
    fn best_ask(&self) -> Decimal;
    fn mid_price(&self) -> Decimal;
    fn bid_depth(&self) -> Decimal;
    fn ask_depth(&self) -> Decimal;
    fn timestamp(&self) -> i64;
    fn has_bids(&self) -> bool;
    fn has_asks(&self) -> bool;
}

impl BookScalarView for OrderbookSnapshot {
    fn best_bid(&self) -> Decimal {
        self.best_bid
    }
    fn best_ask(&self) -> Decimal {
        self.best_ask
    }
    fn mid_price(&self) -> Decimal {
        self.mid_price
    }
    fn bid_depth(&self) -> Decimal {
        self.bid_depth
    }
    fn ask_depth(&self) -> Decimal {
        self.ask_depth
    }
    fn timestamp(&self) -> i64 {
        self.timestamp
    }
    fn has_bids(&self) -> bool {
        !self.bids.is_empty()
    }
    fn has_asks(&self) -> bool {
        !self.asks.is_empty()
    }
}

impl BookScalarView for BookView {
    fn best_bid(&self) -> Decimal {
        self.best_bid
    }
    fn best_ask(&self) -> Decimal {
        self.best_ask
    }
    fn mid_price(&self) -> Decimal {
        self.mid_price
    }
    fn bid_depth(&self) -> Decimal {
        self.bid_depth
    }
    fn ask_depth(&self) -> Decimal {
        self.ask_depth
    }
    fn timestamp(&self) -> i64 {
        self.timestamp
    }
    fn has_bids(&self) -> bool {
        self.has_bids
    }
    fn has_asks(&self) -> bool {
        self.has_asks
    }
}

/// Exit-relevant config subset. Mirrors fields read by the TS `decideExit`.
#[derive(Debug, Clone)]
pub struct ExitConfig {
    pub force_exit_sec: i64,
    pub min_time_left_sec: i64,
    pub take_profit_pct: Decimal,
    pub stop_loss_pct: Decimal,
    pub simple_exit_enabled: bool,
    pub dynamic_stop_enabled: bool,
    pub stop_tighten_start_sec: i64,
    pub stop_min_pct: Decimal,
    pub trailing_enabled: bool,
    pub trailing_min_high_pct: Decimal,
    pub min_trail_pct: Decimal,
    pub proportional_trail_enabled: bool,
    pub proportional_trail_pct: Decimal,
    pub proportional_trail_min_pct: Decimal,
    pub proportional_trail_min_giveback_pct: Decimal,
    pub tight_stop_enabled: bool,
    pub tight_stop_pct: Decimal,
    pub ratchet_enabled: bool,
    pub ratchet_confirm_ticks: u32,
    pub ratchet_confirm_tolerance_pct: Decimal,
    pub max_bid_wick_pct: Decimal,
    /// How old the last known price may be to stand in for a live quote. A
    /// trigger judged on a price older than this has nothing honest to stand on
    /// and stays silent (P0 #177).
    ///
    /// This is the JUDGEMENT window and BOTH sides of the ladder read it: a
    /// protective stop may act on a remembered price inside it (#177), and a
    /// profit-side rule may judge on one inside it too (#267 — before that the
    /// profit side had no fallback at all and went blind the moment the buy side
    /// emptied). One number for both sides is the point: the two can never
    /// disagree about what the market is doing. It never prices an ORDER — see
    /// [`executable_bid`].
    pub max_last_price_age_sec: i64,
    pub stale_profit_pct: Decimal,
    pub stale_profit_bid_unchanged_sec: i64,
    pub stagnant_profit_pct: Decimal,
    pub stagnant_duration_sec: i64,
    pub depth_collapse_threshold_pct: Decimal,
    pub exit_grace_sec: i64,
    pub maker_exits_for_tp_only: bool,
    pub maker_first_exit_enabled: bool,
    /// F6: a book older than this (sec, measured against the tick's `now_ms`)
    /// must not price an exit — a stale quote is not an executable one.
    /// Snapshots with `timestamp <= 0` (synthetic tests, legacy records) are
    /// exempt because their age cannot be judged (see [`book_age_ms`]: that
    /// exemption is a deliberate choice, not an accident, and it is asserted in
    /// this module's tests). Live service books are rebuilt with
    /// `timestamp = now_ms` and are therefore always "fresh" at this layer; real
    /// receive-time preservation happens upstream.
    ///
    /// This is the PRICING window, and it is the same concept as
    /// `max_last_price_age_sec` (the JUDGEMENT window) at twice the budget
    /// (#268): a book still carries real LEVELS a remembered number has nothing
    /// behind, so a book may be read for longer. Both defaults are derived from
    /// [`QUOTE_TRUST_SEC`], so "how long may a quote speak for the market" has
    /// ONE answer, and the 30–60 s band where a stop could still act while the
    /// profit side was already blind is stated here rather than implied.
    pub max_book_age_sec: i64,
}

/// The one "how long may a quote still speak for the market" budget the two
/// windows below are derived from (#268, item 3). It is the JUDGEMENT window:
/// how old a remembered price may be before neither a stop nor a profit rule may
/// judge on it.
const QUOTE_TRUST_SEC: i64 = 30;

/// The PRICING window: how old a book may be before its levels stop being
/// quotable. Twice the judgement budget, because a book carries real levels
/// while a remembered price has no one standing behind it.
const BOOK_TRUST_SEC: i64 = QUOTE_TRUST_SEC * 2;

impl Default for ExitConfig {
    fn default() -> Self {
        // Calibrated against the one proven-profitable on-chain operator the
        // replay was ever pointed at (@almach, 85k fills, SELL=0 — every exit
        // is a REDEEM or a MERGE): the profit side HOLDS to settlement and the
        // only in-flight exits are protective. Concretely:
        //   * force_exit_sec 0 = the guillotine is OFF (0 disables the rule) —
        //     the old 120s deadline amputated rounds that went on to pay;
        //   * min_time_left_sec 0 = the time exit fires only at expiry itself,
        //     i.e. "time exit" and "settlement" are the same event;
        //   * trailing_enabled false = no profit-banking sells (SELL=0); the
        //     fixed TP 100% backstop stays as the only profit-side rule.
        // The stop half is untouched (12% hard stop, tighten ramp) — containment
        // is what the ladder must keep doing. A caller that wants the pre-
        // calibration behaviour pins the old values explicitly (the tests do).
        Self {
            force_exit_sec: 0,
            min_time_left_sec: 0,
            // Fixed take-profit is a BACKSTOP only, matching Node (takeProfitPct=100):
            // normal profits are locked by the trailing stop below, while this guard
            // banks a near-certain binary win (+100%, e.g. 0.45 -> 0.90) if it ever
            // runs that far. An aggressive fixed TP (e.g. 20%) would cut winners
            // short and hand the profit-taking job back to a static level.
            take_profit_pct: dec!(100),
            // Was 50. A 50% stop meant a binary contract (entry ~0.40-0.45) could
            // ride to ~0.20 before exiting — a ~$2.4 loss vs a ~$0.45 median win.
            // 12% caps the loss near $0.55; the canonical exit sweep over 62
            // path-recorded trades puts SL12 as the profit/PF sweet spot.
            stop_loss_pct: dec!(12),
            simple_exit_enabled: true,
            dynamic_stop_enabled: true,
            stop_tighten_start_sec: 300,
            stop_min_pct: dec!(10),
            // Calibrated: the proven operator NEVER sells into profit (SELL=0,
            // every exit is a REDEEM/MERGE) — trailing banking is a counter-
            // factual behaviour the ladder must not default to. Callers that
            // want it set the flag explicitly.
            trailing_enabled: false,
            trailing_min_high_pct: dec!(15),
            // Node uses 10. A canonical sweep shows 8 locks profit slightly sooner
            // and raises net at essentially unchanged PF; 6 gains only ~$0.5 more
            // (within noise) at a bigger deviation from Node, so 8 is the pick.
            min_trail_pct: dec!(8),
            proportional_trail_enabled: true,
            proportional_trail_pct: dec!(15),
            proportional_trail_min_pct: dec!(15),
            proportional_trail_min_giveback_pct: dec!(3),
            tight_stop_enabled: true,
            tight_stop_pct: dec!(12),
            ratchet_enabled: false,
            ratchet_confirm_ticks: 3,
            ratchet_confirm_tolerance_pct: dec!(0.5),
            max_bid_wick_pct: dec!(8),
            // The judgement window, and now the profit side's fallback window
            // too (#267): a quote a few multiples older than the engine's 8s
            // entry budget may still JUDGE an exit rather than nothing at all
            // (see `stop_reference` and `profit_reference`).
            max_last_price_age_sec: QUOTE_TRUST_SEC,
            stale_profit_pct: dec!(20),
            stale_profit_bid_unchanged_sec: 10,
            stagnant_profit_pct: dec!(5),
            stagnant_duration_sec: 30,
            depth_collapse_threshold_pct: dec!(60),
            exit_grace_sec: 3,
            maker_exits_for_tp_only: false,
            maker_first_exit_enabled: true,
            max_book_age_sec: BOOK_TRUST_SEC,
        }
    }
}

// ── Tables ──────────────────────────────────────────────────────────────────

const RATCHET_TABLE: &[(i64, i64)] = &[
    (100, 94),
    (50, 44),
    (40, 35),
    (30, 25),
    (25, 20),
    (20, 15),
    (15, 10),
    (10, 6),
    (8, 4),
    (6, 3),
    (5, 2),
    (4, 1),
    (3, 0),
    (2, -2),
    (1, -4),
];
const RATCHET_DEFAULT_FLOOR: i64 = -12;

pub fn get_ratchet_floor(confirmed_high_pct: Decimal) -> Decimal {
    for (threshold, floor) in RATCHET_TABLE {
        if confirmed_high_pct >= Decimal::from(*threshold) {
            return Decimal::from(*floor);
        }
    }
    Decimal::from(RATCHET_DEFAULT_FLOOR)
}

pub fn get_profit_trail_pct(high_pnl_pct: Decimal, cfg: &ExitConfig) -> Decimal {
    let table = if high_pnl_pct >= dec!(50) {
        dec!(15)
    } else if high_pnl_pct >= dec!(30) {
        dec!(12)
    } else if high_pnl_pct >= dec!(20) {
        dec!(9)
    } else if high_pnl_pct >= dec!(10) {
        dec!(6)
    } else if high_pnl_pct >= dec!(5) {
        dec!(4)
    } else if high_pnl_pct >= dec!(3) {
        dec!(3)
    } else {
        dec!(2)
    };

    if cfg.proportional_trail_enabled && high_pnl_pct >= cfg.proportional_trail_min_pct {
        let prop = high_pnl_pct * (cfg.proportional_trail_pct / Decimal::ONE_HUNDRED);
        return cfg.proportional_trail_min_giveback_pct.max(table.min(prop));
    }
    table
}

pub fn get_time_trail_pct(time_left_sec: i64) -> Decimal {
    if time_left_sec > 420 {
        dec!(12)
    } else if time_left_sec > 180 {
        dec!(8)
    } else {
        dec!(6)
    }
}

pub fn effective_stop_pct(base: Decimal, time_left_sec: i64, cfg: &ExitConfig) -> Decimal {
    if !cfg.dynamic_stop_enabled {
        return base;
    }
    let start = cfg.stop_tighten_start_sec;
    let floor_t = cfg.force_exit_sec.min((start - 1).max(1));
    let min_pct = base.min(cfg.stop_min_pct);
    if time_left_sec >= start {
        return base;
    }
    if time_left_sec <= floor_t {
        return min_pct;
    }
    let frac = Decimal::from(time_left_sec - floor_t) / Decimal::from(start - floor_t);
    min_pct + (base - min_pct) * frac
}

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

// ── State ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitState {
    pub high_pnl_pct: Decimal,
    pub low_pnl_pct: Decimal,
    pub was_ever_positive: bool,
    pub high_water_mark: Decimal,
    pub hwm_confirm_count: u32,
    pub confirmed_high: Decimal,
    pub last_bid_price: Decimal,
    pub bid_unchanged_since: i64,
    /// When the last usable quote was folded in. Ages the fallback price a
    /// protective stop may still use when the book goes dark (P0 #177).
    /// Snapshots written before this field existed carry the serde default 0 =
    /// "age unknown" (see `last_known_price`).
    #[serde(default)]
    pub last_quote_at_ms: i64,
    pub last_progress_at: i64,
    pub last_progress_pct: Decimal,
    pub initial_depth: Decimal,
}

impl ExitState {
    pub fn new(entry_price: Decimal, now_ms: i64) -> Self {
        Self {
            high_pnl_pct: Decimal::ZERO,
            low_pnl_pct: Decimal::ZERO,
            was_ever_positive: false,
            high_water_mark: entry_price,
            hwm_confirm_count: 0,
            confirmed_high: entry_price,
            last_bid_price: entry_price,
            bid_unchanged_since: now_ms,
            // The entry fill is itself a real price at `now_ms`: it is what the
            // position's last-known price means until a book arrives.
            last_quote_at_ms: now_ms,
            last_progress_at: now_ms,
            last_progress_pct: Decimal::ZERO,
            initial_depth: Decimal::ZERO,
        }
    }
}

pub fn pnl_pct(price: Decimal, entry_price: Decimal) -> Decimal {
    if entry_price > Decimal::ZERO {
        ((price - entry_price) / entry_price) * Decimal::ONE_HUNDRED
    } else {
        Decimal::ZERO
    }
}

/// The price a long can realistically SELL at: the live best bid, or nothing.
///
/// F6: this must never fall back to the mid. A book with no bids has NO buyer —
/// the mid `(0 + ask)/2` is an arithmetic artifact, not a level anyone will
/// lift; returning it manufactured a sell price out of a one-sided book
/// (ask 0.90, no bid → a phantom 0.45) and let exits book profit no counterparty
/// was offering. Valuation (a display reference) and executability (an order
/// that can actually fill) are different questions; callers that need a
/// reference price use [`reference_price`], while anything that places a SELL
/// or books realised PnL prices itself off THIS function and must treat a zero
/// return as "no executable quote".
pub fn executable_bid<B: BookScalarView + ?Sized>(book: Option<&B>) -> Decimal {
    match book {
        None => Decimal::ZERO,
        Some(b) if b.best_bid() > Decimal::ZERO => b.best_bid(),
        _ => Decimal::ZERO,
    }
}

/// Issue #390: the SCALAR projection of a book — exactly the fields the exit
/// path and the marks refresh read, and none of the level vectors.
///
/// The replay's maintenance pass used to build a full [`OrderbookSnapshot`]
/// per open position per tick: two owned level-vector clones, a token
/// `String`, and five Decimal reductions (depth sums, OBI, spread)
/// recomputed from data the core's mirrored [`crate::sim::Book`] already
/// carries. Every one of those is per-position waste — the ladder and the
/// marks refresh read ONLY the scalars below (`best_bid`, `best_ask`,
/// `mid_price`, the two depths, `timestamp`), never the vectors. `BookView`
/// carries them computed ONCE per mirrored book and copied per position (a
/// flat `Copy` struct — no allocation), so a quiet-tick refresh of a few
/// hundred positions stops allocating entirely.
///
/// Semantics are the snapshot's, field for field:
///   * `best_bid` = highest bid, else 0; `best_ask` = lowest ask, else the
///     pre-existing ONE sentinel ("nothing for sale" read as maximally
///     expensive, see `strategy_logic::model`);
///   * `has_bids` / `has_asks` mirror the snapshot's `bids.is_empty()` /
///     `asks.is_empty()` — the ONE sentinel makes the emptiness flag the only
///     way to tell "no ask side" from "an ask genuinely priced at 1.00"
///     ([`best_live_ask`] depends on the distinction);
///   * `bid_depth` / `ask_depth` are the size sums;
///   * the F6 side-aware mid: zero unless BOTH sides quote a usable price —
///     the ask sentinel must not manufacture a mid from a one-sided book.
///
/// `from_book` reads the mirrored book UNORDERED (the mirror stores the
/// producer's level order; only `from_levels` sorts), so it derives the tops
/// with max/min — the same values `from_sorted_levels`' sort would put first.
#[derive(Debug, Clone, Copy)]
pub struct BookView {
    pub bid_depth: Decimal,
    pub ask_depth: Decimal,
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    pub mid_price: Decimal,
    pub timestamp: i64,
    pub has_bids: bool,
    pub has_asks: bool,
}

impl BookView {
    /// The scalar projection of a mirrored [`crate::sim::Book`] at
    /// `timestamp` — the same values [`OrderbookSnapshot::from_levels`]
    /// produces for the same levels, without building the snapshot.
    pub fn from_book(book: &crate::sim::Book, timestamp: i64) -> Self {
        let bid_depth: Decimal = book.bids.iter().map(|(_, s)| *s).sum();
        let ask_depth: Decimal = book.asks.iter().map(|(_, s)| *s).sum();
        let best_bid = book
            .bids
            .iter()
            .map(|(p, _)| *p)
            .max()
            .unwrap_or(Decimal::ZERO);
        let best_ask = book
            .asks
            .iter()
            .map(|(p, _)| *p)
            .min()
            .unwrap_or(Decimal::ONE);
        // F6 side-aware mid, exactly as `from_sorted_levels` computes it: a
        // mid exists only when BOTH sides quote a usable price. The ask
        // sentinel (1 with no asks) must not manufacture a mid — a one-sided
        // book presents no tradeable reference.
        let both_sides_quote = !book.bids.is_empty()
            && !book.asks.is_empty()
            && best_bid > Decimal::ZERO
            && best_ask > Decimal::ZERO;
        let mid_price = if both_sides_quote {
            (best_bid + best_ask) / Decimal::TWO
        } else {
            Decimal::ZERO
        };
        Self {
            bid_depth,
            ask_depth,
            best_bid,
            best_ask,
            mid_price,
            timestamp,
            has_bids: !book.bids.is_empty(),
            has_asks: !book.asks.is_empty(),
        }
    }

    /// The scalar projection of an existing [`OrderbookSnapshot`] — a field
    /// copy (the snapshot already did the reductions), never a rebuild.
    pub fn from_snapshot(s: &OrderbookSnapshot) -> Self {
        Self {
            bid_depth: s.bid_depth,
            ask_depth: s.ask_depth,
            best_bid: s.best_bid,
            best_ask: s.best_ask,
            mid_price: s.mid_price,
            timestamp: s.timestamp,
            has_bids: !s.bids.is_empty(),
            has_asks: !s.asks.is_empty(),
        }
    }
}

/// A display/reference valuation for a long position when there is no
/// executable bid: the mid of a two-sided book, else the last known
/// `current_price`. NEVER use this to price a fill — realised PnL, SELL fills
/// and forced exits must go through [`executable_bid`] — it exists so the
/// dashboard keeps showing something sensible while a book is one-sided.
pub fn reference_price<B: BookScalarView + ?Sized>(book: Option<&B>, fallback: Decimal) -> Decimal {
    if let Some(b) = book {
        if b.best_bid() > Decimal::ZERO {
            return b.best_bid();
        }
        // model.rs zeroes the mid of a one-sided book, so this cannot
        // resurrect the phantom `(0 + ask)/2` price that started F6.
        if b.mid_price() > Decimal::ZERO {
            return b.mid_price();
        }
    }
    fallback
}

/// How old a book is at `now_ms`, or `None` when its age cannot be judged.
///
/// `OrderbookSnapshot::timestamp <= 0` is "no receive time recorded" — synthetic
/// fixtures and records written before the field was stamped. There is no age to
/// compute, which is a different statement from "age zero", and the two are kept
/// apart here so the choice made downstream ([`book_is_fresh`]) is visible
/// instead of hidden in an `||`.
pub fn book_age_ms<B: BookScalarView + ?Sized>(book: &B, now_ms: i64) -> Option<i64> {
    (book.timestamp() > 0).then(|| now_ms.saturating_sub(book.timestamp()))
}

/// Whether a book is inside the exit path's PRICING window ([#267], #268).
///
/// One spelling for a question the exit path asks in two places (this module's
/// callers and `position.rs`'s `priceable` gate): a book may price an exit while
/// it is younger than `max_book_age_sec`.
///
/// A book whose age is UNKNOWN (`timestamp <= 0`) is treated as fresh. That is a
/// deliberate choice and not an oversight: refusing to price every synthetic or
/// legacy snapshot would turn "this record carries no clock" into "this position
/// can never be exited", which is the failure this whole file is about. It is
/// asserted explicitly in this module's tests.
pub fn book_is_fresh<B: BookScalarView + ?Sized>(book: &B, now_ms: i64, cfg: &ExitConfig) -> bool {
    match book_age_ms(book, now_ms) {
        Some(age) => age <= cfg.max_book_age_sec * 1_000,
        None => true,
    }
}

/// Where a protective stop's reference price came from (P0 #177).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopRefSource {
    /// A live best bid that is not a dislocated wick.
    Bid,
    /// The mid, when the bid existed but was a dislocated wick: the mid is the
    /// price the guard judges on to decide whether the move is real.
    Mid,
    /// The best ask of a book with NO bid (#225): the buy side was swept, so
    /// the only real level left is what the market is offering to sell at. A
    /// long's value cannot exceed it, which is what makes it usable as an upper
    /// bound for the JUDGEMENT — it never prices a fill (see `executable_bid`).
    Ask,
    /// The last known price, when no book (or no usable price in it) is left.
    LastKnown,
}

/// The price a PROTECTIVE stop is judged on, with the provenance a reviewer
/// needs to explain the decision afterwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StopReference {
    pub price: Decimal,
    pub source: StopRefSource,
    /// The raw best bid that would have breached the stop, when the wick guard
    /// judged on a stable mid instead. `Some` means "the guard held a trigger
    /// back" — the caller must surface it, never swallow it (P0 #177).
    pub suppressed_bid: Option<Decimal>,
}

/// A protective stop the wick guard held back: the raw bid breached it while
/// the mid did NOT confirm the move. Reported so review sees the trigger that
/// was deliberately not taken (P0 #177).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SuppressedStop {
    pub bid: Decimal,
    pub mid: Decimal,
    pub pnl_pct_at_bid: Decimal,
    pub pnl_pct_at_mid: Decimal,
    pub stop_pct: Decimal,
}

/// What one exit evaluation concluded: the decision (if any) plus the
/// protective stop the guard suppressed (if any). [`decide_exit`] keeps the old
/// `Option<ExitDecision>` shape for callers that only act on the decision.
#[derive(Debug, Clone, PartialEq)]
pub struct ExitVerdict {
    pub decision: Option<ExitDecision>,
    pub suppressed_stop: Option<SuppressedStop>,
    /// The price the protective stop judged on this tick, with its provenance.
    /// `None` only when the tick returned before the stop was reached (a force
    /// exit on a live bid, a grace period, a dead entry price) — after that
    /// point it is always resolved, because a stop that judged on something
    /// must be able to say so.
    ///
    /// `check_exits` reports it when the fired exit cannot be priced into an
    /// order, so the operator sees WHICH price said "get out" instead of the
    /// position's stale valuation (#225).
    pub stop_reference: Option<StopReference>,
}

impl ExitVerdict {
    fn exit(reason: ExitReason, use_maker: bool, judged: Option<StopReference>) -> Self {
        Self {
            decision: Some(ExitDecision { reason, use_maker }),
            suppressed_stop: None,
            stop_reference: judged,
        }
    }
    fn hold() -> Self {
        Self {
            decision: None,
            suppressed_stop: None,
            stop_reference: None,
        }
    }
}

/// The last known price, if it is fresh enough to stand in for a live quote.
fn last_known_price(
    fallback: Option<Decimal>,
    state: &ExitState,
    now_ms: i64,
    cfg: &ExitConfig,
) -> Option<Decimal> {
    let price = fallback.filter(|p| *p > Decimal::ZERO)?;
    // Age unknown (a snapshot written before the field existed): the value is
    // the position's own last valuation, which is never newer than the position
    // itself — still the only honest number left, and a stale protective stop
    // errs toward exiting rather than toward staying in.
    if state.last_quote_at_ms <= 0 {
        return Some(price);
    }
    let age_ms = now_ms.saturating_sub(state.last_quote_at_ms);
    (age_ms <= cfg.max_last_price_age_sec.max(1) * 1_000).then_some(price)
}

/// The price a PROFIT-SIDE rule may JUDGE on when there is no executable bid
/// (#267). `None` = the rule has nothing to judge on and stays silent.
///
/// The ladder is [`reference_price`]'s, with one deliberate difference from the
/// protective stop's [`stop_reference`]:
///   1. a two-sided book's mid — a one-sided book has none, `from_levels` zeroes
///      it, so the F6 phantom `(0 + ask)/2` cannot come back through here;
///   2. the last known price, but ONLY while its age is KNOWN and inside
///      `max_last_price_age_sec`.
///
/// Step 2 is where this differs from `last_known_price`, which also accepts a
/// price of unknown age. That escape is granted to a protective stop on purpose
/// — "a stale protective stop errs toward exiting rather than toward staying in"
/// — and it must NOT be granted here: taking a PROFIT off a price nobody can
/// date is how a display reference becomes a trade, and it is exactly what
/// `dead_book_fallback_cannot_take_profit` forbids. So an unknown age is `None`
/// on this side.
///
/// Nothing here can price an order: a book with no bid has `executable_bid` 0
/// regardless of what this returns, so a trigger resolved here becomes a
/// REPORTED held exit ("I wanted out and nobody was bidding"), never a fill.
fn profit_reference<B: BookScalarView + ?Sized>(
    book: Option<&B>,
    fallback: Option<Decimal>,
    state: &ExitState,
    now_ms: i64,
    cfg: &ExitConfig,
) -> Option<Decimal> {
    // `reference_price` answers bid → mid → its fallback; with a zero fallback
    // its own answer is "the book has a reference price, or nothing".
    let from_book = reference_price(book, Decimal::ZERO);
    if from_book > Decimal::ZERO {
        return Some(from_book);
    }
    let price = fallback.filter(|p| *p > Decimal::ZERO)?;
    if state.last_quote_at_ms <= 0 {
        // Age unknown: no licence to take a profit (see the doc above).
        return None;
    }
    let age_ms = now_ms.saturating_sub(state.last_quote_at_ms);
    (age_ms <= cfg.max_last_price_age_sec.max(1) * 1_000).then_some(price)
}

/// The best ask a book actually carries, or `None` when it has no sell side.
///
/// `OrderbookSnapshot::best_ask` is `1` when the ask side is EMPTY — a
/// pre-existing entry-side sentinel ("nothing for sale" read as maximally
/// expensive, see `strategy_logic::model`), not a quote. The sentinel must be
/// read as "no ask", never as "someone is offering a dollar". The emptiness
/// FLAG (not the sentinel value) is the ground truth: a book that genuinely
/// quotes an ask at exactly 1.00 is a live ask, not an empty side.
fn best_live_ask<B: BookScalarView + ?Sized>(book: &B) -> Option<Decimal> {
    (book.has_asks() && book.best_ask() > Decimal::ZERO).then_some(book.best_ask())
}

/// Resolve the price a PROTECTIVE stop is judged on (P0 #177, #225).
///
/// The wick guard exists so a dislocated bid cannot TRIGGER a market exit — it
/// is not a reason to abandon the stop, and the one moment it must never win is
/// when the book is emptied and the mid collapsed with it. In order:
///   1. a live bid that is not a wick → judge on the bid (unchanged);
///   2. a live bid that IS a wick → judge on the mid: if the mid breached the
///      stop too the move is real and the stop fires; if the mid held, the
///      trigger the raw bid would have produced is reported as suppressed;
///   3. no bid at all but a live ASK → judge on `min(last known price, ask)`
///      (#225). The buy side was swept, so the only real level left is what the
///      market is offering to sell at; a long's value cannot exceed it, and a
///      remembered price that is ABOVE it is a price nobody is standing behind;
///   4. no usable price in the book → the last known price, if still fresh.
///
/// There is deliberately NO step that judges on a mid with no bid. F6 removed
/// the one-sided mid (`from_levels` zeroes it — `(0 + ask)/2` is arithmetic, not
/// a level anyone will lift), so that branch could never run on a real book;
/// leaving it in place is what made a swept book look like a quiet one (#225).
///
/// Nothing here can price an ORDER: a book with no bid has `executable_bid` 0
/// regardless of what this returns, so a trigger resolved through step 3 can
/// only ever become a reported held stop (#179's split — the gate prices the
/// order, not the decision).
fn stop_reference<B: BookScalarView + ?Sized>(
    book: Option<&B>,
    fallback: Option<Decimal>,
    state: &ExitState,
    now_ms: i64,
    cfg: &ExitConfig,
) -> Option<StopReference> {
    if let Some(b) = book {
        if b.best_bid() > Decimal::ZERO {
            if b.mid_price() > Decimal::ZERO {
                let wick = (b.mid_price() - b.best_bid()) / b.mid_price();
                if wick > cfg.max_bid_wick_pct / Decimal::ONE_HUNDRED {
                    return Some(StopReference {
                        price: b.mid_price(),
                        source: StopRefSource::Mid,
                        suppressed_bid: Some(b.best_bid()),
                    });
                }
            }
            return Some(StopReference {
                price: b.best_bid(),
                source: StopRefSource::Bid,
                suppressed_bid: None,
            });
        }
        if let Some(ask) = best_live_ask(b) {
            // `min` keeps the direction conservative: the ask may only ever fire
            // a stop the remembered price would have fired on its own, never
            // hold one back. A fresh last known price BELOW the ask — a book
            // that fell apart while a stale ask still hangs above it — stays
            // the judgement, exactly as before.
            let price =
                last_known_price(fallback, state, now_ms, cfg).map_or(ask, |known| known.min(ask));
            return Some(StopReference {
                price,
                source: StopRefSource::Ask,
                suppressed_bid: None,
            });
        }
    }
    last_known_price(fallback, state, now_ms, cfg).map(|price| StopReference {
        price,
        source: StopRefSource::LastKnown,
        suppressed_bid: None,
    })
}

/// The protective-stop verdict for one tick.
enum StopVerdict {
    /// The reference price breached the stop: place the exit.
    Fire,
    /// The raw bid breached the stop but the mid did not confirm the move: the
    /// guard withholds it, and the caller must report the withheld trigger.
    Suppressed(SuppressedStop),
    Nothing,
}

fn stop_verdict(
    entry_price: Decimal,
    stop_pct: Decimal,
    reference: Option<StopReference>,
) -> StopVerdict {
    let Some(reference) = reference else {
        return StopVerdict::Nothing;
    };
    let pct = pnl_pct(reference.price, entry_price);
    if pct <= -stop_pct {
        return StopVerdict::Fire;
    }
    // The guard only withholds a trigger it would otherwise have produced: the
    // raw bid must itself have breached the stop, and the mid must not have.
    if let Some(bid) = reference.suppressed_bid {
        let at_bid = pnl_pct(bid, entry_price);
        if at_bid <= -stop_pct {
            return StopVerdict::Suppressed(SuppressedStop {
                bid,
                mid: reference.price,
                pnl_pct_at_bid: at_bid,
                pnl_pct_at_mid: pct,
                stop_pct,
            });
        }
    }
    StopVerdict::Nothing
}

/// Update HWM / staleness / depth from a fresh book, using the executable price.
pub fn update_exit_state<B: BookScalarView + ?Sized>(
    state: &mut ExitState,
    entry_price: Decimal,
    book: Option<&B>,
    now_ms: i64,
    cfg: &ExitConfig,
) {
    update_exit_state_interval(
        state,
        entry_price,
        book,
        now_ms,
        cfg,
        now_ms,
        REPLAY_TICK_BOUNDARY_MS,
    );
}

/// [`update_exit_state_interval`] at the replay's DOUBLE update rate: the base
/// kernel advances `hwm_confirm_count` twice per maintenance tick (the
/// every-tick valuate pass and the every-tick ladder), so a replay run that
/// performs ONE merged update at `now_ms` must backfill the skipped
/// boundaries at 2× for the counter to track the base's walk. Everything else
/// in the maintenance update is idempotent under re-observation and needs no
/// rate. `last_run_ms >= now_ms` still leaves the update byte-identical to
/// [`update_exit_state`].
pub fn update_exit_state_interval_2x<B: BookScalarView + ?Sized>(
    state: &mut ExitState,
    entry_price: Decimal,
    book: Option<&B>,
    now_ms: i64,
    cfg: &ExitConfig,
    last_run_ms: i64,
    tick_boundary_ms: i64,
) {
    update_exit_state_interval(
        state,
        entry_price,
        book,
        now_ms,
        cfg,
        last_run_ms,
        2 * tick_boundary_ms,
    );
}

/// [`update_exit_state`] with an explicit run interval: the whole ladder sees
/// now_ms (the DECISION clock never moves — commands stamp the tick they fire
/// on), and `last_run_ms` is the previous call's own instant, used ONLY to
/// undo the per-tick counter ratchets the skipped maintenance ticks would
/// have walked through. Issue #390: the replay maintenance pass runs on the
/// 50 ms tick grid, and the fast path proves a quiet-tick re-run reproduces
/// the previous marks exactly — the cheap output-preserving generalisation is
/// to run the pass on its deadlines and compensate the one stateful counter
/// the interval invalidates. `last_run_ms >= now_ms` (never ran, or the
/// same-tick legacy spelling) leaves the ladder byte-identical to
/// [`update_exit_state`].
pub fn update_exit_state_interval<B: BookScalarView + ?Sized>(
    state: &mut ExitState,
    entry_price: Decimal,
    book: Option<&B>,
    now_ms: i64,
    cfg: &ExitConfig,
    last_run_ms: i64,
    tick_boundary_ms: i64,
) {
    let Some(book) = book else { return };
    if entry_price <= Decimal::ZERO {
        return;
    }
    let val = executable_bid(Some(book));
    if val <= Decimal::ZERO {
        return;
    }
    // A usable quote was seen: stamps the age the protective stop's fallback is
    // bounded by (P0 #177).
    state.last_quote_at_ms = now_ms;
    let pct = pnl_pct(val, entry_price);

    if pct > state.high_pnl_pct {
        state.high_pnl_pct = pct;
    }
    if pct < state.low_pnl_pct {
        state.low_pnl_pct = pct;
    }
    if pct > Decimal::ZERO {
        state.was_ever_positive = true;
    }

    if val > state.high_water_mark {
        state.high_water_mark = val;
        state.hwm_confirm_count = 1;
    } else {
        let near_high = state.high_water_mark > Decimal::ZERO
            && ((val - state.high_water_mark).abs() / state.high_water_mark * Decimal::ONE_HUNDRED
                < cfg.ratchet_confirm_tolerance_pct);
        if near_high {
            // Issue #390: `hwm_confirm_count` walks once per MAINTENANCE tick
            // that sees the same near-high quote. An interval-compensated run
            // observes only its own boundaries, so the counter advances by the
            // tick boundaries the ladder did NOT run through (final +1 = the
            // observed one). The skipped boundaries carried UNCHANGED books —
            // a re-run against the same snapshot advances the counter and
            // touches nothing else — so the arithmetic below reproduces the
            // ratchet state the every-tick pass would have produced at this
            // quote. Two correction terms keep the replay exact:
            //   * `last_run_ms` bounds the backfill (ticks before the ladder
            //     first ran this position are not ladder ticks), and a
            //     same-instant legacy run is a plain +1;
            //   * the counter SATURATES at `ratchet_confirm_ticks` in
            //     observable terms — once it has fired `confirmed_high`, the
            //     every-tick pass keeps counting, but `confirmed_high` has
            //     already latched the value a later HWM step would only
            //     re-latch after a fresh confirm walk. Reproducing the
            //     post-saturation count exactly is therefore unnecessary for
            //     every observable output, and the saturating add (below)
            //     keeps a long-skipped interval from wrapping the u32.
            let skipped = if last_run_ms >= now_ms {
                0u64
            } else {
                let gap_ms = (now_ms - last_run_ms).max(0) as u64;
                // Tick boundaries strictly between the runs: the replay loop
                // fires its uniform grid, so a gap of G ms carries
                // floor((G-1)/tick_ms) skipped boundaries. The caller passes
                // its grid (`tick_boundary_ms`); the constant is only the
                // documented default.
                let step = if tick_boundary_ms > 0 {
                    tick_boundary_ms as u64
                } else {
                    REPLAY_TICK_BOUNDARY_MS as u64
                };
                gap_ms.saturating_sub(1) / step.max(1)
            };
            state.hwm_confirm_count = state
                .hwm_confirm_count
                .saturating_add((skipped.min(u64::from(u32::MAX - 1))) as u32)
                .saturating_add(1)
                .min(u32::MAX);
            if state.hwm_confirm_count >= cfg.ratchet_confirm_ticks {
                state.confirmed_high = state.high_water_mark;
            }
        } else {
            state.hwm_confirm_count = 0;
        }
    }

    if state.initial_depth == Decimal::ZERO {
        state.initial_depth = book.bid_depth() + book.ask_depth();
    }
    if book.best_bid() != state.last_bid_price {
        state.last_bid_price = book.best_bid();
        state.bid_unchanged_since = now_ms;
    }
    if (pct - state.last_progress_pct).abs() > Decimal::ONE {
        state.last_progress_at = now_ms;
        state.last_progress_pct = pct;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExitDecision {
    pub reason: ExitReason,
    pub use_maker: bool,
}

pub struct ExitTickInput<'a> {
    pub entry_price: Decimal,
    /// The book this tick judges on. Generic over the scalar view: callers
    /// holding a full [`OrderbookSnapshot`] and callers holding the
    /// allocation-free [`BookView`] (issue #390's replay maintenance pass)
    /// run the SAME decision code — the exit path reads only the scalars.
    pub book: Option<&'a dyn BookScalarView>,
    pub fallback_price: Option<Decimal>,
    pub time_left_sec: i64,
    pub hold_sec: i64,
    pub state: &'a ExitState,
    pub now_ms: i64,
    pub cfg: &'a ExitConfig,
}

const BREAKEVEN_LOCK_TRIGGER_PCT: i64 = 3;

/// Issue #390: the replay maintenance grid the interval-compensated ladder
/// backfills over. The replay driver steps `tick_ms` (50 ms in every shipped
/// config and test), so the count of skipped boundaries inside a gap of G ms
/// is (G-1)/tick_ms; the caller passes its own grid for non-default steps.
pub const REPLAY_TICK_BOUNDARY_MS: i64 = 50;

/// Pure exit decision; mutates nothing. Mandatory exits return use_maker=false.
///
/// This is the narrow shape for callers that only act on the decision (offline
/// replay, shadow variants); the kernel's live path uses
/// [`decide_exit_verdict`], which also carries back the protective stop the
/// wick guard withheld (P0 #177).
pub fn decide_exit(input: ExitTickInput) -> Option<ExitDecision> {
    decide_exit_verdict(input).decision
}

/// Pure exit decision plus the suppressed-stop report; mutates nothing.
///
/// F6: every exit ORDER is priced off the EXECUTABLE bid. `fallback_price` is a
/// stale display reference — a position can no longer be force-exited or
/// stopped out of it when nobody is bidding, because that books a SELL at a
/// price no counterparty ever offered. Expiry without a bid must be settled as
/// a market outcome (the service layer's job), not sold at the last price.
///
/// PRICING and JUDGING are different questions, and this function answers both
/// with different numbers (#267):
///
/// * the protective stop judges on [`stop_reference`] — bid, else the mid, else
///   the ask-side bound, else the last known price inside its freshness budget
///   (P0 #177 / #225) — and it is the one rule that keeps working when the quote
///   is gone, because "no usable bid" is the situation it exists for;
/// * the profit side judges on [`profit_reference`] — the live bid when there is
///   one, else a two-sided mid, else the last known price inside the SAME
///   budget. It used to require a live bid to judge at all, which made "cannot
///   be priced" mean "does not fire" and left a bid-less book with a stop that
///   still worked and a profit side that had gone dark;
/// * the live bid, and only the live bid, prices an order: a rule that fires
///   without one is reported as a held exit by `check_exits`, never filled.
pub fn decide_exit_verdict(input: ExitTickInput) -> ExitVerdict {
    let ExitTickInput {
        entry_price,
        book,
        fallback_price,
        time_left_sec,
        hold_sec,
        state,
        now_ms,
        cfg,
    } = input;
    if entry_price <= Decimal::ZERO {
        return ExitVerdict::hold();
    }

    let bid = executable_bid(book);
    // F6 (main #168) and the deploy line's stop machinery agree on the same
    // split: only a LIVE bid may price a mandatory/voluntary SELL. That is a
    // statement about PRICING an order and about nothing else — see below.
    let live = bid > Decimal::ZERO;

    // 1. Force exit — absolute deadline. It still prices off the live bid:
    //    deadline pressure alone must not mint a SELL against a dried-up book.
    //    `force_exit_sec <= 0` DISABLES the guillotine (the calibrated default:
    //    the deadline amputated rounds that went on to pay); a non-positive
    //    threshold can never be a deadline, it can only be a misfire.
    if cfg.force_exit_sec > 0 && time_left_sec <= cfg.force_exit_sec {
        if !live {
            return ExitVerdict::hold();
        }
        return ExitVerdict::exit(ExitReason::ForceExit, false, None);
    }

    // Calibrated expiry gate (@almach: SELL=0, every exit a REDEEM/MERGE).
    // When the guillotine is OFF (the shipped default) a position at/past
    // expiry belongs to SETTLEMENT, not to this ladder: every rule below
    // prices off a book that is dying with the round, and letting them fire
    // converts a $1 winner into a "stop-loss" at the residual bid — the same
    // failure that zeroed bid-less winners at settlement. Placed AFTER the
    // force-exit check on purpose: an operator who re-arms the guillotine has
    // made the explicit choice to sell at the deadline, expiry included; the
    // calibrated default lets settlement own the position (dry:
    // `drive_settlement`; live: the venue answer). Strategy-signal exits
    // bypass this ladder entirely, so an explicit "get out" is honoured.
    if cfg.force_exit_sec <= 0 && time_left_sec <= 0 {
        return ExitVerdict::hold();
    }

    // The stop needs SOME price to judge on, and may fall back to the last
    // known one; every other rule additionally requires a live quote. Resolve
    // it ONCE: the stop is the one rule that must keep working when the quote
    // is gone (#177), and `check_exits` has to report WHICH price judged when
    // the fired exit cannot be priced into an order (#225).
    let stop_ref = stop_reference(book, fallback_price, state, now_ms, cfg);

    // #267: the profit side needs a price to JUDGE on for exactly the same
    // reason the stop does. Gating the judgement on `live` conflated "can this
    // be priced" with "should this fire": on a book with no bid the stop still
    // fired through its fallback while every profit rule went dark, so the exit
    // ladder kept its loss half and lost its profit half — "该跑的没跑、该止损的
    // 照止损". The judgement now reads `profit_reference` (bid → mid → bounded
    // last known), while `live`/`executable_bid` still decide what may be
    // PRICED. A trigger with no bid behind it becomes a reported held exit
    // (`StopSuppressionCause::NoExecutableQuote`), which is the visible form of
    // "wanted out, nobody bidding" — never a silent non-trigger, and never a
    // fill priced off a mid.
    let profit_judge = if live {
        Some(bid)
    } else {
        profit_reference(book, fallback_price, state, now_ms, cfg)
    };
    if profit_judge.is_none() && stop_ref.is_none() {
        // Nothing at all to judge on: no bid, no ask, and no remembered price
        // inside its budget. There is no rule left that could honestly fire.
        return ExitVerdict::hold();
    }
    let pct = profit_judge.map_or(Decimal::ZERO, |p| pnl_pct(p, entry_price));
    let mut suppressed: Option<SuppressedStop> = None;

    if cfg.simple_exit_enabled {
        if profit_judge.is_some()
            && cfg.take_profit_pct > Decimal::ZERO
            && cfg.take_profit_pct < dec!(9999)
            && pct >= cfg.take_profit_pct
        {
            return ExitVerdict::exit(ExitReason::TakeProfit, cfg.maker_first_exit_enabled, None);
        }
        match stop_verdict(
            entry_price,
            effective_stop_pct(cfg.stop_loss_pct, time_left_sec, cfg),
            stop_ref,
        ) {
            StopVerdict::Fire => return ExitVerdict::exit(ExitReason::StopLoss, false, stop_ref),
            StopVerdict::Suppressed(s) => suppressed = Some(s),
            StopVerdict::Nothing => {}
        }
        if profit_judge.is_some()
            && cfg.trailing_enabled
            && state.high_pnl_pct >= cfg.trailing_min_high_pct
        {
            let profit_trail = get_profit_trail_pct(state.high_pnl_pct, cfg);
            let time_trail = get_time_trail_pct(time_left_sec);
            let trail = cfg.min_trail_pct.max(profit_trail.min(time_trail));
            if state.high_pnl_pct - pct >= trail {
                return ExitVerdict::exit(ExitReason::TrailingStop, false, None);
            }
        }
        if profit_judge.is_some() && time_left_sec <= cfg.min_time_left_sec {
            return ExitVerdict::exit(ExitReason::TimeExit, false, None);
        }
        return ExitVerdict {
            decision: None,
            suppressed_stop: suppressed,
            stop_reference: stop_ref,
        };
    }

    // Full mode: grace period right after a maker fill.
    if hold_sec < cfg.exit_grace_sec {
        return ExitVerdict::hold();
    }

    if profit_judge.is_some() && pct >= cfg.take_profit_pct {
        return ExitVerdict::exit(ExitReason::TakeProfit, cfg.maker_first_exit_enabled, None);
    }

    let base_stop = if cfg.tight_stop_enabled {
        cfg.tight_stop_pct
    } else {
        cfg.stop_loss_pct
    };
    match stop_verdict(
        entry_price,
        effective_stop_pct(base_stop, time_left_sec, cfg),
        stop_ref,
    ) {
        StopVerdict::Fire => return ExitVerdict::exit(ExitReason::StopLoss, false, stop_ref),
        StopVerdict::Suppressed(s) => suppressed = Some(s),
        StopVerdict::Nothing => {}
    }

    if let Some(judge) = profit_judge {
        // None of these is the protective stop — they are the profit-side rules,
        // which now judge on `judge` (the live bid when there is one, else the
        // bounded reference) and still require an executable bid to become an
        // ORDER. They carry no stop reference forward: a report about one of
        // them is about the bid, and `check_exits` already reports `exit_price`
        // for that.
        if cfg.ratchet_enabled {
            let confirmed_high_pct = pnl_pct(state.confirmed_high, entry_price);
            if pct <= get_ratchet_floor(confirmed_high_pct) {
                return ExitVerdict::exit(ExitReason::RatchetFloor, false, None);
            }
        }

        if state.high_pnl_pct >= Decimal::from(BREAKEVEN_LOCK_TRIGGER_PCT) {
            let lock_floor = dec!(0.5).max(taker_fee_pct(judge) + dec!(0.2));
            if pct <= lock_floor {
                return ExitVerdict::exit(ExitReason::BreakevenLock, false, None);
            }
        }

        if cfg.trailing_enabled && state.high_pnl_pct >= cfg.trailing_min_high_pct {
            let profit_trail = get_profit_trail_pct(state.high_pnl_pct, cfg);
            let time_trail = get_time_trail_pct(time_left_sec);
            let trail = cfg.min_trail_pct.max(profit_trail.min(time_trail));
            if state.high_pnl_pct - pct >= trail {
                return ExitVerdict::exit(ExitReason::TrailingStop, false, None);
            }
        }

        if let Some(book) = book
            && state.initial_depth > Decimal::ZERO
        {
            let current_depth = book.bid_depth() + book.ask_depth();
            let depth_change = ((current_depth - state.initial_depth) / state.initial_depth)
                * Decimal::ONE_HUNDRED;
            if depth_change <= -cfg.depth_collapse_threshold_pct
                && judge < state.high_water_mark
                && pct >= dec!(2)
            {
                return ExitVerdict::exit(ExitReason::DepthCollapse, false, None);
            }
        }

        if pct >= cfg.stale_profit_pct {
            let stale_sec = (now_ms - state.bid_unchanged_since) / 1000;
            if stale_sec >= cfg.stale_profit_bid_unchanged_sec {
                return ExitVerdict::exit(ExitReason::StaleProfit, true, None);
            }
        }

        if pct >= cfg.stagnant_profit_pct && pct < cfg.take_profit_pct {
            let stagnant_sec = (now_ms - state.last_progress_at) / 1000;
            if stagnant_sec >= cfg.stagnant_duration_sec {
                return ExitVerdict::exit(ExitReason::StagnantProfit, true, None);
            }
        }

        if time_left_sec <= cfg.min_time_left_sec {
            return ExitVerdict::exit(ExitReason::TimeExit, cfg.maker_exits_for_tp_only, None);
        }
    }

    ExitVerdict {
        decision: None,
        suppressed_stop: suppressed,
        stop_reference: stop_ref,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        use rust_decimal::prelude::FromPrimitive;
        let bd = Decimal::from_f64(bid).unwrap();
        let ad = Decimal::from_f64(ask).unwrap();
        OrderbookSnapshot {
            token_id: "t".into(),
            bids: vec![(bd, dec!(100))],
            asks: vec![(ad, dec!(100))],
            bid_depth: dec!(100),
            ask_depth: dec!(100),
            obi: Decimal::ZERO,
            spread: ad - bd,
            spread_pct: Decimal::ZERO,
            best_bid: bd,
            best_ask: ad,
            mid_price: (bd + ad) / Decimal::TWO,
            timestamp: 0,
        }
    }

    /// The shipped default must stay the shipped default. If a change flips it,
    /// this is the test that says so before a replay is needed (#203).
    #[test]
    fn default_fee_schedule_is_the_shipped_one() {
        let s = fee_schedule();
        assert_eq!(s.name, "legacy_quadratic");
        assert_eq!(s.rate, dec!(0.125));
        assert_eq!(s.exponent, 2);
    }

    /// The legacy curve as charged: `0.125*(p*(1-p))^2` per share, quoted as a
    /// percentage of price (p=0.40 -> $0.0072/share, 1.8% of price).
    #[test]
    fn legacy_schedule_prices_as_documented() {
        assert_eq!(taker_fee_pct(dec!(0.4)), dec!(1.8));
        assert_eq!(taker_fee_pct(dec!(0.5)), dec!(1.5625));
        assert_eq!(taker_fee_pct(dec!(0)), Decimal::ZERO);
    }

    /// #203's decision rests on this ratio: the published crypto schedule is
    /// 2.33x the legacy curve at p=0.40 and 5.30x at p=0.12. A test, not a
    /// comment, because the whole "switching is a cost increase" claim is this
    /// arithmetic.
    #[test]
    fn official_schedule_is_the_published_multiple_of_legacy() {
        let official = fee_schedule_by_name("official").expect("official schedule");
        assert_eq!(official.rate, dec!(0.07));
        assert_eq!(official.exponent, 1);
        let legacy = legacy_quadratic_schedule();
        // The ratio is taken through the ONE production spelling of the curve
        // (#234, item 2): a restated copy here would test the copy, not the fee.
        let at = |p: Decimal| official.fee_per_share(p) / legacy.fee_per_share(p);
        assert_eq!(at(dec!(0.40)).round_dp(2), dec!(2.33));
        assert_eq!(at(dec!(0.12)).round_dp(2), dec!(5.30));
    }

    /// A price outside `(0, 1)` charges nothing — for EVERY schedule, in both
    /// conventions (#234, item 3). The `p > 1` half is the money-path case: with
    /// `exponent >= 1` the un-clamped curve prices a NEGATIVE fee there, i.e. a
    /// rebate on a live fill. `p == 1` is 0 for the accidental reason that the
    /// base is 0, so it is pinned here rather than left to arithmetic luck.
    #[test]
    fn out_of_range_prices_are_charged_nothing() {
        for s in [legacy_quadratic_schedule(), official_schedule()] {
            for p in [dec!(1), dec!(1.0000001), dec!(1.5), dec!(2), dec!(100)] {
                assert_eq!(
                    s.fee_per_share(p),
                    Decimal::ZERO,
                    "{}: {p} is not a price in (0,1)",
                    s.name
                );
                assert_eq!(
                    s.fee_pct(p),
                    Decimal::ZERO,
                    "{}: {p} must charge nothing as a percentage either",
                    s.name
                );
                assert!(
                    s.fee_per_share(p) >= Decimal::ZERO,
                    "{}: a fee at {p} must never be a rebate",
                    s.name
                );
            }
            for p in [dec!(0), dec!(-0.5), dec!(-1)] {
                assert_eq!(s.fee_per_share(p), Decimal::ZERO, "{}: p={p}", s.name);
                assert_eq!(s.fee_pct(p), Decimal::ZERO, "{}: p={p}", s.name);
            }
        }
        // The charge path in force goes through the same clamp.
        assert_eq!(taker_fee_pct(dec!(1)), Decimal::ZERO);
        assert_eq!(taker_fee_pct(dec!(1.5)), Decimal::ZERO);
        assert_eq!(taker_fee_pct(dec!(2)), Decimal::ZERO);
    }

    /// Every schedule must name where its parameters come from. `0.07` is only
    /// authoritative because the publication says so; a schedule with a blank
    /// source is a number nobody can check (#203 acceptance).
    #[test]
    fn every_schedule_declares_its_source() {
        for name in FEE_SCHEDULE_NAMES {
            let s = fee_schedule_by_name(name).unwrap_or_else(|| panic!("{name} missing"));
            assert!(
                s.source.len() > 20,
                "{name}: the schedule must say who published its parameters, got {:?}",
                s.source
            );
            assert_eq!(s.name, *name, "lookup name and declared name disagree");
        }
        assert!(fee_schedule_by_name("no_such_model").is_none());
    }

    #[test]
    fn force_exit_fires_at_deadline() {
        // The guillotine is OFF by default (calibrated: the deadline amputated
        // rounds that went on to pay), so the deadline itself is pinned here:
        // the RULE still works when an operator turns it on.
        let cfg = ExitConfig {
            force_exit_sec: 120,
            ..Default::default()
        };
        let st = ExitState::new(dec!(0.4), 0);
        let b = book(0.39, 0.41);
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: None,
            time_left_sec: 100,
            hold_sec: 50,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        });
        assert_eq!(d.unwrap().reason, ExitReason::ForceExit);
    }

    /// The calibrated default: `force_exit_sec = 0` DISABLES the guillotine —
    /// including the `time_left == 0` edge a non-positive threshold would
    /// otherwise misfire on ahead of settlement.
    #[test]
    fn force_exit_default_is_disabled() {
        let cfg = ExitConfig::default();
        assert_eq!(cfg.force_exit_sec, 0);
        assert_eq!(cfg.min_time_left_sec, 0);
        let st = ExitState::new(dec!(0.4), 0);
        let b = book(0.39, 0.41);
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: None,
            time_left_sec: 0,
            hold_sec: 900,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        });
        // At expiry with the guillotine OFF the ladder is SILENT — the position
        // belongs to settlement (dry: `drive_settlement`; live: the venue
        // answer). No dying-book rule (stop included) may fire: letting them
        // would sell a $1 winner at the residual bid.
        assert!(
            d.is_none(),
            "expiry + guillotine OFF = hold; settlement owns the position"
        );
    }

    #[test]
    fn take_profit_is_maker_first() {
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        // Bid +150% ≥ TP 100%.
        let b = book(1.0, 1.0);
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: None,
            time_left_sec: 600,
            hold_sec: 20,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        })
        .unwrap();
        assert_eq!(d.reason, ExitReason::TakeProfit);
        assert!(d.use_maker);
    }

    #[test]
    fn default_stop_is_tight_but_an_explicit_wide_stop_does_not_fire() {
        // Default stop was tightened to 15% (see the §26 tuning note), so a -30%
        // bid now fires a StopLoss in simple mode.
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let b = book(0.28, 0.30); // -30%
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: None,
            time_left_sec: 600,
            hold_sec: 20,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        })
        .unwrap();
        assert_eq!(d.reason, ExitReason::StopLoss);
        assert!(!d.use_maker);

        // An explicit wide stop (the old 50%) still holds through -30%.
        let wide = ExitConfig {
            stop_loss_pct: dec!(50),
            ..Default::default()
        };
        assert!(
            decide_exit(ExitTickInput {
                entry_price: dec!(0.4),
                book: Some(&b),
                fallback_price: None,
                time_left_sec: 600,
                hold_sec: 20,
                state: &st,
                now_ms: 1000,
                cfg: &wide,
            })
            .is_none()
        );
    }

    #[test]
    fn wick_below_mid_does_not_trigger_stop() {
        // bid 0.20 vs mid 0.40 → a 50% wick; protective stop must not fire on it.
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let b = book(0.20, 0.60);
        let verdict = decide_exit_verdict(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: None,
            time_left_sec: 600,
            hold_sec: 20,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        });
        assert!(verdict.decision.is_none());
        // …but the trigger it withheld must be reported (P0 #177): a stop that
        // fired on the raw bid and was held back is NOT the same as no trigger.
        let s = verdict
            .suppressed_stop
            .expect("a withheld protective stop must be reported");
        assert_eq!(s.bid, dec!(0.20));
        assert_eq!(s.mid, dec!(0.40));
        assert_eq!(s.pnl_pct_at_bid, dec!(-50));
        assert_eq!(s.pnl_pct_at_mid, dec!(0));
    }

    /// A bid-less book is the situation the protective stop exists for: the
    /// mid is the only price left, and it must be able to fire the stop.
    #[test]
    fn zero_bid_falling_mid_fires_stop() {
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let mut b = book(0.30, 0.30);
        b.best_bid = Decimal::ZERO;
        b.mid_price = dec!(0.15);
        b.bids.clear();
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: None,
            time_left_sec: 600,
            hold_sec: 20,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        })
        .expect("a collapsed mid with no bid must still fire the stop");
        assert_eq!(d.reason, ExitReason::StopLoss);
        assert!(!d.use_maker);
    }

    /// No book at all: the position's own last price (fresh) stands in.
    #[test]
    fn missing_book_uses_fresh_last_price_for_stop() {
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: None,
            fallback_price: Some(dec!(0.15)),
            time_left_sec: 600,
            hold_sec: 20,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        })
        .expect("a fresh last price must be able to fire the stop");
        assert_eq!(d.reason, ExitReason::StopLoss);
    }

    /// …but the fallback is bounded by freshness: a stale last price can no
    /// longer speak for the market.
    #[test]
    fn stale_last_price_does_not_fire_stop() {
        let cfg = ExitConfig::default();
        let mut st = ExitState::new(dec!(0.4), 0);
        st.last_quote_at_ms = 0; // written before the field existed = age unknown
        // Age the quote well past the bound.
        let aged = ExitState {
            last_quote_at_ms: 1,
            ..st
        };
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: None,
            fallback_price: Some(dec!(0.15)),
            time_left_sec: 600,
            hold_sec: 20,
            state: &aged,
            now_ms: 1 + cfg.max_last_price_age_sec * 1_000 + 1,
            cfg: &cfg,
        });
        assert!(d.is_none());
        // The same price, still fresh, does fire — the bound is the only
        // difference between the two.
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: None,
            fallback_price: Some(dec!(0.15)),
            time_left_sec: 600,
            hold_sec: 20,
            state: &aged,
            now_ms: 1 + cfg.max_last_price_age_sec * 1_000,
            cfg: &cfg,
        });
        assert_eq!(d.unwrap().reason, ExitReason::StopLoss);
    }

    /// A real decline that ALSO trips the wick ratio (thin book, bid hanging
    /// far under the ask) is not a pin bar: the mid collapsed with it, so the
    /// stop must fire rather than hide behind the wick guard.
    #[test]
    fn wick_ratio_with_collapsed_mid_fires_stop() {
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let b = book(0.05, 0.075); // mid 0.0625: -84% from entry, wick 20%
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: None,
            time_left_sec: 600,
            hold_sec: 20,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        })
        .expect("a mid that fell in sync is a real decline, not a wick");
        assert_eq!(d.reason, ExitReason::StopLoss);
    }

    /// A book with no levels on either side ("the book was swept") has no price
    /// at all: its placeholder mid must not veto the stop, so the last known
    /// price decides.
    #[test]
    fn an_emptied_book_falls_back_to_the_last_price() {
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let mut b = book(0.0, 0.0);
        b.best_bid = Decimal::ZERO;
        b.best_ask = Decimal::ONE;
        b.mid_price = dec!(0.5); // the from_levels placeholder
        b.bids.clear();
        b.asks.clear();
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4),
            book: Some(&b),
            fallback_price: Some(dec!(0.15)),
            time_left_sec: 600,
            hold_sec: 20,
            state: &st,
            now_ms: 1000,
            cfg: &cfg,
        })
        .expect("an emptied book must not hide the stop behind a placeholder mid");
        assert_eq!(d.reason, ExitReason::StopLoss);
    }

    /// A dead book — no levels on either side, so the mid is only the
    /// construction placeholder — plus a fallback quote far ABOVE entry is a
    /// paper profit with no executable price behind it. The fallback may not
    /// manufacture a take-profit any more than it may hide a stop.
    #[test]
    fn dead_book_fallback_cannot_take_profit() {
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let mut b = book(0.0, 0.0);
        b.best_bid = Decimal::ZERO;
        b.best_ask = Decimal::ZERO;
        b.mid_price = Decimal::ZERO;
        b.bids.clear();
        b.asks.clear();
        assert!(
            decide_exit(ExitTickInput {
                entry_price: dec!(0.4),
                book: Some(&b),
                fallback_price: Some(dec!(1.0)), // +150% on paper
                time_left_sec: 600,
                hold_sec: 20,
                state: &st,
                now_ms: 1000,
                cfg: &cfg,
            })
            .is_none()
        );
    }

    // ── F6: a one-sided book must never mint an executable sell price ───────

    fn one_sided_book(ask: Decimal) -> OrderbookSnapshot {
        OrderbookSnapshot {
            token_id: "t".into(),
            bids: vec![],
            asks: vec![(ask, dec!(100))],
            bid_depth: Decimal::ZERO,
            ask_depth: dec!(100),
            obi: Decimal::ZERO,
            spread: Decimal::ZERO,
            spread_pct: Decimal::ZERO,
            best_bid: Decimal::ZERO,
            best_ask: ask,
            // A one-sided book carries NO mid (see strategy_logic
            // `from_sorted_levels`): `(0 + ask)/2` was the phantom price that
            // started F6.
            mid_price: Decimal::ZERO,
            timestamp: 0,
        }
    }

    #[test]
    fn no_bid_means_no_executable_price_even_with_a_high_ask() {
        // ask 0.90, zero bids: the old code returned mid = 0.45 here.
        let b = one_sided_book(dec!(0.90));
        assert_eq!(executable_bid(Some(&b)), Decimal::ZERO);
        assert_eq!(executable_bid::<OrderbookSnapshot>(None), Decimal::ZERO);
    }

    #[test]
    fn force_exit_does_not_fire_without_an_executable_bid() {
        // The audit's F6 scenario: maker entry @0.40, then only an ask 0.90
        // remains. Deadline pressure must not mint a 0.45 SELL out of thin air.
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let b = one_sided_book(dec!(0.90));
        assert!(
            decide_exit(ExitTickInput {
                entry_price: dec!(0.4),
                book: Some(&b),
                // The stale last-known price is a reference, not a quote: even
                // offered as fallback it must not price a forced SELL.
                fallback_price: Some(dec!(0.45)),
                time_left_sec: 10,
                hold_sec: 800,
                state: &st,
                now_ms: 900_000,
                cfg: &cfg,
            })
            .is_none()
        );
    }

    #[test]
    fn reference_price_prefers_bid_then_two_sided_mid_then_fallback() {
        let two_sided = book(0.40, 0.44);
        assert_eq!(reference_price(Some(&two_sided), dec!(9)), dec!(0.40));
        // One-sided book: no phantom mid — fall back to the last known price.
        let one_sided = one_sided_book(dec!(0.90));
        assert_eq!(reference_price(Some(&one_sided), dec!(9)), dec!(9));
        assert_eq!(reference_price::<OrderbookSnapshot>(None, dec!(9)), dec!(9));
    }

    // ── #225: a bid-less book must not leave the stop with no price ──────────

    /// Build a book the way the kernel does, so the placeholder/sentinel prices
    /// an empty side gets are part of what is under test.
    fn shaped(bids: Vec<(Decimal, Decimal)>, asks: Vec<(Decimal, Decimal)>) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels("tok", bids, asks, 1_000)
    }

    /// The stop's judgement for a book, with a last known price of 0.40 taken a
    /// second ago (fresh for the default 30s bound).
    fn judged(book: &OrderbookSnapshot, fallback: Decimal) -> StopReference {
        stop_reference(
            Some(book),
            Some(fallback),
            &ExitState::new(dec!(0.40), 1_000),
            1_000,
            &ExitConfig::default(),
        )
        .expect("every book here leaves the stop something to judge on")
    }

    #[test]
    fn a_bidless_book_judges_the_stop_on_its_ask_and_prices_nothing() {
        let b = shaped(vec![], vec![(dec!(0.15), dec!(1000))]);
        assert_eq!(b.mid_price, Decimal::ZERO, "F6: no bid ⇒ no mid");

        let r = judged(&b, dec!(0.40));
        assert_eq!(r.price, dec!(0.15), "the ask is the only real level left");
        assert_eq!(r.source, StopRefSource::Ask);
        assert_eq!(r.suppressed_bid, None);
        // …and the judgement still cannot become an order: the F6 gate is
        // untouched, so all it can produce is a REPORTED held stop (#179).
        assert_eq!(executable_bid(Some(&b)), Decimal::ZERO);
    }

    #[test]
    fn an_ask_above_the_last_known_price_never_loosens_or_tightens_the_stop() {
        let b = shaped(vec![], vec![(dec!(0.90), dec!(1000))]);
        // The buy side was swept but the market did not fall: the remembered
        // price stays the judgement. `min` is what guarantees this branch can
        // only ever FIRE a stop the old code fired, never hide one.
        assert_eq!(judged(&b, dec!(0.40)).price, dec!(0.40));
        // …and an ask ABOVE a last known price that itself collapsed does not
        // raise the judgement back up.
        assert_eq!(judged(&b, dec!(0.10)).price, dec!(0.10));
    }

    #[test]
    fn a_book_with_no_levels_at_all_carries_no_price_information() {
        let b = shaped(vec![], vec![]);
        // The empty ask side's `1` is an entry-side sentinel (pre-existing API,
        // see `strategy_logic::model`), not a dollar of demand — reading it as a
        // quote would judge every stop against a price nobody offered.
        assert_eq!(b.best_ask, Decimal::ONE);
        assert_eq!(b.mid_price, Decimal::ZERO);

        let r = judged(&b, dec!(0.40));
        assert_eq!(r.price, dec!(0.40), "only the last known price is left");
        assert_eq!(r.source, StopRefSource::LastKnown);
    }

    #[test]
    fn a_mid_without_a_bid_is_never_the_judgement() {
        // A hand-built or legacy snapshot can still carry the phantom
        // `(0 + ask)/2` mid that F6 killed. `from_levels` no longer produces it,
        // so the branch that read it is gone — and it must not come back
        // through a snapshot that was not built by the kernel.
        let mut b = shaped(vec![], vec![]);
        b.mid_price = dec!(0.15);

        let r = judged(&b, dec!(0.40));
        assert_eq!(r.source, StopRefSource::LastKnown);
        assert_eq!(r.price, dec!(0.40));
    }

    #[test]
    fn a_stale_last_price_leaves_the_live_ask_as_the_only_witness() {
        // The remembered price is past its freshness bound, so it drops out —
        // but the ask is a live level in front of us, and there is no reason to
        // fall silent while one exists (#225).
        let b =
            OrderbookSnapshot::from_levels("tok", vec![], vec![(dec!(0.15), dec!(1000))], 32_000);
        let r = stop_reference(
            Some(&b),
            Some(dec!(0.40)),
            &ExitState::new(dec!(0.40), 1_000), // last quote 31s ago
            32_000,
            &ExitConfig::default(),
        )
        .expect("a live ask is a price");
        assert_eq!(r.price, dec!(0.15));
        assert_eq!(r.source, StopRefSource::Ask);

        // With no ask either there is nothing at all, and the stop is silent.
        let dark = OrderbookSnapshot::from_levels("tok", vec![], vec![], 32_000);
        assert!(
            stop_reference(
                Some(&dark),
                Some(dec!(0.40)),
                &ExitState::new(dec!(0.40), 1_000),
                32_000,
                &ExitConfig::default(),
            )
            .is_none(),
            "no bid, no ask and a stale memory is the one case with no witness"
        );
    }

    // ── F8: fee schedules ────────────────────────────────────────────────────
    //
    // These assert on schedule VALUES, never on an installed one: the active
    // schedule is set-once per process, so a test that swapped it would poison
    // every other test in the binary. `FeeSchedule` is a plain `Copy` value and
    // needs no installation to be measured.

    #[test]
    fn legacy_schedule_fee_is_unchanged() {
        let legacy = legacy_quadratic_schedule();
        // p=0.50: 0.125 * (0.25)^2 = 0.0078125 USD/share → /0.50 = 1.5625%.
        assert_eq!(legacy.fee_pct(dec!(0.50)), dec!(1.5625));
        // The process default is this curve, so the charge path agrees.
        assert_eq!(taker_fee_pct(dec!(0.50)), legacy.fee_pct(dec!(0.50)));
        assert_eq!(legacy.fee_pct(dec!(0)), Decimal::ZERO);
        assert_eq!(legacy.fee_pct(dec!(-1)), Decimal::ZERO);
        // At and above a full dollar nothing is charged (#234, item 3): `p == 1`
        // for the base's sake, `p > 1` because squaring must not be what hides a
        // rebate.
        assert_eq!(legacy.fee_pct(dec!(1)), Decimal::ZERO);
        assert_eq!(legacy.fee_pct(dec!(1.5)), Decimal::ZERO);
        assert_eq!(legacy.fee_per_share(dec!(1.5)), Decimal::ZERO);
    }

    #[test]
    fn official_schedule_fee_matches_official_parameters() {
        // Official: fee_usd = shares * 0.07 * p * (1-p). 100 shares @0.50 →
        // 100 * 0.07 * 0.25 = $1.75. As a percentage of the fill price:
        // 0.07 * 0.5 * 100 = 3.5%, and 3.5% * 0.50 * 100 shares = $1.75. The
        // per-share and percentage conventions must agree at every price.
        let official = official_schedule();
        for p in [dec!(0.50), dec!(0.40), dec!(0.62), dec!(0.95)] {
            let per_share_fee = dec!(0.07) * p * (Decimal::ONE - p);
            let pct = official.fee_pct(p);
            assert_eq!(
                (pct / Decimal::ONE_HUNDRED) * p,
                per_share_fee,
                "pct convention must equal the official per-share fee at p={p}"
            );
        }
        // p=0.50: 0.07 * 0.25 = 0.0175 USD/share → /0.50 = 3.5% of price.
        assert_eq!(official.fee_pct(dec!(0.50)), dec!(3.5));
        // 100 shares @0.50 → exactly $1.75.
        let fee_usd =
            (official.fee_pct(dec!(0.50)) / Decimal::ONE_HUNDRED) * dec!(0.50) * dec!(100);
        assert_eq!(fee_usd, dec!(1.75));
        // At p=0.50 the published curve is 2.24x the legacy one — the cost jump
        // #203 measured.
        let legacy = legacy_quadratic_schedule();
        assert_eq!(
            (official.fee_pct(dec!(0.50)) / legacy.fee_pct(dec!(0.50))).round_dp(2),
            dec!(2.24)
        );
        // Edge prices charge nothing. `p > 1` is the one that matters here: this
        // schedule's exponent is 1, so without the clamp `p*(1-p)` is negative
        // and the fee becomes a REBATE — money moving the wrong way (#234,
        // item 3). `p == 1` is 0 for the base's sake.
        assert_eq!(official.fee_pct(dec!(0)), Decimal::ZERO);
        assert_eq!(official.fee_pct(dec!(1)), Decimal::ZERO);
        assert_eq!(official.fee_pct(dec!(1.5)), Decimal::ZERO);
        assert!(
            official.fee_per_share(dec!(1.5)) >= Decimal::ZERO,
            "a fee above $1 of price must never be negative"
        );
    }

    #[test]
    fn crypto_fee_flips_the_marginal_round_trip_sign() {
        // Audit F8: 100 shares taker 0.50 in → 0.52 out. Legacy ≈ +0.44 net;
        // official crypto fee ≈ −1.497 net. The sign must flip.
        let round_trip = |s: &FeeSchedule| -> Decimal {
            let entry_fee = (s.fee_pct(dec!(0.50)) / Decimal::ONE_HUNDRED) * dec!(0.50) * dec!(100);
            let exit_fee = (s.fee_pct(dec!(0.52)) / Decimal::ONE_HUNDRED) * dec!(0.52) * dec!(100);
            (dec!(0.52) - dec!(0.50)) * dec!(100) - entry_fee - exit_fee
        };
        let legacy = round_trip(&legacy_quadratic_schedule());
        let crypto = round_trip(&official_schedule());
        assert!(
            legacy > Decimal::ZERO,
            "legacy round trip was profitable: {legacy}"
        );
        assert!(
            crypto < Decimal::ZERO,
            "crypto round trip must lose: {crypto}"
        );
    }

    // ── #267: a bid-less book must not put the profit side out of work ───────

    /// The exact tick the issue names (#267, acceptance 1): the buy side has been
    /// swept (`bid 0 / ask 0.90`), the position has run to a high above the
    /// trailing arming threshold, and its last known price is inside the
    /// judgement window. Gating every profit rule on `live` made this a `hold`:
    /// the stop still fired through its fallback while the profit side went
    /// dark, so the ladder kept its loss half and lost its profit half.
    #[test]
    fn a_bidless_book_judges_the_profit_side_on_a_fresh_last_known_price() {
        // Trailing is explicitly ON here: this test pins the #267 MECHANISM
        // (a bid-less book still JUDGES the profit side), which is orthogonal
        // to the calibrated default of banking nothing via trailing sells.
        let cfg = ExitConfig {
            trailing_enabled: true,
            ..Default::default()
        };
        let now = 1_000_000;
        let entry = dec!(0.40);
        let b = one_sided_book(dec!(0.90));
        assert_eq!(
            executable_bid(Some(&b)),
            Decimal::ZERO,
            "F6: a one-sided book is still not priceable, so the decision below \
             cannot be coming from a bid"
        );
        // `high_pnl_pct` is a RUNNING high: 50% is what the position reached
        // before the buy side emptied, 0.48 (+20%) is where its own last known
        // price now sits. Both are `check_exits`' own inputs.
        let st = ExitState {
            high_pnl_pct: dec!(50),
            last_quote_at_ms: now - 5_000,
            ..ExitState::new(entry, now)
        };

        fn tick<'a>(
            b: &'a OrderbookSnapshot,
            state: &'a ExitState,
            cfg: &'a ExitConfig,
            now_ms: i64,
        ) -> ExitTickInput<'a> {
            ExitTickInput {
                entry_price: dec!(0.40),
                book: Some(b),
                fallback_price: Some(dec!(0.48)),
                time_left_sec: 600,
                hold_sec: 60,
                state,
                now_ms,
                cfg,
            }
        }

        let verdict = decide_exit_verdict(tick(&b, &st, &cfg, now));
        assert_eq!(
            verdict.decision.map(|d| d.reason),
            Some(ExitReason::TrailingStop),
            "the profit side must judge on the remembered price, not go dark"
        );
        assert!(
            verdict.suppressed_stop.is_none(),
            "nothing was withheld: the stop never fired here"
        );
        assert!(
            verdict.stop_reference.is_none(),
            "a profit-side exit carries no stop reference: the report is about \
             the bid, and the two sides judge independently (#267)"
        );

        // The freshness bound is what governs, not the book's emptiness: the
        // same tick with the memory past its window has nothing honest left to
        // judge a profit on, and holds.
        let aged = ExitState {
            last_quote_at_ms: now - (cfg.max_last_price_age_sec + 1) * 1_000,
            ..st.clone()
        };
        assert!(
            decide_exit_verdict(tick(&b, &aged, &cfg, now))
                .decision
                .is_none(),
            "a remembered price past its budget is not a judgement"
        );
    }

    // ── #268: one quote-trust budget, two windows ───────────────────────────

    /// #268, item 3: the judgement window (30 s) and the pricing window (60 s)
    /// are one concept at two budgets, so the band where a stop could still act
    /// while the profit side was already blind is stated rather than implied.
    #[test]
    fn the_two_windows_are_one_quote_trust_budget_at_two_depths() {
        let cfg = ExitConfig::default();
        assert_eq!(cfg.max_last_price_age_sec, QUOTE_TRUST_SEC);
        assert_eq!(cfg.max_book_age_sec, BOOK_TRUST_SEC);
        assert_eq!(
            BOOK_TRUST_SEC,
            QUOTE_TRUST_SEC * 2,
            "a book carries real levels a remembered number has nobody behind, \
             so it may be read for twice the budget — derived, never restated"
        );
    }

    /// #268, item 1: `timestamp <= 0` is "this record carries no clock", which
    /// is a different statement from "age zero" and takes the other branch. The
    /// choice — an unknown age counts as FRESH — is asserted here so nobody has
    /// to infer it from an `||`.
    #[test]
    fn an_unknown_book_age_is_fresh_by_choice_and_a_far_expired_one_is_not() {
        let cfg = ExitConfig::default();
        let now = 1_000_000;

        let unknown = one_sided_book(dec!(0.90)); // timestamp 0
        assert_eq!(
            book_age_ms(&unknown, now),
            None,
            "no clock on the record is not age zero"
        );
        assert!(
            book_is_fresh(&unknown, now, &cfg),
            "an unknown age counts as fresh ON PURPOSE: refusing to price every \
             clockless snapshot would make 'no timestamp' mean 'can never exit'"
        );

        let expired = OrderbookSnapshot::from_levels(
            "tok",
            vec![],
            vec![(dec!(0.90), dec!(100))],
            now - 10 * 60_000,
        );
        assert_eq!(book_age_ms(&expired, now), Some(600_000));
        assert!(
            !book_is_fresh(&expired, now, &cfg),
            "ten minutes old is not a quote standing here now"
        );

        // The budget is the only difference between the two verdicts: the same
        // book exactly one budget old is still inside it.
        let inside = OrderbookSnapshot::from_levels(
            "tok",
            vec![],
            vec![(dec!(0.90), dec!(100))],
            now - cfg.max_book_age_sec * 1_000,
        );
        assert!(book_is_fresh(&inside, now, &cfg));
    }
}
