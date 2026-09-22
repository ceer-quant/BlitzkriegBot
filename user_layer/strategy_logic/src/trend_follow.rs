//! `trend_follow` (the chase leg) — config, momentum tracker and evaluator.
//! The kernel wrapper and every external cdylib run THIS code.
//!
//! Where `spread_arb` buys a dip inside a confirmed trend (resting a bid
//! strictly BELOW the mid and waiting), this strategy buys the side that is
//! being bid UP — it lifts the offer and pays above the mid. The two are
//! structural inverses, which is what makes them a hedge:
//!
//! | | `spread_arb` | `trend_follow` |
//! | --- | --- | --- |
//! | trigger | a dip inside a confirmed trend | the token's mid RISES `>= min_move_pct` |
//! | side | the underpriced side | the side being bought up (with the move) |
//! | pricing | `entry < mid`, capped at the best bid | `entry = best_ask`, so `entry > mid` |
//! | ceiling | `trend_max_entry_price` (cheap) | `max_entry_price` (payoff floor) |
//!
//! All state comes from this token's own book history (a [`PriceBuffer`] fed by
//! the host's book ticks), never from the scanner's cached prices. Exits are
//! NOT this strategy's business: it produces no exit intents, so a filled
//! position is managed by the kernel's shared exit policy.

use crate::knobs::declare_knobs;
use crate::model::{OrderbookSnapshot, SignalDirection};
use crate::params::{KnobSpec, StrategyParams};
use crate::signal::{PriceBuffer, TradeSignal, round2};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};

/// Entry-side parameters for the chase leg. Every field is a declared knob.
///
/// The two defaults that are NOT free choices are grounded in the recorded
/// market data (`data/archive/`, ~18 h of dry-run capture), because both are
/// thresholds on a 0.01-tick binary book where "1%" can be less than one tick:
///
///  * `min_move_pct = 3.0` — at the 0.55 confirmation floor one tick is
///    `0.01/0.55 = 1.8%`, so a 1% threshold fires on a *single* uptick and is a
///    noise trigger, not a breakout detector. 3% demands a genuine multi-tick
///    rise (≈1.7 ticks at 0.55, ≈2.7 ticks at 0.90).
///  * `max_spread_pct = 3.0` — measured over the same corpus restricted to
///    `mid >= 0.55`: median quoted spread 1.4%, p90 3.1%. A 3% cap keeps ~90% of
///    observed books and refuses the fat tail; being a *relative* cap it also
///    tightens automatically as the price rises toward the payoff floor, which
///    is where a wide book does the most damage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendFollowConfig {
    /// Window (sec) over which the token's own mid move is measured.
    pub momentum_window_sec: i64,
    /// Minimum RISE (%) of the token's mid over the window to call it a breakout.
    pub min_move_pct: Decimal,
    /// The chased side must already be priced at least this high (confirmation).
    pub min_confirm_price: Decimal,
    /// Below this the move is dead and the resting entry bid is cancelled
    /// (hysteresis: strictly lower than `min_confirm_price`).
    pub break_price: Decimal,
    /// Never pay above this — the binary's payoff must still be asymmetric.
    pub max_entry_price: Decimal,
    /// Don't chase into a book this wide (%).
    pub max_spread_pct: Decimal,
}

impl Default for TrendFollowConfig {
    fn default() -> Self {
        Self {
            momentum_window_sec: 30,
            min_move_pct: dec!(3.0),
            min_confirm_price: dec!(0.55),
            break_price: dec!(0.45),
            max_entry_price: dec!(0.88),
            max_spread_pct: dec!(3.0),
        }
    }
}

/// The knobs `trend_follow` declares evolvable, as `(name, default, min, max)`.
///
/// The domain is a HARD outer bound: the kernel's domain guard rejects anything
/// outside it before the ±gradient lock is consulted, so no number of successive
/// steps can walk a knob past these edges.
///
/// `min_move_pct`'s floor is 0.5, not 0: below that, one tick *always* exceeds
/// the threshold for any price this strategy will consider, so the knob would
/// stop being a discriminator. The ceiling of 10 is a large move for a binary.
pub const TREND_FOLLOW_KNOBS: [(&str, Decimal, Decimal, Decimal); 6] = [
    ("momentum_window_sec", dec!(30), dec!(5), dec!(300)),
    ("min_move_pct", dec!(3.0), dec!(0.5), dec!(10.0)),
    ("min_confirm_price", dec!(0.55), dec!(0.50), dec!(0.95)),
    ("break_price", dec!(0.45), dec!(0.05), dec!(0.60)),
    ("max_entry_price", dec!(0.88), dec!(0.50), dec!(0.98)),
    ("max_spread_pct", dec!(3.0), dec!(0.10), dec!(20.0)),
];

