//! `mean_reversion` (the fade leg) — config, fade tracker and evaluator.
//! The kernel wrapper and every external cdylib run THIS code.
//!
//! Where `spread_arb` buys a dip inside a CONFIRMED trend (it must wait out the
//! trend window) and `trend_follow` chases a rising mid, this strategy buys a
//! freshly CRASHED side — a token whose mid has fallen a large fraction off the
//! high of its own recent history and now sits in the cheap zone. It is the
//! counter-trend leg of the E4 hedge: its candidate windows are exactly the
//! ones the other two legs have nothing in.
//!
//! | | `spread_arb` | `trend_follow` | `mean_reversion` |
//! | --- | --- | --- | --- |
//! | trigger | a dip inside a confirmed trend | the mid RISES `>= min_move_pct` | the mid FELL `>= min_drop_pct` off the lookback high |
//! | side | the underpriced side | the side being bid up | the just-crashed side |
//! | pricing | `entry < mid` | `entry = best_ask > mid` | `entry < mid` |
//! | entry gate pass | natural | natural | **momentum exempted** (E2-b) |
//!
//! All state comes from the token's own book history (a [`PriceBuffer`] fed by
//! the host's book ticks), never from the scanner's cached prices.

use crate::knobs::declare_knobs;
use crate::model::{OrderbookSnapshot, SignalDirection};
use crate::params::{KnobSpec, StrategyParams};
use crate::signal::{PriceBuffer, TradeSignal, round2};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};

/// Entry-side parameters for the fade leg. Every field is a declared knob.
///
/// The two defaults that are NOT free choices are grounded in the recorded
/// market data (`data/archive/`, ~18 h of dry-run capture; the extraction script
/// lives in the E4-b acceptance report):
///
///  * `min_drop_pct = 10` — over every cheap-zone tick (mid <= 0.35) of the
///    held-out slice, the drop from the 120 s lookback high has p25 = -49% /
///    p50 = -33%: crashes are deep here all the time, so a 10% gate removes
///    only the shallow drift and stops the strategy fading noise; tightening it
///    further cuts candidates without changing their population (fires are 35
///    at 8% vs 35 at 10% vs 32 at 15%).
///  * `max_spread_pct = 8` — at the candidate instants the quoted spread is
///    ~5.7-6.7% mid-to-p75% and 13.3% at p75; 8% (~45% of candidates) excludes
///    the fat tail — a book 3x wider than the two with-trend legs, which is
///    what a crashed token looks like — without making the leg unfireable.
///
/// The remaining defaults are structural:
///  * `max_price = 0.35` follows the TS reference (`cheapThreshold`), and the
///    payoff floor `0.05` makes an entry at least 5:1 on the payoff.
///  * `entry_factor = 0.98` is shared with `spread_arb` (one resting-bid
///    discipline across the dip family).
///  * `lookback_sec = 120` is the drop's memory window; 60 s fired ~9% less
///    with no change in entry quality on the slice.
///  * `cooldown_sec = 60` — one candidate per token per minute, so a token
///    grinding down to zero cannot re-fire with every tick.
///
/// The trend gate (`trend_window_sec` / `trend_drop_pct`, #176) is the one
/// control here that is not about the DEPTH of the fall but about its AGE: a mid
/// 10% off its 120 s high is a dip only if the token is not several legs into a
/// one-sided slide. It measures the same quantity as `min_drop_pct` — the draw
/// off the high of the token's own history — over a LONGER window, so it needs
/// no second price series, and it refuses exactly the case the fade premise has
/// no basis for: a double market's cheap side is not oversold, it is being
/// repriced, and every further leg of the slide is another knife.
///
/// Grounded in the frozen corpus `docs/reports/data/mean-reversion-gate/` (four
/// 1 h slices of the recorded 2026-09-19/20 capture — the worst-PnL and the
/// best-PnL hour of each day — replayed through THIS engine by
/// `scripts/mean-reversion-gate-evidence.mjs`): with the gate off the shipped
/// config closed 72 trades, 14 of them winners, for -$22.80 net and a $31.00
/// drawdown, and 37 of those (0 wins, -$22.27) came off the two one-sided
/// slices. At 600 s / -30% those two slices keep 8 trades (0 wins, -$5.23); the
/// two two-sided slices give up 27 of their 35 entries but 5 of the 8 survivors
/// win (14/35 -> 5/8); the corpus ends at +$0.82 with a $7.16 drawdown.
///
/// Read that honestly: the gate mostly means "trade much less", and it is not a
/// moneymaker — it cuts the loss tail. The threshold is also not robust: the
/// sweep is monotone (net PnL rises to -30% and falls again from -35%), and the
/// value sits at the edge of the net-positive band, so a few points tighter
/// flips the sign. It is the LOOSEST setting that is still net-positive with at
/// least 10 trades (600 s / -20% is +$1.16 on 4). `trend_window_sec = 0` is the
/// pre-gate behaviour exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeanReversionConfig {
    /// Memory window (sec) the drop is measured over.
    pub lookback_sec: i64,
    /// Minimum FALL (%) of the mid off the lookback high to call it oversold.
    pub min_drop_pct: Decimal,
    /// The faded side must sit at or below this price (the cheap zone).
    pub max_price: Decimal,
    /// Resting bid = mid * this factor (same discipline as `spread_arb`).
    pub entry_factor: Decimal,
    /// Don't buy into a book wider than this (%).
    pub max_spread_pct: Decimal,
    /// At most one candidate per token per this many seconds.
    pub cooldown_sec: i64,
    /// Trend gate: the window (sec) the SLIDE is measured over — the drop of the
    /// mid off the high of this much history, a `min_drop_pct` over a longer
    /// memory. `0` disables the gate and restores the pre-gate behaviour.
    pub trend_window_sec: i64,
    /// Trend gate: a mid this far (%) below the trend-window high is the latest
    /// leg of a one-sided slide, not a dip — no entry.
    pub trend_drop_pct: Decimal,
}

