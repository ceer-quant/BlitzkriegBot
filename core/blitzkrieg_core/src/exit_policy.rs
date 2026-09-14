//! Exit policy — pure, replayable exit decisions (Rust port of
//! `src/strategies/crypto-hft/exit-policy.ts`).
//!
//! Single source of truth for exit logic in the core, mirrored byte-for-byte by
//! the TS policy so offline replay and both runtimes agree. Triggers and the
//! high-water mark are valued on the EXECUTABLE bid (what a long can be sold
//! for), never the mid: a token can show mid 0.82 while the bid is 0.59.
//!
//! Protective stops additionally require the bid not to be a dislocated wick
//! (bid within maxBidWickPct of mid); profit-side triggers don't need that.

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
    pub stale_profit_pct: Decimal,
    pub stale_profit_bid_unchanged_sec: i64,
    pub stagnant_profit_pct: Decimal,
    pub stagnant_duration_sec: i64,
    pub depth_collapse_threshold_pct: Decimal,
    pub exit_grace_sec: i64,
    pub maker_exits_for_tp_only: bool,
    pub maker_first_exit_enabled: bool,
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
            stale_profit_pct: dec!(20),
            stale_profit_bid_unchanged_sec: 10,
            stagnant_profit_pct: dec!(5),
            stagnant_duration_sec: 30,
            depth_collapse_threshold_pct: dec!(60),
            exit_grace_sec: 3,
            maker_exits_for_tp_only: false,
            maker_first_exit_enabled: true,
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

