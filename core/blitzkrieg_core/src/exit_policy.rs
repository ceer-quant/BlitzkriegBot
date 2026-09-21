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
    /// How old the last known price may be to stand in for a protective stop
    /// when the book has no usable quote left (no bid AND no mid). A trigger
    /// judged on a price older than this has nothing honest to stand on and
    /// stays silent; the profit side never uses this at all (P0 #177).
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
    /// exempt because their age cannot be judged. Live service books are
    /// rebuilt with `timestamp = now_ms` and are therefore always "fresh" at
    /// this layer; real receive-time preservation happens upstream.
    pub max_book_age_sec: i64,
}

impl Default for ExitConfig {
    fn default() -> Self {
        // Tuned from the exit walk-forward study (MIGRATION_LOG §26/§27). The
        // profit side mirrors Node: fixed TP 100% is only a backstop, and normal
        // exits ride the trailing stop. The ONE deliberate deviation from Node is
        // the stop-loss (50 -> 15), which the study showed was the dominant loss
        // source; that change alone flipped profit factor from 1.55 to ~2.5.
        Self {
            force_exit_sec: 120,
            min_time_left_sec: 180,
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
            trailing_enabled: true,
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
            // The engine's book freshness gate is 8s; a protective stop may act
            // on a quote a few multiples older than that rather than on nothing
            // at all (see `stop_reference`).
            max_last_price_age_sec: 30,
            stale_profit_pct: dec!(20),
            stale_profit_bid_unchanged_sec: 10,
            stagnant_profit_pct: dec!(5),
            stagnant_duration_sec: 30,
            depth_collapse_threshold_pct: dec!(60),
            exit_grace_sec: 3,
            maker_exits_for_tp_only: false,
            maker_first_exit_enabled: true,
            max_book_age_sec: 60,
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
    /// This schedule's taker fee as a PERCENTAGE OF THE FILL PRICE — the unit the
    /// charge path settles in (`fee_usd = pct/100 * price * shares`). The schedule
    /// stores the per-share form (`rate * (p*(1-p))^exponent` USD per share,
    /// which is how Polymarket publishes it); dividing by `price` converts it, so
    /// both curves keep ONE accounting convention at every call site.
    /// `price <= 0` charges nothing.
    pub fn fee_pct(&self, price: Decimal) -> Decimal {
        if price <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let base = price * (Decimal::ONE - price);
        let mut acc = Decimal::ONE;
        for _ in 0..self.exponent {
            acc *= base;
        }
        ((self.rate * acc) / price) * Decimal::ONE_HUNDRED
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
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
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
pub fn executable_bid(book: Option<&OrderbookSnapshot>) -> Decimal {
    match book {
        None => Decimal::ZERO,
        Some(b) if b.best_bid > Decimal::ZERO => b.best_bid,
        _ => Decimal::ZERO,
    }
}

/// A display/reference valuation for a long position when there is no
/// executable bid: the mid of a two-sided book, else the last known
/// `current_price`. NEVER use this to price a fill — realised PnL, SELL fills
/// and forced exits must go through [`executable_bid`] — it exists so the
/// dashboard keeps showing something sensible while a book is one-sided.
pub fn reference_price(book: Option<&OrderbookSnapshot>, fallback: Decimal) -> Decimal {
    if let Some(b) = book {
        if b.best_bid > Decimal::ZERO {
            return b.best_bid;
        }
        // model.rs zeroes the mid of a one-sided book, so this cannot
        // resurrect the phantom `(0 + ask)/2` price that started F6.
        if b.mid_price > Decimal::ZERO {
            return b.mid_price;
        }
    }
    fallback
}

/// Where a protective stop's reference price came from (P0 #177).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopRefSource {
    /// A live best bid that is not a dislocated wick.
    Bid,
    /// The mid: either the book had no bid at all, or the bid was a wick.
    Mid,
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
}

impl ExitVerdict {
    fn exit(reason: ExitReason, use_maker: bool) -> Self {
        Self {
            decision: Some(ExitDecision { reason, use_maker }),
            suppressed_stop: None,
        }
    }
    fn hold() -> Self {
        Self {
            decision: None,
            suppressed_stop: None,
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

/// Resolve the price a PROTECTIVE stop is judged on (P0 #177).
///
/// The wick guard exists so a dislocated bid cannot TRIGGER a market exit — it
/// is not a reason to abandon the stop, and the one moment it must never win is
/// when the book is emptied and the mid collapsed with it. In order:
///   1. a live bid that is not a wick → judge on the bid (unchanged);
///   2. a live bid that IS a wick → judge on the mid: if the mid breached the
///      stop too the move is real and the stop fires; if the mid held, the
///      trigger the raw bid would have produced is reported as suppressed;
///   3. no bid at all but a mid → judge on the mid (it is already the price
///      `executable_bid` values the position at, so nothing can veto a stop on
///      top of it);
///   4. no usable price in the book → the last known price, if still fresh.
fn stop_reference(
    book: Option<&OrderbookSnapshot>,
    fallback: Option<Decimal>,
    state: &ExitState,
    now_ms: i64,
    cfg: &ExitConfig,
) -> Option<StopReference> {
    if let Some(b) = book {
        // A book with NO levels at all carries no price information: its
        // `mid_price` is the builder's placeholder (0 and 1 → 0.5), not a
        // quote, and letting it veto the stop is the very hole this fixes.
        let has_levels = !b.bids.is_empty() || !b.asks.is_empty();
        if b.best_bid > Decimal::ZERO {
            if b.mid_price > Decimal::ZERO {
                let wick = (b.mid_price - b.best_bid) / b.mid_price;
                if wick > cfg.max_bid_wick_pct / Decimal::ONE_HUNDRED {
                    return Some(StopReference {
                        price: b.mid_price,
                        source: StopRefSource::Mid,
                        suppressed_bid: Some(b.best_bid),
                    });
                }
            }
            return Some(StopReference {
                price: b.best_bid,
                source: StopRefSource::Bid,
                suppressed_bid: None,
            });
        }
        if has_levels && b.mid_price > Decimal::ZERO {
            return Some(StopReference {
                price: b.mid_price,
                source: StopRefSource::Mid,
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
    book: Option<&OrderbookSnapshot>,
    fallback: Option<Decimal>,
    state: &ExitState,
    now_ms: i64,
    cfg: &ExitConfig,
) -> StopVerdict {
    let Some(reference) = stop_reference(book, fallback, state, now_ms, cfg) else {
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
pub fn update_exit_state(
    state: &mut ExitState,
    entry_price: Decimal,
    book: Option<&OrderbookSnapshot>,
    now_ms: i64,
    cfg: &ExitConfig,
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
            state.hwm_confirm_count += 1;
            if state.hwm_confirm_count >= cfg.ratchet_confirm_ticks {
                state.confirmed_high = state.high_water_mark;
            }
        } else {
            state.hwm_confirm_count = 0;
        }
    }

    if state.initial_depth == Decimal::ZERO {
        state.initial_depth = book.bid_depth + book.ask_depth;
    }
    if book.best_bid != state.last_bid_price {
        state.last_bid_price = book.best_bid;
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
    pub book: Option<&'a OrderbookSnapshot>,
    pub fallback_price: Option<Decimal>,
    pub time_left_sec: i64,
    pub hold_sec: i64,
    pub state: &'a ExitState,
    pub now_ms: i64,
    pub cfg: &'a ExitConfig,
}

const BREAKEVEN_LOCK_TRIGGER_PCT: i64 = 3;

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
/// F6: every exit decision prices off the EXECUTABLE bid. `fallback_price` is a
/// stale display reference — a position can no longer be force-exited or
/// stopped out of it when nobody is bidding, because that books a SELL at a
/// price no counterparty ever offered. Expiry without a bid must be settled as
/// a market outcome (the service layer's job), not sold at the last price.
///
/// The protective stop is the ONE rule that keeps working when the quote is
/// gone, because "no usable bid" is the situation it exists for. Every
/// profit-side rule below is evaluated against the live executable bid only —
/// acting on a stale or dislocated quote to take a profit is exactly the
/// mistake the fallback must not introduce.
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
    // split: only a LIVE bid may price a mandatory/voluntary SELL; the last
    // known price may still price the protective stop (bounded by
    // `max_last_price_age_sec`), and nothing may price a profit-side exit.
    let live = bid > Decimal::ZERO;

    // 1. Force exit — absolute deadline. It still prices off the live bid:
    //    deadline pressure alone must not mint a SELL against a dried-up book.
    if time_left_sec <= cfg.force_exit_sec {
        if !live {
            return ExitVerdict::hold();
        }
        return ExitVerdict::exit(ExitReason::ForceExit, false);
    }

    // The stop needs SOME price to judge on, and may fall back to the last
    // known one; every other rule additionally requires a live quote.
    if !live && stop_reference(book, fallback_price, state, now_ms, cfg).is_none() {
        return ExitVerdict::hold();
    }
    let pct = if live {
        pnl_pct(bid, entry_price)
    } else {
        Decimal::ZERO
    };
    let mut suppressed: Option<SuppressedStop> = None;

    if cfg.simple_exit_enabled {
        if live
            && cfg.take_profit_pct > Decimal::ZERO
            && cfg.take_profit_pct < dec!(9999)
            && pct >= cfg.take_profit_pct
        {
            return ExitVerdict::exit(ExitReason::TakeProfit, cfg.maker_first_exit_enabled);
        }
        match stop_verdict(
            entry_price,
            effective_stop_pct(cfg.stop_loss_pct, time_left_sec, cfg),
            book,
            fallback_price,
            state,
            now_ms,
            cfg,
        ) {
            StopVerdict::Fire => return ExitVerdict::exit(ExitReason::StopLoss, false),
            StopVerdict::Suppressed(s) => suppressed = Some(s),
            StopVerdict::Nothing => {}
        }
        if live && cfg.trailing_enabled && state.high_pnl_pct >= cfg.trailing_min_high_pct {
            let profit_trail = get_profit_trail_pct(state.high_pnl_pct, cfg);
            let time_trail = get_time_trail_pct(time_left_sec);
            let trail = cfg.min_trail_pct.max(profit_trail.min(time_trail));
            if state.high_pnl_pct - pct >= trail {
                return ExitVerdict::exit(ExitReason::TrailingStop, false);
            }
        }
        if live && time_left_sec <= cfg.min_time_left_sec {
            return ExitVerdict::exit(ExitReason::TimeExit, false);
        }
        return ExitVerdict {
            decision: None,
            suppressed_stop: suppressed,
        };
    }

    // Full mode: grace period right after a maker fill.
    if hold_sec < cfg.exit_grace_sec {
        return ExitVerdict::hold();
    }

    if live && pct >= cfg.take_profit_pct {
        return ExitVerdict::exit(ExitReason::TakeProfit, cfg.maker_first_exit_enabled);
    }

    let base_stop = if cfg.tight_stop_enabled {
        cfg.tight_stop_pct
    } else {
        cfg.stop_loss_pct
    };
    match stop_verdict(
        entry_price,
        effective_stop_pct(base_stop, time_left_sec, cfg),
        book,
        fallback_price,
        state,
        now_ms,
        cfg,
    ) {
        StopVerdict::Fire => return ExitVerdict::exit(ExitReason::StopLoss, false),
        StopVerdict::Suppressed(s) => suppressed = Some(s),
        StopVerdict::Nothing => {}
    }

    if live {
        if cfg.ratchet_enabled {
            let confirmed_high_pct = pnl_pct(state.confirmed_high, entry_price);
            if pct <= get_ratchet_floor(confirmed_high_pct) {
                return ExitVerdict::exit(ExitReason::RatchetFloor, false);
            }
        }

        if state.high_pnl_pct >= Decimal::from(BREAKEVEN_LOCK_TRIGGER_PCT) {
            let lock_floor = dec!(0.5).max(taker_fee_pct(bid) + dec!(0.2));
            if pct <= lock_floor {
                return ExitVerdict::exit(ExitReason::BreakevenLock, false);
            }
        }

        if cfg.trailing_enabled && state.high_pnl_pct >= cfg.trailing_min_high_pct {
            let profit_trail = get_profit_trail_pct(state.high_pnl_pct, cfg);
            let time_trail = get_time_trail_pct(time_left_sec);
            let trail = cfg.min_trail_pct.max(profit_trail.min(time_trail));
            if state.high_pnl_pct - pct >= trail {
                return ExitVerdict::exit(ExitReason::TrailingStop, false);
            }
        }

        if let Some(book) = book
            && state.initial_depth > Decimal::ZERO
        {
            let current_depth = book.bid_depth + book.ask_depth;
            let depth_change = ((current_depth - state.initial_depth) / state.initial_depth)
                * Decimal::ONE_HUNDRED;
            if depth_change <= -cfg.depth_collapse_threshold_pct
                && bid < state.high_water_mark
                && pct >= dec!(2)
            {
                return ExitVerdict::exit(ExitReason::DepthCollapse, false);
            }
        }

        if pct >= cfg.stale_profit_pct {
            let stale_sec = (now_ms - state.bid_unchanged_since) / 1000;
            if stale_sec >= cfg.stale_profit_bid_unchanged_sec {
                return ExitVerdict::exit(ExitReason::StaleProfit, true);
            }
        }

        if pct >= cfg.stagnant_profit_pct && pct < cfg.take_profit_pct {
            let stagnant_sec = (now_ms - state.last_progress_at) / 1000;
            if stagnant_sec >= cfg.stagnant_duration_sec {
                return ExitVerdict::exit(ExitReason::StagnantProfit, true);
            }
        }

        if time_left_sec <= cfg.min_time_left_sec {
            return ExitVerdict::exit(ExitReason::TimeExit, cfg.maker_exits_for_tp_only);
        }
    }

    ExitVerdict {
        decision: None,
        suppressed_stop: suppressed,
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
        let per_share = |s: &FeeSchedule, p: Decimal| {
            let mut acc = Decimal::ONE;
            let base = p * (Decimal::ONE - p);
            for _ in 0..s.exponent {
                acc *= base;
            }
            s.rate * acc
        };
        let legacy = legacy_quadratic_schedule();
        let at = |p: Decimal| per_share(&official, p) / per_share(&legacy, p);
        assert_eq!(at(dec!(0.40)).round_dp(2), dec!(2.33));
        assert_eq!(at(dec!(0.12)).round_dp(2), dec!(5.30));
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
        let cfg = ExitConfig::default();
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
        assert_eq!(executable_bid(None), Decimal::ZERO);
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
        assert_eq!(reference_price(None, dec!(9)), dec!(9));
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
        // Edge prices charge nothing.
        assert_eq!(official.fee_pct(dec!(0)), Decimal::ZERO);
        assert_eq!(official.fee_pct(dec!(1)), Decimal::ZERO);
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
}