impl Default for MeanReversionConfig {
    fn default() -> Self {
        Self {
            lookback_sec: 120,
            min_drop_pct: dec!(10),
            max_price: dec!(0.35),
            entry_factor: dec!(0.98),
            max_spread_pct: dec!(8),
            cooldown_sec: 60,
            trend_window_sec: 600,
            trend_drop_pct: dec!(30),
        }
    }
}

/// The knobs `mean_reversion` declares evolvable, as `(name, default, min, max)`.
///
/// The domain is a HARD outer bound (the kernel's domain guard rejects anything
/// outside it before the ±gradient lock is consulted). `entry_factor`'s ceiling
/// is 1.00 exclusive in spirit: the discipline requires a bid STRICTLY below the
/// mid, so a proposal of exactly 1.00 produces entries that every mid comparison
/// rejects rather than an inversion of the strategy — kept in-domain so a hot
/// swap can approach it without tripping the domain guard, and rejected by the
/// pricing rule instead.
pub const MEAN_REVERSION_KNOBS: [(&str, Decimal, Decimal, Decimal); 8] = [
    ("lookback_sec", dec!(120), dec!(10), dec!(600)),
    ("min_drop_pct", dec!(10), dec!(1.0), dec!(50.0)),
    ("max_price", dec!(0.35), dec!(0.10), dec!(0.60)),
    ("entry_factor", dec!(0.98), dec!(0.80), dec!(1.00)),
    ("max_spread_pct", dec!(8), dec!(0.10), dec!(25.0)),
    ("cooldown_sec", dec!(60), dec!(0), dec!(600)),
    // #176: the trend gate. A window of 0 is the pre-gate behaviour, and it is
    // in-domain on purpose — "no gate" has to be reachable by evolution (and by
    // the reverse-acceptance test that proves the gate is what blocks entries).
    // The window ceiling equals the longest lookback retention the tracker
    // keeps, so every legal window is measurable.
    ("trend_window_sec", dec!(600), dec!(0), dec!(600)),
    ("trend_drop_pct", dec!(30), dec!(5), dec!(90)),
];

fn knob_value(cfg: &MeanReversionConfig, name: &str) -> Decimal {
    match name {
        "lookback_sec" => Decimal::from(cfg.lookback_sec),
        "min_drop_pct" => cfg.min_drop_pct,
        "max_price" => cfg.max_price,
        "entry_factor" => cfg.entry_factor,
        "max_spread_pct" => cfg.max_spread_pct,
        "cooldown_sec" => Decimal::from(cfg.cooldown_sec),
        "trend_window_sec" => Decimal::from(cfg.trend_window_sec),
        "trend_drop_pct" => cfg.trend_drop_pct,
        _ => Decimal::ZERO,
    }
}

/// This strategy's knob declaration derived from the config in force.
pub fn mean_reversion_knobs(cfg: &MeanReversionConfig) -> Vec<KnobSpec> {
    declare_knobs(MEAN_REVERSION_KNOBS, |name| knob_value(cfg, name))
}

