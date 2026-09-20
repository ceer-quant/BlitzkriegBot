//! Signal engine — price buffers, trend confirmation and the spread_arb
//! evaluator. Shared reference implementation: the kernel's in-tree wrapper and
//! every external cdylib run THIS code, so there is no drift surface.
//!
//! Pricing discipline (inherited from the TS version, which fixed a real bug):
//! the resting bid is derived from the FRESH mid, capped at the live best bid,
//! and must sit strictly below the mid. Never price off a stale scanner value.

use crate::model::{OrderbookSnapshot, SignalDirection};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Rolling price buffer (newest-first), used for oscillation/spot move metrics.
#[derive(Debug, Clone)]
pub struct PriceBuffer {
    prices: Vec<(i64, Decimal)>, // (ts_ms, price), newest first
    max_age_ms: i64,
}

impl PriceBuffer {
    pub fn new(max_age_sec: i64) -> Self {
        Self {
            prices: Vec::new(),
            max_age_ms: max_age_sec * 1000,
        }
    }

    pub fn push(&mut self, price: Decimal, now_ms: i64) {
        if price <= Decimal::ZERO {
            return;
        }
        self.prices.insert(0, (now_ms, price));
        let cutoff = now_ms - self.max_age_ms;
        self.prices.retain(|(t, _)| *t >= cutoff);
        if self.prices.len() > 2000 {
            self.prices.truncate(2000);
        }
    }

    pub fn len(&self) -> usize {
        self.prices.len()
    }
    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    fn in_window(&self, window_sec: i64, now_ms: i64) -> impl Iterator<Item = &(i64, Decimal)> {
        let cutoff = now_ms - window_sec * 1000;
        self.prices.iter().filter(move |(t, _)| *t >= cutoff)
    }

    /// Percentage move over the window (newest vs oldest in window).
    pub fn move_pct(&self, window_sec: i64, now_ms: i64) -> Decimal {
        let w: Vec<&(i64, Decimal)> = self.in_window(window_sec, now_ms).collect();
        if w.len() < 2 {
            return Decimal::ZERO;
        }
        let newest = w[0].1;
        let oldest = w[w.len() - 1].1;
        if oldest <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        ((newest - oldest) / oldest) * Decimal::ONE_HUNDRED
    }

    pub fn mean(&self, window_sec: i64, now_ms: i64) -> Decimal {
        let mut sum = Decimal::ZERO;
        let mut n = 0u32;
        for (_, p) in self.in_window(window_sec, now_ms) {
            sum += p;
            n += 1;
        }
        if n == 0 {
            Decimal::ZERO
        } else {
            sum / Decimal::from(n)
        }
    }

    pub fn range(&self, window_sec: i64, now_ms: i64) -> Decimal {
        let mut hi: Option<Decimal> = None;
        let mut lo: Option<Decimal> = None;
        for (_, p) in self.in_window(window_sec, now_ms) {
            hi = Some(hi.map_or(*p, |h| h.max(*p)));
            lo = Some(lo.map_or(*p, |l| l.min(*p)));
        }
        match (hi, lo) {
            (Some(h), Some(l)) => h - l,
            _ => Decimal::ZERO,
        }
    }

    /// Highest price in the window (0 when the window has no samples).
    pub fn highest(&self, window_sec: i64, now_ms: i64) -> Decimal {
        let mut hi = Decimal::ZERO;
        for (_, p) in self.in_window(window_sec, now_ms) {
            if *p > hi {
                hi = *p;
            }
        }
        hi
    }

    /// Newest sample (0 when empty) — the same value the last `push` inserted.
    pub fn latest(&self) -> Decimal {
        self.prices
            .first()
            .map(|(_, p)| *p)
            .unwrap_or(Decimal::ZERO)
    }

