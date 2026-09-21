//! Dry-mode fill simulation, ported from the Node engine's dry semantics so a
//! DRY run exercises the exact same OME/ledger/risk code path as LIVE.
//!
//!  - Taker fills like a live FOK: it walks the opposing side's resting levels
//!    (best first, no worse than its limit) and fills at the volume-weighted
//!    price; if that side cannot cover the full size, the order is rejected —
//!    all-or-nothing, the same way the venue would kill it
//!  - Maker rests post-only and fills only when the live book crosses the limit
//!    (BUY when best ask <= limit, SELL when best bid >= limit)
//!  - MakerThenTaker rests as maker and is escalated by the service tick after
//!    maker_timeout_ms.
//!
//! Pure functions over the L2 snapshot so they are unit-testable.

use crate::model::{FillPolicy, Side, TrackedOrder};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Default)]
pub struct Book {
    /// (price, size); best-first ordering is enforced at insertion/query.
    pub bids: Vec<(Decimal, Decimal)>,
    pub asks: Vec<(Decimal, Decimal)>,
}

impl Book {
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.iter().map(|(p, _)| *p).max()
    }
    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.iter().map(|(p, _)| *p).min()
    }
    pub fn mid(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) if a > Decimal::ZERO => Some((b + a) / Decimal::TWO),
            _ => None,
        }
    }

    /// A resting maker order fills when the opposite side crosses its limit.
    pub fn crosses(&self, order: &TrackedOrder) -> bool {
        match order.side {
            Side::Buy => self.best_ask().map(|a| a <= order.price).unwrap_or(false),
            Side::Sell => self.best_bid().map(|b| b >= order.price).unwrap_or(false),
        }
    }

    /// The two questions [`Book::walk_marketable`] collapses into a single
    /// `None`, asked separately so a refusal can say WHICH one failed (#180):
    /// does any resting level cross `limit` at all, and how much size rests at
    /// or inside it.
    ///
    /// Pure and defined against the same crossing rule the walk uses, so the
    /// answer describes the walk that just failed rather than forming a second
    /// opinion about it.
    pub fn crossing_depth(&self, side: Side, limit: Decimal) -> (bool, Decimal) {
        let crosses = |p: Decimal| match side {
            Side::Buy => p <= limit,
            Side::Sell => p >= limit,
        };
        let levels = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };
        let mut any = false;
        let mut depth = Decimal::ZERO;
        for (price, size) in levels.iter() {
            if !crosses(*price) {
                continue;
            }
            any = true;
            if *size > Decimal::ZERO {
                depth += *size;
            }
        }
        (any, depth)
    }

    /// Walk the book as a taker would: consume resting levels, best price
    /// first, while they are no worse than `limit`, until `size` is covered.
    ///
    /// Returns the volume-weighted average fill price and the worst level
    /// touched (the price a live limit request must state to guarantee this
    /// walk), or `None` when the crossing side cannot cover `size` within
    /// `limit` — a live FOK would be killed, so the dry path rejects too.
    pub fn walk_marketable(
        &self,
        side: Side,
        limit: Decimal,
        size: Decimal,
    ) -> Option<(Decimal, Decimal)> {
        if size <= Decimal::ZERO {
            return None;
        }
        let mut levels = match side {
            Side::Buy => self.asks.clone(),
            Side::Sell => self.bids.clone(),
        };
        levels.sort();
        // Best first, per side: the best ask is the LOWEST ask, the best bid
        // is the HIGHEST bid. Ascending sort only serves the ask side; the bid
        // side must walk down from the top or a deep-but-worse level hides the
        // better ones behind an early `break`.
        if side == Side::Sell {
            levels.reverse();
        }
        let crosses = |p: Decimal| match side {
            Side::Buy => p <= limit,
            Side::Sell => p >= limit,
        };
        let mut remaining = size;
        let mut notional = Decimal::ZERO;
        for (price, level_size) in levels.iter() {
            if !crosses(*price) {
                break; // sorted best-first: nothing further out can cross
            }
            let take = (*level_size).min(remaining);
            notional += *price * take;
            remaining -= take;
            if remaining <= Decimal::ZERO {
                break;
            }
        }
        if remaining > Decimal::ZERO {
            return None;
        }
        let vwap = notional / size;
        let worst = levels
            .iter()
            .filter(|(p, s)| crosses(*p) && *s > Decimal::ZERO)
            .map(|(p, _)| *p)
            .reduce(|a, b| {
                if side == Side::Buy {
                    a.max(b)
                } else {
                    a.min(b)
                }
            });
        Some((vwap, worst?))
    }
}