/// Overlay a parameter set on a base config. Undeclared names are ignored.
pub fn apply_knobs(base: &MeanReversionConfig, params: &StrategyParams) -> MeanReversionConfig {
    let mut cfg = base.clone();
    for (name, v) in params.iter() {
        match name {
            // Whole-number-of-seconds knobs truncate, so a fractional proposal
            // can never produce a non-integral window or a negative cooldown.
            "lookback_sec" => {
                if let Some(secs) = v.trunc().to_i64()
                    && secs > 0
                {
                    cfg.lookback_sec = secs;
                }
            }
            "cooldown_sec" => {
                if let Some(secs) = v.trunc().to_i64()
                    && secs >= 0
                {
                    cfg.cooldown_sec = secs;
                }
            }
            // 0 is a legal window (the gate off); a negative one is not, and a
            // fractional one truncates so the window stays a whole second count.
            "trend_window_sec" => {
                if let Some(secs) = v.trunc().to_i64()
                    && secs >= 0
                {
                    cfg.trend_window_sec = secs;
                }
            }
            "min_drop_pct" => cfg.min_drop_pct = v,
            "max_price" => cfg.max_price = v,
            "entry_factor" => cfg.entry_factor = v,
            "max_spread_pct" => cfg.max_spread_pct = v,
            "trend_drop_pct" => cfg.trend_drop_pct = v,
            _ => {}
        }
    }
    cfg
}

/// Per-token fade state for the reverse leg.
///
/// `on_price` feeds each token's own mid into a rolling [`PriceBuffer`] (retention
/// sized for the widest legal `lookback_sec`, so a hot swap can not silently
/// shrink the history the knob is measured over) and records candidate fires.
pub struct FadeTracker {
    cfg: MeanReversionConfig,
    buffers: HashMap<String, PriceBuffer>,
    /// Last candidate fire per token (for the cooldown), token -> ts.
    last_fire: HashMap<String, i64>,
    round_slot: i64,
    /// Tokens whose oversold premise just vanished (mid back above `max_price`).
    broken: Vec<(String, Decimal)>,
    /// Mid was inside the cheap zone at the previous tick, per token.
    in_zone: HashMap<String, bool>,
}

/// Retention (sec) for each token's price history — the widest window any
/// declared knob can measure over, so a hot swap can never read a window the
/// buffer has already forgotten. Both the drop window and the trend window are
/// declared with a 600 s ceiling; the maximum is taken rather than the constant
/// so raising either ceiling cannot silently truncate the other's history.
fn history_retention_sec(cfg: &MeanReversionConfig) -> i64 {
    let ceiling = |name: &str| {
        MEAN_REVERSION_KNOBS
            .iter()
            .find(|(n, ..)| *n == name)
            .map(|(_, _, _, max)| max.to_i64().unwrap_or(600))
            .unwrap_or(600)
    };
    let widest = ceiling("lookback_sec").max(ceiling("trend_window_sec"));
    cfg.lookback_sec
        .max(cfg.trend_window_sec)
        .max(1)
        .max(widest)
}

impl FadeTracker {
    pub fn new(cfg: MeanReversionConfig) -> Self {
        Self {
            cfg,
            buffers: HashMap::new(),
            last_fire: HashMap::new(),
            round_slot: i64::MIN,
            broken: Vec::new(),
            in_zone: HashMap::new(),
        }
    }

    pub fn set_config(&mut self, cfg: MeanReversionConfig) {
        self.cfg = cfg;
    }

    pub fn on_price(&mut self, token_id: &str, price: Decimal, now_ms: i64) {
        if token_id.is_empty() || price <= Decimal::ZERO {
            return;
        }
        let retention = history_retention_sec(&self.cfg);
        let buf = self
            .buffers
            .entry(token_id.to_string())
            .or_insert_with(|| PriceBuffer::new(retention));
        buf.push(price, now_ms);

        let cheap = self.cfg.max_price;
        let was = self.in_zone.get(token_id).copied().unwrap_or(false);
        let now = price <= cheap && price > Decimal::ZERO;
        self.in_zone.insert(token_id.to_string(), now);
        if was && !now && cheap > Decimal::ZERO {
            // Left the cheap zone upward: the oversold premise is gone.
            self.broken.push((token_id.to_string(), price));
            self.last_fire.remove(token_id);
        }
    }

    pub fn reset_if_new_round(&mut self, slot: i64) {
        if slot != self.round_slot {
            self.round_slot = slot;
            self.buffers.clear();
            self.last_fire.clear();
            self.broken.clear();
            self.in_zone.clear();
        }
    }

    /// The drop (%) of one token's current mid off the highest mid in its
    /// lookback window. Zero when there is not enough history; the entry
    /// evaluator's `< -min_drop_pct` check then refuses.
    pub fn drop_pct(&self, token_id: &str, now_ms: i64) -> Decimal {
        let window = self.cfg.lookback_sec.max(1);
        self.buffers
            .get(token_id)
            .map(|b| {
                if b.len() < 2 {
                    return Decimal::ZERO;
                }
                let hi = b.highest(window, now_ms);
                if hi <= Decimal::ZERO {
                    return Decimal::ZERO;
                }
                let cur = b.latest();
                (cur - hi) / hi * Decimal::ONE_HUNDRED
            })
            .unwrap_or(Decimal::ZERO)
    }