/// fee_per_share = 0.125*(p*(1-p))^2; as a percentage of price.
pub fn taker_fee_pct(price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let one_minus = Decimal::ONE - price;
    let fee = dec!(0.125) * (price * one_minus) * (price * one_minus);
    (fee / price) * Decimal::ONE_HUNDRED
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

/// The price a long can realistically sell at: live best bid, else mid, else 0.
pub fn executable_bid(book: Option<&OrderbookSnapshot>) -> Decimal {
    match book {
        None => Decimal::ZERO,
        Some(b) => {
            if b.best_bid > Decimal::ZERO {
                b.best_bid
            } else if b.mid_price > Decimal::ZERO {
                b.mid_price
            } else {
                Decimal::ZERO
            }
        }
    }
}

fn bid_confirmed_by_mid(book: &OrderbookSnapshot, cfg: &ExitConfig) -> bool {
    if book.best_bid <= Decimal::ZERO || book.mid_price <= Decimal::ZERO {
        return false;
    }
    let wick = (book.mid_price - book.best_bid) / book.mid_price;
    wick <= cfg.max_bid_wick_pct / Decimal::ONE_HUNDRED
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
pub fn decide_exit(input: ExitTickInput) -> Option<ExitDecision> {
    let ExitTickInput { entry_price, book, fallback_price, time_left_sec, hold_sec, state, now_ms, cfg } = input;
    if entry_price <= Decimal::ZERO {
        return None;
    }

    let bid = executable_bid(book);
    let usable = if bid > Decimal::ZERO {
        bid
    } else {
        fallback_price.filter(|p| *p > Decimal::ZERO).unwrap_or(Decimal::ZERO)
    };

    // 1. Force exit — absolute deadline; must fire even without a fresh book.
    if time_left_sec <= cfg.force_exit_sec {
        if usable <= Decimal::ZERO {
            return None;
        }
        return Some(ExitDecision { reason: ExitReason::ForceExit, use_maker: false });
    }

    if bid <= Decimal::ZERO {
        return None;
    }
    let pct = pnl_pct(bid, entry_price);

    if cfg.simple_exit_enabled {
        if cfg.take_profit_pct > Decimal::ZERO
            && cfg.take_profit_pct < dec!(9999)
            && pct >= cfg.take_profit_pct
        {
            return Some(ExitDecision { reason: ExitReason::TakeProfit, use_maker: cfg.maker_first_exit_enabled });
        }
        if pct <= -effective_stop_pct(cfg.stop_loss_pct, time_left_sec, cfg)
            && book.map(|b| bid_confirmed_by_mid(b, cfg)).unwrap_or(true)
        {
            return Some(ExitDecision { reason: ExitReason::StopLoss, use_maker: false });
        }
        if cfg.trailing_enabled && state.high_pnl_pct >= cfg.trailing_min_high_pct {
            let profit_trail = get_profit_trail_pct(state.high_pnl_pct, cfg);
            let time_trail = get_time_trail_pct(time_left_sec);
            let trail = cfg.min_trail_pct.max(profit_trail.min(time_trail));
            if state.high_pnl_pct - pct >= trail {
                return Some(ExitDecision { reason: ExitReason::TrailingStop, use_maker: false });
            }
        }
        if time_left_sec <= cfg.min_time_left_sec {
            return Some(ExitDecision { reason: ExitReason::TimeExit, use_maker: false });
        }
        return None;
    }

    // Full mode: grace period right after a maker fill.
    if hold_sec < cfg.exit_grace_sec {
        return None;
    }

    if pct >= cfg.take_profit_pct {
        return Some(ExitDecision { reason: ExitReason::TakeProfit, use_maker: cfg.maker_first_exit_enabled });
    }

    let base_stop = if cfg.tight_stop_enabled { cfg.tight_stop_pct } else { cfg.stop_loss_pct };
    let stop = effective_stop_pct(base_stop, time_left_sec, cfg);
    if pct <= -stop && book.map(|b| bid_confirmed_by_mid(b, cfg)).unwrap_or(true) {
        return Some(ExitDecision { reason: ExitReason::StopLoss, use_maker: false });
    }

    if cfg.ratchet_enabled {
        let confirmed_high_pct = pnl_pct(state.confirmed_high, entry_price);
        if pct <= get_ratchet_floor(confirmed_high_pct) {
            return Some(ExitDecision { reason: ExitReason::RatchetFloor, use_maker: false });
        }
    }

    if state.high_pnl_pct >= Decimal::from(BREAKEVEN_LOCK_TRIGGER_PCT) {
        let lock_floor = dec!(0.5).max(taker_fee_pct(bid) + dec!(0.2));
        if pct <= lock_floor {
            return Some(ExitDecision { reason: ExitReason::BreakevenLock, use_maker: false });
        }
    }

    if cfg.trailing_enabled && state.high_pnl_pct >= cfg.trailing_min_high_pct {
        let profit_trail = get_profit_trail_pct(state.high_pnl_pct, cfg);
        let time_trail = get_time_trail_pct(time_left_sec);
        let trail = cfg.min_trail_pct.max(profit_trail.min(time_trail));
        if state.high_pnl_pct - pct >= trail {
            return Some(ExitDecision { reason: ExitReason::TrailingStop, use_maker: false });
        }
    }

    if let Some(book) = book {
        if state.initial_depth > Decimal::ZERO {
            let current_depth = book.bid_depth + book.ask_depth;
            let depth_change = ((current_depth - state.initial_depth) / state.initial_depth)
                * Decimal::ONE_HUNDRED;
            if depth_change <= -cfg.depth_collapse_threshold_pct
                && bid < state.high_water_mark
                && pct >= dec!(2)
            {
                return Some(ExitDecision { reason: ExitReason::DepthCollapse, use_maker: false });
            }
        }
    }

    if pct >= cfg.stale_profit_pct {
        let stale_sec = (now_ms - state.bid_unchanged_since) / 1000;
        if stale_sec >= cfg.stale_profit_bid_unchanged_sec {
            return Some(ExitDecision { reason: ExitReason::StaleProfit, use_maker: true });
        }
    }

    if pct >= cfg.stagnant_profit_pct && pct < cfg.take_profit_pct {
        let stagnant_sec = (now_ms - state.last_progress_at) / 1000;
        if stagnant_sec >= cfg.stagnant_duration_sec {
            return Some(ExitDecision { reason: ExitReason::StagnantProfit, use_maker: true });
        }
    }

    if time_left_sec <= cfg.min_time_left_sec {
        return Some(ExitDecision { reason: ExitReason::TimeExit, use_maker: cfg.maker_exits_for_tp_only });
    }

    None
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

    #[test]
    fn force_exit_fires_at_deadline() {
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let b = book(0.39, 0.41);
        let d = decide_exit(ExitTickInput {
            entry_price: dec!(0.4), book: Some(&b), fallback_price: None,
            time_left_sec: 100, hold_sec: 50, state: &st, now_ms: 1000, cfg: &cfg,
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
            entry_price: dec!(0.4), book: Some(&b), fallback_price: None,
            time_left_sec: 600, hold_sec: 20, state: &st, now_ms: 1000, cfg: &cfg,
        }).unwrap();
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
            entry_price: dec!(0.4), book: Some(&b), fallback_price: None,
            time_left_sec: 600, hold_sec: 20, state: &st, now_ms: 1000, cfg: &cfg,
        })
        .unwrap();
        assert_eq!(d.reason, ExitReason::StopLoss);
        assert!(!d.use_maker);

        // An explicit wide stop (the old 50%) still holds through -30%.
        let mut wide = ExitConfig::default();
        wide.stop_loss_pct = dec!(50);
        assert!(decide_exit(ExitTickInput {
            entry_price: dec!(0.4), book: Some(&b), fallback_price: None,
            time_left_sec: 600, hold_sec: 20, state: &st, now_ms: 1000, cfg: &wide,
        }).is_none());
    }

    #[test]
    fn wick_below_mid_does_not_trigger_stop() {
        // bid 0.20 vs mid 0.40 → a 50% wick; protective stop must not fire on it.
        let cfg = ExitConfig::default();
        let st = ExitState::new(dec!(0.4), 0);
        let b = book(0.20, 0.60);
        assert!(decide_exit(ExitTickInput {
            entry_price: dec!(0.4), book: Some(&b), fallback_price: None,
            time_left_sec: 600, hold_sec: 20, state: &st, now_ms: 1000, cfg: &cfg,
        }).is_none());
    }
}