pub fn rests_on_book(mode: FillPolicy) -> bool {
    matches!(mode, FillPolicy::Maker | FillPolicy::MakerThenTaker)
}

/// Tradable price grid on the binary markets (1 tick = 0.01).
fn tick() -> Decimal {
    Decimal::new(1, 2)
}
fn price_min() -> Decimal {
    Decimal::new(1, 2)
}
fn price_max() -> Decimal {
    Decimal::new(99, 2)
}

/// Fill-realism model for the dry matcher (P-1.2 backtesting).
///
/// The default is the **identity** — no slippage, no latency, always fill — i.e.
/// exactly the behaviour the dry path has always had, so live/dry runs are
/// unaffected. A backtest raises these knobs to price in the friction the venue
/// adds (crossing the spread, queue position, being slow):
///
///  - `taker_slippage_ticks`: taker fills cross N extra ticks (buys pay up, sells
///    give up), clamped to the tradable grid.
///  - `maker_latency_ms`: a resting maker order cannot fill until this long after
///    it was submitted.
///  - `maker_fill_prob_bps`: chance a crossing maker order actually fills. The
///    draw is a hash of the order id, so a given replay is reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FillModel {
    pub taker_slippage_ticks: u32,
    pub maker_latency_ms: i64,
    pub maker_fill_prob_bps: u32,
}

impl Default for FillModel {
    fn default() -> Self {
        Self {
            taker_slippage_ticks: 0,
            maker_latency_ms: 0,
            maker_fill_prob_bps: 10_000,
        }
    }
}

impl FillModel {
    /// True when the model cannot change any outcome (the default).
    pub fn is_identity(&self) -> bool {
        self.taker_slippage_ticks == 0
            && self.maker_latency_ms <= 0
            && self.maker_fill_prob_bps >= 10_000
    }

    /// Price a taker fill would actually get, given the side.
    pub fn apply_slippage(&self, side: Side, price: Decimal) -> Decimal {
        if self.taker_slippage_ticks == 0 {
            return price;
        }
        let slip = tick() * Decimal::from(self.taker_slippage_ticks);
        let p = match side {
            Side::Buy => price + slip,
            Side::Sell => price - slip,
        };
        p.max(price_min()).min(price_max())
    }

    /// Earliest time a maker order may fill.
    pub fn maker_eligible_at_ms(&self, submitted_at_ms: i64) -> i64 {
        submitted_at_ms.saturating_add(self.maker_latency_ms.max(0))
    }

    /// Whether a crossing maker order fills (deterministic per order id).
    pub fn maker_fill_wins(&self, order_id: &str) -> bool {
        if self.maker_fill_prob_bps >= 10_000 {
            return true;
        }
        if self.maker_fill_prob_bps == 0 {
            return false;
        }
        draw(order_id) % 10_000 < self.maker_fill_prob_bps
    }
}