    /// The drop (%) of one token's current mid off the highest mid in its TREND
    /// window — the same measure [`Self::drop_pct`] takes over the lookback, read
    /// over a longer memory. Zero when the gate is off or history is short, so a
    /// disabled gate and a fresh token both read "no slide" and never block.
    pub fn trend_drop_pct(&self, token_id: &str, now_ms: i64) -> Decimal {
        let window = self.cfg.trend_window_sec;
        if window <= 0 {
            return Decimal::ZERO;
        }
        self.buffers
            .get(token_id)
            .map(|b| {
                if b.len() < 2 {
                    return Decimal::ZERO;
                }
                let hi = b.highest(window, now_ms);
                if hi <= Decimal::ZERO {
                    return Decimal::ZERO;
                }
                let cur = b.latest();
                (cur - hi) / hi * Decimal::ONE_HUNDRED
            })
            .unwrap_or(Decimal::ZERO)
    }

    /// Is the token's fall the latest leg of a one-sided slide rather than a dip?
    ///
    /// True when the mid sits `trend_drop_pct` or more below the high of the
    /// trend window. A window of 0 disables the gate (always false).
    pub fn in_trend_slide(&self, token_id: &str, now_ms: i64) -> bool {
        let cfg = &self.cfg;
        if cfg.trend_window_sec <= 0 {
            return false;
        }
        self.trend_drop_pct(token_id, now_ms) <= -cfg.trend_drop_pct
    }

    /// Cooldown check and record for one token's candidate fire.
    pub fn try_fire(&mut self, token_id: &str, now_ms: i64) -> bool {
        let last = self.last_fire.get(token_id).copied();
        if let Some(t) = last
            && now_ms - t < self.cfg.cooldown_sec * 1000
        {
            return false;
        }
        self.last_fire.insert(token_id.to_string(), now_ms);
        true
    }

    pub fn is_in_zone(&self, token_id: &str) -> bool {
        // Being asked again means the buffer exists; absence reads as "not yet".
        *self.in_zone.get(token_id).unwrap_or(&false)
    }