fn knob_value(cfg: &TrendFollowConfig, name: &str) -> Decimal {
    match name {
        "momentum_window_sec" => Decimal::from(cfg.momentum_window_sec),
        "min_move_pct" => cfg.min_move_pct,
        "min_confirm_price" => cfg.min_confirm_price,
        "break_price" => cfg.break_price,
        "max_entry_price" => cfg.max_entry_price,
        "max_spread_pct" => cfg.max_spread_pct,
        _ => Decimal::ZERO,
    }
}

/// This strategy's knob declaration derived from the config in force, so it
/// reports reality rather than a compiled-in default.
pub fn trend_follow_knobs(cfg: &TrendFollowConfig) -> Vec<KnobSpec> {
    declare_knobs(TREND_FOLLOW_KNOBS, |name| knob_value(cfg, name))
}

/// Overlay a parameter set on a base config. Names this strategy did not declare
/// are ignored, so a stray key can never write a field it never opened.
pub fn apply_knobs(base: &TrendFollowConfig, params: &StrategyParams) -> TrendFollowConfig {
    let mut cfg = base.clone();
    for (name, v) in params.iter() {
        match name {
            // The window is a whole number of seconds in the config but travels
            // as a decimal like every other knob; truncate so a fractional
            // proposal can never produce a non-integral window.
            "momentum_window_sec" => {
                if let Some(secs) = v.trunc().to_i64()
                    && secs > 0
                {
                    cfg.momentum_window_sec = secs;
                }
            }
            "min_move_pct" => cfg.min_move_pct = v,
            "min_confirm_price" => cfg.min_confirm_price = v,
            "break_price" => cfg.break_price = v,
            "max_entry_price" => cfg.max_entry_price = v,
            "max_spread_pct" => cfg.max_spread_pct = v,
            _ => {}
        }
    }
    cfg
}

/// Per-token momentum state for the chase leg.
///
/// `on_price` feeds each token's own mid into a rolling [`PriceBuffer`] and
/// classifies it. Hysteresis mirrors `TrendTracker`: a token is CONFIRMED at or
/// above `min_confirm_price` while rising fast enough, and only BROKEN once it
/// falls below the (lower) `break_price` — so a shallow pullback inside a live
/// move does not flap the resting bid on and off.
pub struct MomentumTracker {
    cfg: TrendFollowConfig,
    buffers: HashMap<String, PriceBuffer>,
    confirmed: HashSet<String>,
    round_slot: i64,
    /// Tokens that just died; drained by the kernel to cancel their resting bids.
    broken: Vec<(String, Decimal)>,
}

/// Retention (sec) for each token's price history.
///
/// The buffer prunes by age on every push and has no resize hook, so it is sized
/// for the WIDEST window this knob can legally take — the declared domain's
/// ceiling, or the starting value when that already sits outside it. A hot swap
/// can then widen `momentum_window_sec` without the history silently shrinking
/// to a shorter span than the window it is measured over.
fn history_retention_sec(cfg: &TrendFollowConfig) -> i64 {
    let ceiling = TREND_FOLLOW_KNOBS[0].3.to_i64().unwrap_or(300);
    cfg.momentum_window_sec.max(1).max(ceiling)
}

impl MomentumTracker {
    pub fn new(cfg: TrendFollowConfig) -> Self {
        Self {
            cfg,
            buffers: HashMap::new(),
            confirmed: HashSet::new(),
            round_slot: i64::MIN,
            broken: Vec::new(),
        }
    }

    pub fn set_config(&mut self, cfg: TrendFollowConfig) {
        self.cfg = cfg;
    }