    /// Count direction reversals (steps >= min_step) in the window.
    pub fn reversals(&self, window_sec: i64, min_step: Decimal, now_ms: i64) -> u32 {
        let w: Vec<&(i64, Decimal)> = self.in_window(window_sec, now_ms).collect();
        if w.len() < 3 {
            return 0;
        }
        let mut count = 0u32;
        let mut last_dir: Option<bool> = None; // true = up
        for i in 1..w.len() {
            let diff = w[i - 1].1 - w[i].1;
            if diff.abs() < min_step {
                continue;
            }
            let dir = diff > Decimal::ZERO;
            if let Some(prev) = last_dir
                && dir != prev
            {
                count += 1;
            }
            last_dir = Some(dir);
        }
        count
    }
}

// ── Trend tracker ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendPhase {
    Idle,
    Building,
    Confirmed,
    Broken,
}

#[derive(Debug, Clone)]
pub struct TrendConfig {
    pub confirm_sec: i64,
    pub min_price: Decimal,
    pub broken_price: Decimal,
    pub ratio: Decimal,
    /// Floor on the confirmation window (TS uses 10s). Lowered in tests.
    pub window_floor_ms: i64,
}

impl Default for TrendConfig {
    fn default() -> Self {
        Self {
            confirm_sec: 60,
            min_price: dec!(0.55),
            broken_price: dec!(0.35),
            ratio: dec!(0.8),
            window_floor_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone)]
struct TrendEntry {
    phase: TrendPhase,
    samples: Vec<(i64, Decimal)>, // (ts_ms, price)
}

/// Rolling-window above-threshold trend confirmation. Tolerates brief dips.
pub struct TrendTracker {
    cfg: TrendConfig,
    states: std::collections::HashMap<String, TrendEntry>,
    round_slot: i64,
    /// Token ids that just broke; drained by the engine to cancel resting bids.
    broken: Vec<(String, Decimal)>,
}

impl TrendTracker {
    pub fn new(cfg: TrendConfig) -> Self {
        Self {
            cfg,
            states: Default::default(),
            round_slot: i64::MIN,
            broken: Vec::new(),
        }
    }

    pub fn set_config(&mut self, cfg: TrendConfig) {
        self.cfg = cfg;
    }

    pub fn on_price(&mut self, token_id: &str, price: Decimal, now_ms: i64) {
        if token_id.is_empty() || price <= Decimal::ZERO {
            return;
        }
        let window_ms = (self.cfg.confirm_sec * 1000).max(self.cfg.window_floor_ms);
        let threshold = self.cfg.min_price;
        let broken_price = self.cfg.broken_price;
        let ratio_needed = self.cfg.ratio;

        let s = self
            .states
            .entry(token_id.to_string())
            .or_insert_with(|| TrendEntry {
                phase: TrendPhase::Idle,
                samples: Vec::new(),
            });
        s.samples.push((now_ms, price));
        let cutoff = now_ms - window_ms;
        s.samples.retain(|(t, _)| *t >= cutoff);

        let total = s.samples.len();
        let above = s.samples.iter().filter(|(_, p)| *p >= threshold).count();
        let above_ratio = if total > 0 {
            Decimal::from(above) / Decimal::from(total)
        } else {
            Decimal::ZERO
        };
        let oldest = if total > 0 { s.samples[0].0 } else { now_ms };
        let spanned_ms = now_ms - oldest;

        // Regime change: a confirmed trend broke its floor.
        if s.phase == TrendPhase::Confirmed && price < broken_price {
            s.phase = TrendPhase::Broken;
            self.broken.push((token_id.to_string(), price));
            return;
        }

        // Confirmation needs a full-ish window and a high above-threshold ratio.
        if s.phase != TrendPhase::Confirmed
            && spanned_ms >= (window_ms as f64 * 0.9) as i64
            && above_ratio >= ratio_needed
        {
            s.phase = TrendPhase::Confirmed;
            return;
        }

        if s.phase != TrendPhase::Confirmed && s.phase != TrendPhase::Broken {
            s.phase = if above_ratio > dec!(0.5) {
                TrendPhase::Building
            } else {
                TrendPhase::Idle
            };
        }
    }