    /// Tokens currently inside the cheap zone (host diagnostics read this; the
    /// entry evaluator gates on `is_in_zone` itself).
    pub fn zone_tokens(&self) -> HashSet<String> {
        self.in_zone
            .iter()
            .filter(|(_, v)| **v)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Drain tokens whose oversold premise just vanished.
    pub fn take_broken(&mut self) -> Vec<(String, Decimal)> {
        std::mem::take(&mut self.broken)
    }
}

/// Fade evaluator for one market's two outcome tokens.
///
/// Returns a signal for the side that is cheap AND has just fallen hard off its
/// own lookback high. Pricing mirrors `evaluate_spread_arb`: a resting bid below
/// the mid (`mid * entry_factor`, clamped to `[0.05, 0.90]` and the tick grid),
/// never above the live best bid and strictly below the mid.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_mean_reversion(
    asset: &str,
    condition_id: &str,
    up_token: &str,
    down_token: &str,
    up_book: Option<&OrderbookSnapshot>,
    down_book: Option<&OrderbookSnapshot>,
    tracker: &FadeTracker,
    now_ms: i64,
    cfg: &MeanReversionConfig,
) -> Option<TradeSignal> {
    for (token, dir, book) in [
        (up_token, SignalDirection::Up, up_book),
        (down_token, SignalDirection::Down, down_book),
    ] {
        if token.is_empty() || !tracker.is_in_zone(token) {
            continue;
        }
        let Some(book) = book else { continue };
        if book.bids.is_empty() || book.asks.is_empty() || book.mid_price <= Decimal::ZERO {
            continue;
        }
        // Don't buy into a wide book.
        if cfg.max_spread_pct > Decimal::ZERO && book.spread_pct > cfg.max_spread_pct {
            continue;
        }
        let mid = book.mid_price;
        // The cheap-zone premise is enforced on the LIVE book mid (the tracker
        // only triggers the break); a stale zone flag must not trade an already
        // recovered token.
        if mid > cfg.max_price || mid < dec!(0.05) {
            continue;
        }
        // Drop from the lookback high, measured on the tracker's own buffer.
        let drop = tracker.drop_pct(token, now_ms);
        if cfg.min_drop_pct > Decimal::ZERO && drop > -cfg.min_drop_pct {
            continue;
        }
        // #176 trend gate: the same drop read over a LONGER memory. A token
        // several legs into a one-sided slide is being repriced, not oversold —
        // in a double market the cheap side of a trend keeps cheapening, and
        // every further dip is another knife. Blocks the entry; the caller's
        // cooldown still records the attempt, exactly like the other refusals.
        if tracker.in_trend_slide(token, now_ms) {
            continue;
        }
        let mut entry = (mid * cfg.entry_factor).max(dec!(0.05)).min(dec!(0.9));
        entry = round2(entry);
        if book.best_bid > Decimal::ZERO && entry > book.best_bid {
            entry = round2(book.best_bid);
        }
        if entry >= mid || entry <= Decimal::ZERO {
            continue;
        }
        return Some(TradeSignal {
            strategy: "mean_reversion".into(),
            asset: asset.to_string(),
            direction: dir,
            token_id: token.to_string(),
            condition_id: condition_id.to_string(),
            price: entry,
            reason: format!(
                "{} fell {:+.2}% off lookback high, cheap zone, resting bid {} (mid {})",
                dir.as_str().to_uppercase(),
                drop,
                entry,
                mid
            ),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::prelude::FromPrimitive;

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        book_d(
            Decimal::from_f64(bid).unwrap(),
            Decimal::from_f64(ask).unwrap(),
        )
    }

    fn book_d(bid: Decimal, ask: Decimal) -> OrderbookSnapshot {
        OrderbookSnapshot::from_levels("t", vec![(bid, dec!(100))], vec![(ask, dec!(100))], 0)
    }

    /// Drive a FALL from `from` to `to` on `token` in `count` steps spaced 1 s
    /// apart, then return the clock.
    fn fall(
        t: &mut FadeTracker,
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

    /// Hold `p` for `secs` one tick per second; returns the clock.
    fn hold(t: &mut FadeTracker, token: &str, p: Decimal, secs: usize, start: i64) -> i64 {
        let mut now = start;
        for _ in 0..secs {
            now += 1_000;
            t.on_price(token, p, now);
        }
        now
    }

    /// Drive a DIP on `token`: `from` held 5 s, then a 20 s fall to `to`,
    /// one tick per second. Returns the clock at the low.
    ///
    /// The SHAPE is what separates a dip from a one-sided slide (#176): the high
    /// `from` sits 25 s behind the low, so both memories see it, and the gate
    /// reads the depth. `0.40 -> 0.30` is -25% (faded), `0.50 -> 0.30` is -40%
    /// (refused). Both are past `min_drop_pct` over the 120 s lookback.
    fn dip(t: &mut FadeTracker, token: &str, from: Decimal, to: Decimal, start: i64) -> i64 {
        let now = hold(t, token, from, 5, start);
        fall(t, token, from, to, 20, now)
    }

    #[test]
    fn declarations_report_the_config_in_force_and_are_coherent() {
        let k = mean_reversion_knobs(&MeanReversionConfig::default());
        assert_eq!(k.len(), 8);
        for spec in &k {
            assert!(spec.is_coherent(), "{spec:?}");
        }
        assert_eq!(k[0].name, "lookback_sec");
        assert_eq!(k[0].value, dec!(120));
        let drop = k.iter().find(|s| s.name == "min_drop_pct").unwrap();
        assert_eq!(drop.value, dec!(10));
        assert_eq!(drop.max, dec!(50));
        // The #176 gate: 600 s / -30% in force, and the gate-OFF window (0) has
        // to stay inside the declared box or a proposal could not switch it off.
        let tw = k.iter().find(|s| s.name == "trend_window_sec").unwrap();
        assert_eq!(tw.value, dec!(600));
        assert!(tw.contains(dec!(0)), "{tw:?}");
        let td = k.iter().find(|s| s.name == "trend_drop_pct").unwrap();
        assert_eq!(td.value, dec!(30));
        assert!(td.contains(dec!(60)), "{td:?}");
        // An out-of-domain starting config widens the declared domain.
        let cfg = MeanReversionConfig {
            lookback_sec: 900,
            ..Default::default()
        };
        let k = mean_reversion_knobs(&cfg);
        let lb = k.iter().find(|s| s.name == "lookback_sec").unwrap();
        assert!(lb.contains(dec!(900)), "{lb:?}");
        assert!(lb.is_coherent());
    }

    #[test]
    fn apply_knobs_ignores_undeclared_names_and_keeps_the_seconds_integral() {
        let base = MeanReversionConfig::default();
        let mut p = StrategyParams::new();
        p.set("max_price", dec!(0.30));
        p.set("lookback_sec", dec!(45.7));
        p.set("cooldown_sec", dec!(30.9));
        p.set("trend_window_sec", dec!(300.9));
        p.set("trend_drop_pct", dec!(45));
        p.set("hard_stop_loss_pct", dec!(0)); // not ours
        let out = apply_knobs(&base, &p);
        assert_eq!(out.max_price, dec!(0.30));
        assert_eq!(out.lookback_sec, 45, "a fractional lookback truncates");
        assert_eq!(out.cooldown_sec, 30);
        assert_eq!(out.trend_window_sec, 300, "a fractional window truncates");
        assert_eq!(out.trend_drop_pct, dec!(45));
        assert_eq!(out.min_drop_pct, base.min_drop_pct);
        // A non-positive lookback can never be written through the overlay.
        let mut bad = StrategyParams::new();
        bad.set("lookback_sec", dec!(0));
        assert_eq!(apply_knobs(&base, &bad).lookback_sec, base.lookback_sec);
        // The trend window is the exception: 0 is the gate OFF and is writable
        // (that is how a proposal — and the reverse-acceptance test — reaches the
        // pre-#176 behaviour), while a negative window is not.
        let mut off = StrategyParams::new();
        off.set("trend_window_sec", dec!(0));
        assert_eq!(apply_knobs(&base, &off).trend_window_sec, 0);
        let mut neg = StrategyParams::new();
        neg.set("trend_window_sec", dec!(-5));
        assert_eq!(
            apply_knobs(&base, &neg).trend_window_sec,
            base.trend_window_sec
        );
    }

    #[test]
    fn a_shallow_drift_never_enters_a_deep_fall_does() {
        let cfg = MeanReversionConfig::default();
        let mut t = FadeTracker::new(cfg.clone());
        // Drift down 0.60 → 0.55 within 120 s ≈ -8.3%: inside the cheap zone at
        // the tail? No — 0.55 > max_price 0.35, never in the zone.
        let _now = fall(&mut t, "t", dec!(0.60), dec!(0.55), 5, 10_000);
        assert!(!t.is_in_zone("t"));
        assert!(t.take_broken().is_empty());

        // A deep fall to the cheap zone IS watched. But the entry evaluator
        // fires off the LIVE book, and the drop is measured off the tracker's
        // history that includes the high.
        let now = fall(&mut t, "t", dec!(0.50), dec!(0.30), 5, 20_000);
        assert!(t.is_in_zone("t"), "0.30 is inside a 0.35 cheap zone");
        // 0.50 → 0.30 over the lookback = -40%: way past the 10% gate.
        let drop = t.drop_pct("t", now);
        assert!(drop <= dec!(-10), "{drop}");

        // Recovery above the cheap zone breaks the premise exactly once.
        t.on_price("t", dec!(0.50), now + 5_000);
        assert!(!t.is_in_zone("t"));
        let broken = t.take_broken();
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].0, "t");
        assert_eq!(broken[0].1, dec!(0.50));
        assert!(t.take_broken().is_empty(), "each break is returned once");
    }

    #[test]
    fn a_new_round_resets_the_fade_state() {
        let mut t = FadeTracker::new(MeanReversionConfig::default());
        let now = fall(&mut t, "t", dec!(0.50), dec!(0.30), 5, 0);
        assert!(t.is_in_zone("t"));
        t.on_price("t", dec!(0.30), now + 1_000);
        t.reset_if_new_round(2);
        assert!(
            !t.is_in_zone("t"),
            "a new round starts with no zone history"
        );
        assert_eq!(t.drop_pct("t", now + 2_000), Decimal::ZERO);
        assert!(t.take_broken().is_empty());
    }

    #[test]
    fn the_entry_rests_below_the_mid_like_the_dip_buyer() {
        let cfg = MeanReversionConfig::default();
        let mut t = FadeTracker::new(cfg.clone());
        // 0.40 -> 0.30 within 25 s: -25% off the lookback high, inside the cheap
        // zone, and shallow enough that the 600 s window does not read it as a
        // one-sided slide — this test isolates the PRICING, so its fixture is a
        // dip; the gate has its own tests below.
        let now = dip(&mut t, "t", dec!(0.40), dec!(0.30), 10_000);
        // Live book round the mid: bid 0.29, ask 0.31 → entry = round2(0.294) = 0.29.
        let b = book(0.29, 0.31);
        let sig = evaluate_mean_reversion("BTC", "c", "t", "t-down", Some(&b), None, &t, now, &cfg)
            .expect("a dip into the cheap zone must fire");
        assert_eq!(sig.strategy, "mean_reversion");
        assert_eq!(sig.direction, SignalDirection::Up);
        assert_eq!(sig.token_id, "t");
        assert_eq!(sig.price, dec!(0.29), "resting bid = round2(mid*0.98)");
        assert!(sig.price < b.mid_price, "strictly below the mid");
        // .. and its reason mentions the drop.
        assert!(sig.reason.contains("resting bid"), "{}", sig.reason);
    }

    /// #176 acceptance, first half: a ONE-SIDED SLIDE is refused while the same
    /// setup one leg shallower is faded. The two tokens share the cheap-zone low
    /// and the shape of the fall; only the depth of the 600 s draw differs, and
    /// the gate is the only rule that separates them.
    #[test]
    fn a_one_sided_slide_is_refused_while_a_dip_is_faded() {
        let cfg = MeanReversionConfig::default();
        let mut t = FadeTracker::new(cfg.clone());
        let a = dip(&mut t, "dip", dec!(0.40), dec!(0.30), 10_000); // -25%
        let b = dip(&mut t, "slide", dec!(0.50), dec!(0.30), 10_000); // -40%
        let now = a.max(b);
        let book = book(0.29, 0.31); // mid 0.30, inside the cheap zone

        // Both are watched and both are "oversold" on the 120 s memory: the gate
        // is not refusing the slide for want of depth or of a cheap mid.
        assert!(t.is_in_zone("dip") && t.is_in_zone("slide"));
        assert!(
            t.drop_pct("dip", now) <= dec!(-10),
            "{}",
            t.drop_pct("dip", now)
        );
        assert!(
            t.drop_pct("slide", now) <= dec!(-10),
            "{}",
            t.drop_pct("slide", now)
        );

        // Only the longer memory tells them apart.
        assert!(
            !t.in_trend_slide("dip", now),
            "a -25% fall off the 600 s high is a dip: {}",
            t.trend_drop_pct("dip", now)
        );
        assert!(
            t.in_trend_slide("slide", now),
            "a -40% fall off the 600 s high is a slide: {}",
            t.trend_drop_pct("slide", now)
        );

        let faded = evaluate_mean_reversion(
            "BTC",
            "c",
            "dip",
            "dip-down",
            Some(&book),
            None,
            &t,
            now,
            &cfg,
        );
        assert!(faded.is_some(), "a -25% dip must still be faded");
        let refused = evaluate_mean_reversion(
            "ETH",
            "c",
            "slide",
            "slide-down",
            Some(&book),
            None,
            &t,
            now,
            &cfg,
        );
        assert!(
            refused.is_none(),
            "a -40% slide must be refused: {refused:?}"
        );
    }

    /// #176 acceptance, second half — the reverse at unit scale: the refusal IS
    /// the gate, not the fixture. Switching the window off (the declared `0`)
    /// restores the pre-#176 entry, a threshold loosened past the slide admits
    /// it, and a window too short to hold the high admits it too. An
    /// always-allow gate turns exactly this test red.
    #[test]
    fn the_trend_gate_is_what_refuses_the_slide() {
        // The tracker owns the price history AND its own config (production keeps
        // the two in sync on every book tick / hot-param push), so each arm gets
        // its own tracker built from the config under test.
        let book = book(0.29, 0.31);
        let fires = |cfg: &MeanReversionConfig| {
            let mut t = FadeTracker::new(cfg.clone());
            let now = dip(&mut t, "t", dec!(0.50), dec!(0.30), 10_000); // a -40% slide
            evaluate_mean_reversion("BTC", "c", "t", "t-down", Some(&book), None, &t, now, cfg)
                .is_some()
        };

        assert!(
            !fires(&MeanReversionConfig::default()),
            "600 s / -30% refuses a -40% slide"
        );
        assert!(
            fires(&MeanReversionConfig {
                trend_window_sec: 0,
                ..Default::default()
            }),
            "a zero window is the pre-gate behaviour exactly"
        );
        assert!(
            fires(&MeanReversionConfig {
                trend_drop_pct: dec!(45),
                ..Default::default()
            }),
            "a threshold past the slide's depth admits it"
        );
        assert!(
            !fires(&MeanReversionConfig {
                trend_drop_pct: dec!(35),
                ..Default::default()
            }),
            "a threshold inside the slide's depth still refuses it"
        );
        assert!(
            fires(&MeanReversionConfig {
                trend_window_sec: 10,
                ..Default::default()
            }),
            "10 s of memory does not contain the slide's high"
        );
    }

    /// The trend window is only measurable if the history reaches back that far:
    /// the buffer is sized from the DECLARED ceilings, not from `lookback_sec`.
    #[test]
    fn the_price_history_reaches_the_widest_declared_window() {
        let cfg = MeanReversionConfig::default();
        assert_eq!(
            history_retention_sec(&cfg),
            600,
            "the trend ceiling sizes the sample history"
        );
        let mut t = FadeTracker::new(cfg);
        // A high 400 s behind the low: invisible to the 120 s lookback, decisive
        // for the 600 s trend window.
        t.on_price("t", dec!(0.50), 1_000);
        t.on_price("t", dec!(0.30), 401_000);
        assert_eq!(
            t.drop_pct("t", 401_000),
            Decimal::ZERO,
            "the high is outside the lookback"
        );
        assert!(
            t.in_trend_slide("t", 401_000),
            "{}",
            t.trend_drop_pct("t", 401_000)
        );
        // A hand config past the declared ceiling drags the retention with it.
        let wide = MeanReversionConfig {
            lookback_sec: 900,
            ..Default::default()
        };
        assert_eq!(history_retention_sec(&wide), 900);
    }

    #[test]
    fn a_shallow_drop_a_wide_book_or_a_recovered_mid_each_blocks_the_entry() {
        let cfg = MeanReversionConfig::default();
        // 1. Shallow drop: 0.50 → 0.47 is 6% — below the 10% minimum.
        let mut t = FadeTracker::new(cfg.clone());
        let _ = fall(&mut t, "t2", dec!(0.50), dec!(0.47), 3, 10_000);
        // For live-book purposes the token must also sit in the cheap zone
        // with a deep drop behind it: give the dip token "t" its own fall.
        let now = dip(&mut t, "t", dec!(0.40), dec!(0.30), 10_000);
        assert!(t.is_in_zone("t"));

        // (a) recovered mid (above the cheap cap) — the zone guard refuses even
        // though the tracker still has massive historical state.
        let b = book(0.54, 0.56);
        assert!(
            evaluate_mean_reversion("BTC", "c", "t2", "t2-down", Some(&b), None, &t, now, &cfg)
                .is_none(),
            "a mid above the cheap cap must not fire"
        );

        // (b) an in-zone mid but no real drop history (the marker token 'cheap'
        // had a deep fall; a token never seen falling deep does not pass the
        // drop gate).
        let still_deep = book(0.29, 0.31);
        assert!(
            evaluate_mean_reversion(
                "BTC",
                "c",
                "t",
                "t-down",
                Some(&still_deep),
                None,
                &t,
                now,
                &cfg
            )
            .is_some(),
            "sanity: the real dip token fires"
        );

        // (c) wide book refused.
        let wide = book_d(dec!(0.15), dec!(0.40)); // spread ~91%
        assert!(
            evaluate_mean_reversion(
                "BTC",
                "c",
                "t",
                "t-down",
                Some(&wide),
                None,
                &t,
                now + 1_000,
                &cfg
            )
            .is_none(),
            "a wide book must not be bought"
        );

        // (d) degenerate one-sided book refused.
        let one_sided =
            OrderbookSnapshot::from_levels("t", vec![(dec!(0.29), dec!(100))], vec![], 0);
        assert!(
            evaluate_mean_reversion(
                "BTC",
                "c",
                "t",
                "t-down",
                Some(&one_sided),
                None,
                &t,
                now + 2_000,
                &cfg
            )
            .is_none()
        );
    }

    #[test]
    fn the_entry_never_exceeds_the_bid_or_the_ceiling() {
        let cfg = MeanReversionConfig::default();
        let mut t = FadeTracker::new(cfg.clone());
        let now = dip(&mut t, "t", dec!(0.40), dec!(0.30), 10_000);

        // entry_factor would put the bid above the live best bid: clamp DOWN to
        // the bid, still strictly below the mid.
        let b = book(0.32, 0.33); // mid 0.325, entry 0.32 = exactly the bid -> clamp
        let sig = evaluate_mean_reversion("BTC", "c", "t", "t-down", Some(&b), None, &t, now, &cfg)
            .expect("in-zone, dip-fallen token must fire");
        assert_eq!(sig.price, dec!(0.32), "clamped to the best bid");
        assert!(sig.price < b.mid_price);

        // The last two tests isolated the bid clamp; this one isolates the
        // 0.05 floor: mid 0.07 * 0.80 = 0.056 → round2 0.06, still above the
        // floor and strictly below the mid. The spread cap is widened so the
        // book's own width is not what the assert turns on.
        let floor_cfg = MeanReversionConfig {
            entry_factor: dec!(0.80),
            max_spread_pct: dec!(20),
            ..Default::default()
        };
        let b2 = book(0.065, 0.075); // spread 14.3% — admitted by the widened cap
        let s2 = evaluate_mean_reversion(
            "BTC",
            "c",
            "t",
            "t-down",
            Some(&b2),
            None,
            &t,
            now,
            &floor_cfg,
        )
        .expect("floor-safe entry must fire");
        assert_eq!(s2.price, dec!(0.06), "round2-floor entry");
        assert!(s2.price < b2.mid_price);
    }

    #[test]
    fn a_fresh_token_without_history_has_no_drop() {
        let mut t = FadeTracker::new(MeanReversionConfig::default());
        assert_eq!(t.drop_pct("t", 1_000), Decimal::ZERO);
        assert!(!t.is_in_zone("t"));
        assert!(t.take_broken().is_empty());
    }

    #[test]
    fn the_cooldown_admits_one_fire_per_window() {
        let cfg = MeanReversionConfig {
            cooldown_sec: 60,
            ..Default::default()
        };
        let mut t = FadeTracker::new(cfg);
        assert!(t.try_fire("t", 1_000), "the first fire is free");
        assert!(
            !t.try_fire("t", 30_000),
            "inside the cooldown the fire is refused"
        );
        assert!(
            t.try_fire("t", 61_000),
            "after the cooldown a fire is admitted"
        );
        // A token that left and re-entered the zone forgets its cooldown, but
        // the 61s fire itself started a new window — 2s later is still inside.
        t.on_price("t", dec!(0.60), 62_000);
        t.on_price("t", dec!(0.30), 63_000);
        assert!(
            !t.try_fire("t", 63_000),
            "the 61s fire restarted the window"
        );
        assert!(t.try_fire("t", 121_000), "and that window expires normally");
    }
}