    pub fn on_price(&mut self, token_id: &str, price: Decimal, now_ms: i64) {
        if token_id.is_empty() || price <= Decimal::ZERO {
            return;
        }
        let window = self.cfg.momentum_window_sec.max(1);
        let retention = history_retention_sec(&self.cfg);
        let buf = self
            .buffers
            .entry(token_id.to_string())
            .or_insert_with(|| PriceBuffer::new(retention));
        buf.push(price, now_ms);

        if self.confirmed.contains(token_id) {
            // Regime change: the move that justified the chase is dead.
            if self.cfg.break_price > Decimal::ZERO && price < self.cfg.break_price {
                self.confirmed.remove(token_id);
                self.broken.push((token_id.to_string(), price));
            }
            return;
        }

        let move_pct = buf.move_pct(window, now_ms);
        if price >= self.cfg.min_confirm_price && move_pct >= self.cfg.min_move_pct {
            self.confirmed.insert(token_id.to_string());
        }
    }

    pub fn reset_if_new_round(&mut self, slot: i64) {
        if slot != self.round_slot {
            self.round_slot = slot;
            self.buffers.clear();
            self.confirmed.clear();
        }
    }

    pub fn is_confirmed(&self, token_id: &str) -> bool {
        self.confirmed.contains(token_id)
    }

    pub fn confirmed_tokens(&self) -> HashSet<String> {
        self.confirmed.clone()
    }

    /// The move (%) currently measured for a token, for the entry reason string.
    pub fn move_pct(&self, token_id: &str, now_ms: i64) -> Decimal {
        let window = self.cfg.momentum_window_sec.max(1);
        self.buffers
            .get(token_id)
            .map(|b| b.move_pct(window, now_ms))
            .unwrap_or(Decimal::ZERO)
    }

    /// Drain tokens whose move just died (the kernel cancels their bids).
    pub fn take_broken(&mut self) -> Vec<(String, Decimal)> {
        std::mem::take(&mut self.broken)
    }
}