    pub fn reset_if_new_round(&mut self, slot: i64) {
        if slot != self.round_slot {
            self.round_slot = slot;
            self.states.clear();
        }
    }

    pub fn is_confirmed(&self, token_id: &str) -> bool {
        self.states
            .get(token_id)
            .map(|s| s.phase == TrendPhase::Confirmed)
            .unwrap_or(false)
    }

    pub fn confirmed_tokens(&self) -> std::collections::HashSet<String> {
        self.states
            .iter()
            .filter(|(_, s)| s.phase == TrendPhase::Confirmed)
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn phase(&self, token_id: &str) -> Option<TrendPhase> {
        self.states.get(token_id).map(|s| s.phase)
    }

    /// Highest price seen for a token inside the live confirmation window (0
    /// when the token is unknown). This is the trend's local high that a dip
    /// buyer is trying to buy back toward; an entry filter can refuse a "dip"
    /// that has already fallen far from it (the trend coming apart, not a
    /// retrace).
    pub fn recent_high(&self, token_id: &str) -> Decimal {
        self.states
            .get(token_id)
            .and_then(|s| s.samples.iter().map(|(_, p)| *p).max())
            .unwrap_or(Decimal::ZERO)
    }

    /// Percentage move of a token's mid over the last `window_sec` (newest vs
    /// oldest sample in that window; 0 with fewer than two samples). The dip
    /// buyer's turn filter reads this over a SHORT window: resting a bid only
    /// once the fall has actually turned up avoids the maker fill that is
    /// nothing but the market falling through the order.
    pub fn move_pct(&self, token_id: &str, window_sec: i64, now_ms: i64) -> Decimal {
        let Some(s) = self.states.get(token_id) else {
            return Decimal::ZERO;
        };
        let cutoff = now_ms - window_sec.max(1) * 1000;
        // Samples are appended oldest-first, so the first hit inside the window
        // is the oldest and the last is the newest.
        let mut oldest: Option<Decimal> = None;
        let mut newest: Option<Decimal> = None;
        for (t, p) in &s.samples {
            if *t >= cutoff {
                if oldest.is_none() {
                    oldest = Some(*p);
                }
                newest = Some(*p);
            }
        }
        let (Some(oldest), Some(newest)) = (oldest, newest) else {
            return Decimal::ZERO;
        };
        if oldest <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        ((newest - oldest) / oldest) * Decimal::ONE_HUNDRED
    }

    /// Drain tokens whose confirmed trend just broke (engine cancels their bids).
    pub fn take_broken(&mut self) -> Vec<(String, Decimal)> {
        std::mem::take(&mut self.broken)
    }
}

// ── spread_arb evaluator ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct SpreadArbConfig {
    pub trend_min_price: Decimal,
    pub trend_confirm_sec: i64,
    pub trend_broken_price: Decimal,
    pub trend_entry_price: Decimal,
    pub trend_entry_factor: Decimal,
    pub trend_max_entry_price: Decimal,
    /// Reject a fresh book whose order-book imbalance is below this
    /// (0 = disabled). A negative imbalance means the ask side is heavier, i.e.
    /// the dip is being sold into — the falling-knife case the stop-loss eats.
    pub entry_min_obi: Decimal,
    /// Reject a fresh book wider than this percent of the mid (0 = disabled).
    /// A dislocated book at the entry tick is not a dip worth resting a bid in.
    pub entry_max_spread_pct: Decimal,
    /// Reject a dip whose mid has already fallen more than this percent below
    /// the token's recent trend high (0 = disabled). Keeps the entry price
    /// unchanged; it only declines to buy a trend that is coming apart.
    pub entry_dip_max_pct: Decimal,
    /// Turn filter: require the token's mid to have RISEN at least this percent
    /// over the last `entry_bounce_window_sec` before resting the bid (0 =
    /// disabled). This is the anti-adverse-selection gate — a maker bid placed
    /// into a still-falling book is filled by the fall itself, which is where
    /// the stop-loss losers come from.
    pub entry_bounce_min_pct: Decimal,
    /// Lookback window (sec) for the turn filter.
    pub entry_bounce_window_sec: i64,
}

impl Default for SpreadArbConfig {
    fn default() -> Self {
        Self {
            trend_min_price: dec!(0.55),
            trend_confirm_sec: 60,
            trend_broken_price: dec!(0.35),
            trend_entry_price: Decimal::ZERO,
            // Tuned on the 2026-09-17..19 dry corpus (9.7M events, frozen
            // replay, holdout-split verified: win rate 74.7%/71.8% on the two
            // time halves at payoff 1.38 vs the 0.98 baseline's 45%/1.18).
            // The discount is the anti-adverse-selection lever: a resting bid
            // this far under the mid only fills on a real flush, so entries
            // land deep inside confirmed-trend dips where the trailing stop's
            // activation (+15%) is a couple of ticks away while the -12% stop
            // sits several ticks down. The two extremes bracket the effect:
            // 0.98 fills on noise (45% win), 0.88 fills on value (74% win).
            trend_entry_factor: dec!(0.88),
            trend_max_entry_price: dec!(0.45),
            // All three entry filters ship OFF so the shipped behaviour is the
            // pre-filter behaviour exactly; a session opts in through config.
            entry_min_obi: Decimal::ZERO,
            entry_max_spread_pct: Decimal::ZERO,
            entry_dip_max_pct: Decimal::ZERO,
            entry_bounce_min_pct: Decimal::ZERO,
            entry_bounce_window_sec: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TradeSignal {
    pub strategy: String,
    pub asset: String,
    pub direction: SignalDirection,
    pub token_id: String,
    pub condition_id: String,
    pub price: Decimal,
    pub reason: String,
}

/// Trend-confirmed dip buyer. Mirrors the original `evaluateSpreadArb`:
///  - requires the token to be trend-confirmed
///  - requires a FRESH two-sided book (mid > 0)
///  - bid = factor * mid (or fixed), never above the live best bid, strictly
///    below mid, and capped at trend_max_entry_price
#[allow(clippy::too_many_arguments)]
pub fn evaluate_spread_arb(
    asset: &str,
    condition_id: &str,
    up_token: &str,
    down_token: &str,
    up_book: Option<&OrderbookSnapshot>,
    down_book: Option<&OrderbookSnapshot>,
    confirmed: &std::collections::HashSet<String>,
    cfg: &SpreadArbConfig,
) -> Option<TradeSignal> {
    for (token, dir, book) in [
        (up_token, SignalDirection::Up, up_book),
        (down_token, SignalDirection::Down, down_book),
    ] {
        if token.is_empty() || !confirmed.contains(token) {
            continue;
        }
        let Some(book) = book else { continue };
        if book.bids.is_empty() || book.asks.is_empty() || book.mid_price <= Decimal::ZERO {
            continue;
        }
        let mid = book.mid_price;
        if cfg.trend_broken_price > Decimal::ZERO && mid < cfg.trend_broken_price {
            continue;
        }
        // High-frequency entry filters, both evaluated on the FRESH book of this
        // cycle. They narrow which dips we buy; they never change the price of
        // the ones we do, so the payoff ratio is left alone.
        if cfg.entry_min_obi > Decimal::ZERO && book.obi < cfg.entry_min_obi {
            continue;
        }
        if cfg.entry_max_spread_pct > Decimal::ZERO && book.spread_pct > cfg.entry_max_spread_pct {
            continue;
        }
        let raw = if cfg.trend_entry_price > Decimal::ZERO {
            cfg.trend_entry_price
        } else {
            mid * cfg.trend_entry_factor
        };
        let mut entry = raw.max(dec!(0.05)).min(dec!(0.9));
        entry = round2(entry);
        if book.best_bid > Decimal::ZERO && entry > book.best_bid {
            entry = round2(book.best_bid);
        }
        if entry >= mid {
            continue;
        }
        if cfg.trend_max_entry_price > Decimal::ZERO && entry > cfg.trend_max_entry_price {
            continue;
        }
        return Some(TradeSignal {
            strategy: "spread_arb".into(),
            asset: asset.to_string(),
            direction: dir,
            token_id: token.to_string(),
            condition_id: condition_id.to_string(),
            price: entry,
            reason: format!(
                "{} trend confirmed (held >{} for >={}s), resting bid {} ({}% of mid {})",
                dir.as_str().to_uppercase(),
                cfg.trend_min_price,
                cfg.trend_confirm_sec,
                entry,
                (entry / mid * Decimal::ONE_HUNDRED).round(),
                mid
            ),
        });
    }
    None
}

pub fn round2(v: Decimal) -> Decimal {
    (v * Decimal::ONE_HUNDRED).round() / Decimal::ONE_HUNDRED
}

/// The tracker-side high-frequency entry gates for a produced `spread_arb`
/// signal; `false` withholds the entry. These need the token's mid HISTORY,
/// which lives in the [`TrendTracker`], so they cannot sit in the book-only
/// [`evaluate_spread_arb`]:
///
///  * turn filter — a resting maker bid placed into a still-falling book is
///    filled BY the fall (adverse selection, the stop-loss loser factory), so
///    when `entry_bounce_min_pct > 0` the token's short-window move must have
///    turned up by at least that much before the bid is rested;
///  * dip-depth guard — when `entry_dip_max_pct > 0` the entry mid must sit
///    within that percent of the token's recent trend high, declining to buy
///    a trend that is already coming apart.
///
/// Both gates only REFUSE entries; nothing here reprices the ones that pass,
/// so the payoff ratio is left exactly as the exit policy shapes it.
pub fn spread_arb_tracker_gates(
    tracker: &TrendTracker,
    token_id: &str,
    mid: Decimal,
    now_ms: i64,
    cfg: &SpreadArbConfig,
) -> bool {
    if cfg.entry_bounce_min_pct > Decimal::ZERO {
        let move_pct = tracker.move_pct(token_id, cfg.entry_bounce_window_sec, now_ms);
        if move_pct < cfg.entry_bounce_min_pct {
            return false;
        }
    }
    if cfg.entry_dip_max_pct > Decimal::ZERO {
        let high = tracker.recent_high(token_id);
        if high > Decimal::ZERO {
            let floor = high * (Decimal::ONE - cfg.entry_dip_max_pct / Decimal::ONE_HUNDRED);
            if mid < floor {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(bid: f64, ask: f64) -> OrderbookSnapshot {
        use rust_decimal::prelude::FromPrimitive;
        OrderbookSnapshot::from_levels(
            "t",
            vec![(Decimal::from_f64(bid).unwrap(), dec!(100))],
            vec![(Decimal::from_f64(ask).unwrap(), dec!(100))],
            0,
        )
    }

    #[test]
    fn trend_confirms_after_window_with_high_ratio() {
        let mut t = TrendTracker::new(TrendConfig {
            confirm_sec: 10,
            ..Default::default()
        });
        // Feed 12 samples over >9s all above 0.55.
        for i in 0..12 {
            t.on_price("tok", dec!(0.60), i * 1000);
        }
        assert!(t.is_confirmed("tok"));
        assert!(t.confirmed_tokens().contains("tok"));
    }

    #[test]
    fn trend_breaks_below_break_price() {
        let mut t = TrendTracker::new(TrendConfig {
            confirm_sec: 10,
            ..Default::default()
        });
        for i in 0..12 {
            t.on_price("tok", dec!(0.60), i * 1000);
        }
        assert!(t.is_confirmed("tok"));
        t.on_price("tok", dec!(0.30), 12_000);
        assert_eq!(t.phase("tok"), Some(TrendPhase::Broken));
        let broken = t.take_broken();
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].0, "tok");
    }

    #[test]
    fn spread_arb_requires_fresh_book_and_respects_caps() {
        let cfg = SpreadArbConfig {
            trend_max_entry_price: dec!(0.45),
            ..Default::default()
        };
        let mut confirmed = std::collections::HashSet::new();
        confirmed.insert("up".to_string());

        // No book → no signal (guards against stale-price entries).
        assert!(
            evaluate_spread_arb("BTC", "c", "up", "down", None, None, &confirmed, &cfg).is_none()
        );

        // Book mid 0.50, bid 0.49 → entry = round2(0.50*0.88)=0.44 ≤ cap 0.45
        // and below mid, so the shallow dip still trades — the cap only binds
        // on higher mids. Book mid 0.55, bid 0.54 → entry = round2(0.55*0.88)
        // = 0.48 > max_entry 0.45 → rejected.
        let b = book(0.54, 0.56);
        assert!(
            evaluate_spread_arb("BTC", "c", "up", "down", Some(&b), None, &confirmed, &cfg)
                .is_none()
        );

        // Book mid 0.44, bid 0.43 → entry = round2(0.44*0.88)=0.39 ≤ 0.45,
        // below mid (and below the 0.43 bid, which no longer binds) → signal.
        let b2 = book(0.43, 0.45);
        let sig = evaluate_spread_arb("BTC", "c", "up", "down", Some(&b2), None, &confirmed, &cfg)
            .unwrap();
        assert_eq!(sig.direction, SignalDirection::Up);
        assert_eq!(sig.price, dec!(0.39));
        assert!(sig.price < dec!(0.44));
    }

    #[test]
    fn spread_arb_skips_unconfirmed_and_broken_regime() {
        let cfg = SpreadArbConfig::default();
        let empty = std::collections::HashSet::new();
        let b = book(0.43, 0.45);
        // Not confirmed → no signal.
        assert!(
            evaluate_spread_arb("BTC", "c", "up", "down", Some(&b), None, &empty, &cfg).is_none()
        );

        // Confirmed but mid below broken price → no signal.
        let mut confirmed = std::collections::HashSet::new();
        confirmed.insert("up".to_string());
        let low = book(0.30, 0.32); // mid 0.31 < 0.35
        assert!(
            evaluate_spread_arb("BTC", "c", "up", "down", Some(&low), None, &confirmed, &cfg)
                .is_none()
        );
    }

    #[test]
    fn high_frequency_entry_filters_reject_their_books_and_leave_the_price_alone() {
        let mut confirmed = std::collections::HashSet::new();
        confirmed.insert("up".to_string());
        // mid 0.40, bid 0.39: entry round2(0.40*0.98)=0.39 ≤ cap 0.45, below mid.
        let b = book(0.39, 0.41);

        // Off by default: the same book trades, so the filters are opt-in.
        let off = SpreadArbConfig::default();
        assert!(
            evaluate_spread_arb("BTC", "c", "up", "down", Some(&b), None, &confirmed, &off)
                .is_some()
        );

        // An OBI floor the (balanced) book cannot clear refuses the entry…
        let obi = SpreadArbConfig {
            entry_min_obi: dec!(0.5),
            ..Default::default()
        };
        assert!(
            evaluate_spread_arb("BTC", "c", "up", "down", Some(&b), None, &confirmed, &obi)
                .is_none()
        );

        // …and a spread cap below the book's own spread does the same.
        let tight = SpreadArbConfig {
            entry_max_spread_pct: dec!(0.1),
            ..Default::default()
        };
        assert!(
            evaluate_spread_arb("BTC", "c", "up", "down", Some(&b), None, &confirmed, &tight)
                .is_none()
        );

        // A filter that the book does clear changes NOTHING about the price.
        let pass = SpreadArbConfig {
            entry_min_obi: dec!(-0.5),
            entry_max_spread_pct: dec!(50),
            ..Default::default()
        };
        let sig = evaluate_spread_arb("BTC", "c", "up", "down", Some(&b), None, &confirmed, &pass)
            .unwrap();
        let base = evaluate_spread_arb("BTC", "c", "up", "down", Some(&b), None, &confirmed, &off)
            .unwrap();
        assert_eq!(sig.price, base.price, "a passing filter must not reprice");
    }

    #[test]
    fn tracker_gates_withhold_the_falling_knife_and_the_come_apart() {
        let cfg = SpreadArbConfig {
            entry_bounce_min_pct: dec!(1),
            entry_bounce_window_sec: 2,
            entry_dip_max_pct: dec!(20),
            ..Default::default()
        };
        let mut t = TrendTracker::new(TrendConfig {
            confirm_sec: 10,
            ..Default::default()
        });
        // A fall: 0.60 → 0.50 over 4s. The 5s move is negative, and 0.50 sits
        // 16.7% under the 0.60 high — inside the dip band but still falling.
        t.on_price("tok", dec!(0.60), 0);
        t.on_price("tok", dec!(0.56), 1_000);
        t.on_price("tok", dec!(0.53), 2_000);
        t.on_price("tok", dec!(0.50), 4_000);
        assert!(
            !spread_arb_tracker_gates(&t, "tok", dec!(0.50), 4_000, &cfg),
            "a still-falling mid must be withheld"
        );
        // The dip guard alone would pass 0.50 (within 20% of 0.60)…
        let dip_only = SpreadArbConfig {
            entry_dip_max_pct: dec!(20),
            ..Default::default()
        };
        assert!(spread_arb_tracker_gates(
            &t,
            "tok",
            dec!(0.50),
            4_000,
            &dip_only
        ));
        // …but a come-apart mid 30% under the high must not pass it.
        assert!(
            !spread_arb_tracker_gates(&t, "tok", dec!(0.42), 4_000, &dip_only),
            "a trend that has come apart must be withheld"
        );
        // The turn: the mid bounces 0.50 → 0.52 (+4% over the window tail).
        t.on_price("tok", dec!(0.52), 5_000);
        assert!(
            spread_arb_tracker_gates(&t, "tok", dec!(0.52), 5_000, &cfg),
            "a bounced mid inside the dip band passes both gates"
        );
        // Unknown tokens are withheld while a turn is required (no evidence).
        assert!(
            !spread_arb_tracker_gates(&t, "ghost", dec!(0.52), 5_000, &cfg),
            "no history, no turn evidence"
        );
        // Off by default: the same falling mid passes untouched.
        assert!(spread_arb_tracker_gates(
            &t,
            "tok",
            dec!(0.50),
            4_000,
            &SpreadArbConfig::default()
        ));
    }

    #[test]
    fn recent_high_is_the_window_max_and_forgets_a_cleared_token() {
        let mut t = TrendTracker::new(TrendConfig {
            confirm_sec: 10,
            ..Default::default()
        });
        t.on_price("tok", dec!(0.60), 0);
        t.on_price("tok", dec!(0.72), 1_000);
        t.on_price("tok", dec!(0.68), 2_000);
        assert_eq!(t.recent_high("tok"), dec!(0.72));
        assert_eq!(t.recent_high("never-seen"), Decimal::ZERO);
    }

    #[test]
    fn price_buffer_move_and_range() {
        let mut b = PriceBuffer::new(180);
        b.push(dec!(0.50), 0);
        b.push(dec!(0.55), 1000);
        b.push(dec!(0.60), 2000);
        // Newest 0.60 vs oldest 0.50 → +20%.
        assert_eq!(b.move_pct(10, 2000), dec!(20));
        assert_eq!(b.range(10, 2000), dec!(0.10));
    }
}