/// FNV-1a over the order id → a stable pseudo-random draw.
fn draw(order_id: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in order_id.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use rust_decimal_macros::dec;

    fn order(side: Side, price: Decimal, mode: FillPolicy) -> TrackedOrder {
        TrackedOrder {
            order_id: "o".into(),
            internal_key: "k".into(),
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            token_id: "t".into(),
            condition_id: "c".into(),
            side,
            mode,
            price,
            size: dec!(10),
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            status: OrderStatus::Live,
            round_slot: 1,
            submitted_at_ms: 1,
            updated_at_ms: 1,
            venue_order_id: None,
            escalate_at_ms: None,
            maker_timeout_ms: 0,
            role: OrderRole::Pending,
        }
    }

    #[test]
    fn maker_crosses_on_book_touch() {
        let book = Book {
            bids: vec![(dec!(0.39), dec!(100))],
            asks: vec![(dec!(0.45), dec!(100))],
        };
        assert!(book.crosses(&order(Side::Buy, dec!(0.45), FillPolicy::Maker)));
        assert!(!book.crosses(&order(Side::Buy, dec!(0.44), FillPolicy::Maker)));
        assert!(book.crosses(&order(Side::Sell, dec!(0.39), FillPolicy::Maker)));
        assert!(!book.crosses(&order(Side::Sell, dec!(0.40), FillPolicy::Maker)));
    }

    #[test]
    fn walk_consumes_levels_best_first_on_both_sides() {
        // Multi-level books: the walk must start at the BEST level of each
        // side (lowest ask / highest bid). A bid-side walk that started at the
        // WORST bid would break early on a below-limit level and reject an
        // order a live FOK would happily fill.
        let book = Book {
            bids: vec![(dec!(0.30), dec!(100)), (dec!(0.95), dec!(8))],
            asks: vec![(dec!(0.70), dec!(100)), (dec!(0.35), dec!(4))],
        };
        // SELL walks bids from the top: 8 @ 0.95, then 2 @ 0.30.
        let (vwap, worst) = book
            .walk_marketable(Side::Sell, dec!(0.30), dec!(10))
            .expect("deep-enough bids fill the FOK");
        assert_eq!(
            vwap,
            (dec!(8) * dec!(0.95) + dec!(2) * dec!(0.30)) / dec!(10)
        );
        assert_eq!(worst, dec!(0.30));
        // The 0.95 level alone cannot cover 10 shares — the walk stays None
        // even though a live FOK at that limit would only have touched it.
        assert!(
            book.walk_marketable(Side::Sell, dec!(0.95), dec!(10))
                .is_none()
        );
        // BUY side walks asks from the bottom: 4 @ 0.35, then 6 @ 0.70.
        let (vwap3, worst3) = book
            .walk_marketable(Side::Buy, dec!(0.70), dec!(10))
            .expect("deep-enough asks fill the FOK");
        assert_eq!(
            vwap3,
            (dec!(4) * dec!(0.35) + dec!(6) * dec!(0.70)) / dec!(10)
        );
        assert_eq!(worst3, dec!(0.70));
    }

    #[test]
    fn crossing_depth_separates_no_crossing_from_not_enough_size() {
        // The two causes `walk_marketable` folds into one `None`, told apart for
        // the refusal message (#180): nothing crosses at all vs a crossing side
        // that is simply too thin for our size.
        let book = Book {
            bids: vec![(dec!(0.30), dec!(2)), (dec!(0.20), dec!(50))],
            asks: vec![(dec!(0.50), dec!(3))],
        };
        // BUY 0.40: no ask at or inside the limit — the quote moved.
        let (crosses, depth) = book.crossing_depth(Side::Buy, dec!(0.40));
        assert!(!crosses);
        assert_eq!(depth, dec!(0));
        assert!(
            book.walk_marketable(Side::Buy, dec!(0.40), dec!(1))
                .is_none()
        );
        // BUY 0.50: it crosses, but only 3 shares rest there — thin book.
        let (crosses, depth) = book.crossing_depth(Side::Buy, dec!(0.50));
        assert!(crosses);
        assert_eq!(depth, dec!(3));
        assert!(
            book.walk_marketable(Side::Buy, dec!(0.50), dec!(5))
                .is_none()
        );
        assert!(
            book.walk_marketable(Side::Buy, dec!(0.50), dec!(3))
                .is_some()
        );
        // SELL 0.25: the 0.30 bid crosses (2 shares) and the 0.20 does not —
        // depth counts only what a walk at that limit could actually take.
        let (crosses, depth) = book.crossing_depth(Side::Sell, dec!(0.25));
        assert!(crosses);
        assert_eq!(depth, dec!(2));
        // An empty book is "nothing crosses", not a panic.
        let empty = Book::default();
        assert_eq!(
            empty.crossing_depth(Side::Buy, dec!(0.50)),
            (false, dec!(0))
        );
    }

    #[test]
    fn mode_classification() {
        assert!(FillPolicy::Taker.immediate());
        assert!(!FillPolicy::Maker.immediate());
        assert_eq!(FillPolicy::Taker.venue_order_type(), OrderType::Fok);
        assert!(FillPolicy::Maker.post_only());
        assert_eq!(
            FillPolicy::MakerThenTaker.venue_order_type(),
            OrderType::Gtc
        );
        assert!(rests_on_book(FillPolicy::MakerThenTaker));
        assert!(!rests_on_book(FillPolicy::Taker));
    }

    #[test]
    fn fill_model_default_is_the_identity() {
        let m = FillModel::default();
        assert!(m.is_identity());
        // No slippage: the requested price is the filled price, both sides.
        assert_eq!(m.apply_slippage(Side::Buy, dec!(0.42)), dec!(0.42));
        assert_eq!(m.apply_slippage(Side::Sell, dec!(0.42)), dec!(0.42));
        // No latency: eligible immediately; always fills.
        assert_eq!(m.maker_eligible_at_ms(1_000), 1_000);
        assert!(m.maker_fill_wins("dry_1"));
    }

    #[test]
    fn slippage_worsens_both_sides_and_clamps_to_the_grid() {
        let m = FillModel {
            taker_slippage_ticks: 2,
            maker_latency_ms: 0,
            maker_fill_prob_bps: 10_000,
        };
        assert_eq!(m.apply_slippage(Side::Buy, dec!(0.42)), dec!(0.44));
        assert_eq!(m.apply_slippage(Side::Sell, dec!(0.42)), dec!(0.40));
        // Clamped: a 5-tick slip cannot push the price off the tradable grid.
        let wide = FillModel {
            taker_slippage_ticks: 5,
            ..Default::default()
        };
        assert_eq!(wide.apply_slippage(Side::Buy, dec!(0.99)), dec!(0.99));
        assert_eq!(wide.apply_slippage(Side::Sell, dec!(0.01)), dec!(0.01));
    }

    #[test]
    fn latency_delays_maker_eligibility() {
        let m = FillModel {
            maker_latency_ms: 250,
            ..Default::default()
        };
        assert_eq!(m.maker_eligible_at_ms(10_000), 10_250);
        assert!(!m.is_identity());
    }

    #[test]
    fn fill_probability_is_deterministic_and_bounded() {
        let never = FillModel {
            maker_fill_prob_bps: 0,
            ..Default::default()
        };
        assert!(!never.maker_fill_wins("dry_1"));
        assert!(!never.maker_fill_wins("dry_2"));

        let always = FillModel {
            maker_fill_prob_bps: 10_000,
            ..Default::default()
        };
        assert!(always.maker_fill_wins("dry_1"));

        let half = FillModel {
            maker_fill_prob_bps: 5_000,
            ..Default::default()
        };
        // Same order id → same verdict on every call (replays are reproducible).
        let first = half.maker_fill_wins("dry_42");
        for _ in 0..10 {
            assert_eq!(half.maker_fill_wins("dry_42"), first);
        }
        // And the draw is not degenerate: over many ids roughly half win.
        let wins = (0..400)
            .filter(|i| half.maker_fill_wins(&format!("dry_{i}")))
            .count();
        assert!(
            (120..=280).contains(&wins),
            "expected ~50% wins, got {wins}/400"
        );
    }
}