/// Trend-following (chase) evaluator for one market's two outcome tokens.
///
/// Returns a signal on the side that is being bid up: its own mid must have
/// RISEN at least `min_move_pct` over the window and sit at or above
/// `min_confirm_price` (the tracker's job), the book must be fresh, two-sided
/// and no wider than `max_spread_pct`, and the lifted offer (`best_ask`, rounded
/// to the tick) must not exceed `max_entry_price`.
///
/// The pricing rule is the structural inverse of `evaluate_spread_arb`'s: this
/// one requires `entry > mid` (pay up) where that one requires `entry < mid`
/// (wait for a dip).
#[allow(clippy::too_many_arguments)]
pub fn evaluate_trend_follow(
    asset: &str,
    condition_id: &str,
    up_token: &str,
    down_token: &str,
    up_book: Option<&OrderbookSnapshot>,
    down_book: Option<&OrderbookSnapshot>,
    tracker: &MomentumTracker,
    now_ms: i64,
    cfg: &TrendFollowConfig,
) -> Option<TradeSignal> {
    for (token, dir, book) in [
        (up_token, SignalDirection::Up, up_book),
        (down_token, SignalDirection::Down, down_book),
    ] {
        if token.is_empty() || !tracker.is_confirmed(token) {
            continue;
        }
        let Some(book) = book else { continue };
        if book.bids.is_empty() || book.asks.is_empty() || book.mid_price <= Decimal::ZERO {
            continue;
        }
        // Don't chase into a wide book: paying a fat spread is the whole edge.
        if cfg.max_spread_pct > Decimal::ZERO && book.spread_pct > cfg.max_spread_pct {
            continue;
        }
        let mid = book.mid_price;
        // Lift the offer. A book whose ask is at or below the mid is degenerate
        // (crossed/one-sided) and is not something to chase.
        let entry = round2(book.best_ask);
        if entry <= mid || entry <= Decimal::ZERO {
            continue;
        }
        if entry > cfg.max_entry_price {
            continue;
        }
        let move_pct = tracker.move_pct(token, now_ms);
        return Some(TradeSignal {
            strategy: "trend_follow".into(),
            asset: asset.to_string(),
            direction: dir,
            token_id: token.to_string(),
            condition_id: condition_id.to_string(),
            price: entry,
            shares: None,
            reason: format!(
                "{} rising {:+.2}% in {}s (mid {} >= {}), lifted offer {} (cap {})",
                dir.as_str().to_uppercase(),
                move_pct,
                cfg.momentum_window_sec,
                mid,
                cfg.min_confirm_price,
                entry,
                cfg.max_entry_price
            ),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book_d(bid: Decimal, ask: Decimal) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels("t", vec![(bid, dec!(100))], vec![(ask, dec!(100))], 0)
    }

    /// Drive a RISE from `from` to `to` on `token` in `steps` steps, then return
    /// the clock. The move is read from the token's own buffer, so two samples
    /// far enough apart already span it.
    fn rise(
        t: &mut MomentumTracker,
        token: &str,
        from: Decimal,
        to: Decimal,
        steps: usize,
        start: i64,
    ) -> i64 {
        let mut now = start;
        for i in 0..steps {
            let p = from + (to - from) * Decimal::from(i as u32) / Decimal::from(steps as u32 - 1);
            now += 1_000;
            t.on_price(token, p, now);
        }
        now
    }

    #[test]
    fn declarations_report_the_config_in_force_and_are_coherent() {
        let k = trend_follow_knobs(&TrendFollowConfig::default());
        assert_eq!(k.len(), 6);
        for spec in &k {
            assert!(spec.is_coherent(), "{spec:?}");
        }
        assert_eq!(k[0].name, "momentum_window_sec");
        assert_eq!(k[0].value, dec!(30));
        let cap = k.iter().find(|s| s.name == "max_entry_price").unwrap();
        assert_eq!(cap.value, dec!(0.88));
        assert_eq!(cap.max, dec!(0.98));
        // An out-of-domain starting config widens the declared domain instead of
        // declaring a box the strategy cannot fit in.
        let cfg = TrendFollowConfig {
            momentum_window_sec: 900,
            ..Default::default()
        };
        let k = trend_follow_knobs(&cfg);
        let win = k.iter().find(|s| s.name == "momentum_window_sec").unwrap();
        assert!(win.contains(dec!(900)), "{win:?}");
        assert!(win.is_coherent());
    }

    #[test]
    fn apply_knobs_ignores_undeclared_names_and_keeps_the_window_integral() {
        let base = TrendFollowConfig::default();
        let mut p = StrategyParams::new();
        p.set("max_entry_price", dec!(0.80));
        p.set("momentum_window_sec", dec!(45.7));
        p.set("hard_stop_loss_pct", dec!(0)); // not ours
        let out = apply_knobs(&base, &p);
        assert_eq!(out.max_entry_price, dec!(0.80));
        assert_eq!(out.momentum_window_sec, 45, "a fractional window truncates");
        assert_eq!(out.min_move_pct, base.min_move_pct);
        // A non-positive window can never be written through the overlay.
        let mut bad = StrategyParams::new();
        bad.set("momentum_window_sec", dec!(0));
        assert_eq!(
            apply_knobs(&base, &bad).momentum_window_sec,
            base.momentum_window_sec
        );
    }

    #[test]
    fn a_rise_confirms_and_a_fall_below_the_break_price_cancels() {
        let cfg = TrendFollowConfig::default();
        let mut t = MomentumTracker::new(cfg.clone());
        // Flat: no move → not confirmed.
        for i in 0..5 {
            t.on_price("t", dec!(0.60), i * 1_000);
        }
        assert!(!t.is_confirmed("t"), "a flat mid is not a breakout");
        // Rise 0.50 → 0.62 (+24%) → confirmed.
        let now = rise(&mut t, "t", dec!(0.50), dec!(0.62), 5, 10_000);
        assert!(t.is_confirmed("t"));
        assert!(t.confirmed_tokens().contains("t"));
        // Shallow pullback stays confirmed (hysteresis: break_price 0.45 < 0.55).
        t.on_price("t", dec!(0.50), now + 1_000);
        assert!(
            t.is_confirmed("t"),
            "a pullback inside the band must not flap"
        );
        assert!(t.take_broken().is_empty());
        // Below the break price → broken, drained exactly once.
        t.on_price("t", dec!(0.40), now + 2_000);
        assert!(!t.is_confirmed("t"));
        let broken = t.take_broken();
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].0, "t");
        assert_eq!(broken[0].1, dec!(0.40));
        assert!(t.take_broken().is_empty(), "each break is returned once");
    }

    #[test]
    fn a_new_round_resets_the_chase_state() {
        let mut t = MomentumTracker::new(TrendFollowConfig::default());
        let now = rise(&mut t, "t", dec!(0.50), dec!(0.62), 5, 0);
        assert!(t.is_confirmed("t"));
        t.on_price("t", dec!(0.70), now + 1_000);
        t.reset_if_new_round(2);
        assert!(!t.is_confirmed("t"), "a new round starts flat");
        assert_eq!(t.move_pct("t", now + 2_000), Decimal::ZERO);
    }

    #[test]
    fn the_entry_lifts_the_offer_and_is_the_inverse_of_the_dip_buyer() {
        let cfg = TrendFollowConfig::default();
        let mut t = MomentumTracker::new(cfg.clone());
        let now = rise(&mut t, "t", dec!(0.50), dec!(0.62), 5, 0);
        // mid 0.625, ask 0.63 → entry 0.63, strictly ABOVE the mid: paying up.
        let b = book_d(dec!(0.62), dec!(0.63));
        let sig = evaluate_trend_follow("BTC", "c", "t", "t-down", Some(&b), None, &t, now, &cfg)
            .expect("a rising, favoured, tight book is chaseable");
        assert_eq!(sig.strategy, "trend_follow");
        assert_eq!(sig.direction, SignalDirection::Up);
        assert_eq!(sig.price, dec!(0.63));
        assert!(
            sig.price > b.mid_price,
            "{} must be above mid {}",
            sig.price,
            b.mid_price
        );

        // The same book through spread_arb's rule is rejected: its entry is
        // capped at the best bid, which is BELOW mid.
        let mut confirmed = HashSet::new();
        confirmed.insert("t".to_string());
        let arb = crate::signal::evaluate_spread_arb(
            "BTC",
            "c",
            "t",
            "t-down",
            Some(&b),
            None,
            &confirmed,
            &crate::signal::SpreadArbConfig::default(),
        );
        assert!(arb.is_none(), "the dip buyer must not chase this book");
    }

    #[test]
    fn an_unconfirmed_or_untraded_side_produces_nothing() {
        let cfg = TrendFollowConfig::default();
        let t = MomentumTracker::new(cfg.clone());
        let b = book_d(dec!(0.62), dec!(0.63));
        // Nothing confirmed yet.
        assert!(evaluate_trend_follow("BTC", "c", "t", "d", Some(&b), None, &t, 0, &cfg).is_none());
        // Confirmed but no fresh book for that token.
        let mut t2 = MomentumTracker::new(cfg.clone());
        let now = rise(&mut t2, "t", dec!(0.50), dec!(0.62), 5, 0);
        assert!(evaluate_trend_follow("BTC", "c", "t", "d", None, None, &t2, now, &cfg).is_none());
    }

    #[test]
    fn a_wide_book_is_not_chased() {
        let cfg = TrendFollowConfig::default();
        let mut t = MomentumTracker::new(cfg.clone());
        let now = rise(&mut t, "t", dec!(0.50), dec!(0.62), 5, 0);
        // mid 0.60, spread 0.04 → 6.7% > max_spread_pct 3.0 → refuse to chase.
        let wide = book_d(dec!(0.58), dec!(0.62));
        assert!(
            evaluate_trend_follow("BTC", "c", "t", "d", Some(&wide), None, &t, now, &cfg).is_none()
        );
        // Tighten the cap and the same book becomes acceptable — the knob is the
        // only thing that changed.
        let loose = TrendFollowConfig {
            max_spread_pct: dec!(10),
            ..cfg.clone()
        };
        assert!(
            evaluate_trend_follow("BTC", "c", "t", "d", Some(&wide), None, &t, now, &loose)
                .is_some()
        );
    }

    #[test]
    fn the_payoff_floor_refuses_to_chase_into_no_edge() {
        let cfg = TrendFollowConfig::default();
        let mut t = MomentumTracker::new(cfg.clone());
        let now = rise(&mut t, "t", dec!(0.80), dec!(0.92), 5, 0);
        // ask 0.93 > max_entry_price 0.88 → paying that leaves ~7% for a 100% risk.
        let rich = book_d(dec!(0.92), dec!(0.93));
        assert!(
            evaluate_trend_follow("BTC", "c", "t", "d", Some(&rich), None, &t, now, &cfg).is_none()
        );
        // A book inside the cap is taken.
        let ok = book_d(dec!(0.85), dec!(0.86));
        assert!(
            evaluate_trend_follow("BTC", "c", "t", "d", Some(&ok), None, &t, now, &cfg).is_some()
        );
    }
}
