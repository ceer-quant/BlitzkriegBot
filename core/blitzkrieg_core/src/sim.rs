//! Dry-mode fill simulation, ported from the Node engine's dry semantics so a
//! DRY run exercises the exact same OME/ledger/risk code path as LIVE.
//!
//!  - Taker fills like a live FOK: it walks the opposing side's resting levels
//!    (best first, no worse than its limit) and fills at the volume-weighted
//!    price; if that side cannot cover the full size, the order is rejected —
//!    all-or-nothing, the same way the venue would kill it
//!  - Maker rests post-only and fills only when the live book crosses the limit
//!    (BUY when best ask <= limit, SELL when best bid >= limit), and then only
//!    up to the size the crossing side actually offers at prices that cross
//!    that limit — a maker cannot trade more than reaches it (issue #183)
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

    /// The resting levels a `side` order priced at `limit` would trade
    /// against, best price first (lowest ask for a BUY, highest bid for a
    /// SELL).
    ///
    /// This is the ONE depth reading of the dry matcher: the taker walk
    /// ([`Book::walk_marketable`]) and the maker partial-fill cap
    /// ([`Book::marketable_depth`]) both start here, so "how much can trade"
    /// cannot drift into two different answers (issue #183).
    fn crossing_levels(&self, side: Side, limit: Decimal) -> Vec<(Decimal, Decimal)> {
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
        // Best-first order makes the crossing levels a PREFIX of the walk, so
        // retaining them is exactly the `break` on the first non-crossing
        // price the walk used to take.
        levels.retain(|(p, _)| match side {
            Side::Buy => *p <= limit,
            Side::Sell => *p >= limit,
        });
        levels
    }

    /// Total resting size on the side a `side` order priced at `limit` would
    /// trade against — the volume that is actually there to be taken.
    ///
    /// A resting maker's fill is capped by this, not by its own `size`: the
    /// book can only trade what reaches the order (issue #183).
    pub fn marketable_depth(&self, side: Side, limit: Decimal) -> Decimal {
        self.crossing_levels(side, limit)
            .iter()
            .map(|(_, s)| *s)
            .sum()
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
        let levels = self.crossing_levels(side, limit);
        let mut remaining = size;
        let mut notional = Decimal::ZERO;
        for (price, level_size) in levels.iter() {
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
            .filter(|(_, s)| *s > Decimal::ZERO)
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

/// Default for [`FillModel::maker_depth_share_bps`]: take the whole crossing
/// depth. Named so the serde default (used when an older config omits the
/// field) is the same identity value the in-code default carries — a plain
/// `0` would deserialize into "never take anything".
fn default_maker_depth_share_bps() -> u32 {
    10_000
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
///  - `maker_depth_share_bps`: of the volume the crossing side actually offers
///    ([`Book::marketable_depth`]), how much this maker order gets — its place
///    in the queue behind everyone already resting. `10_000` (default) takes
///    all of it; lower values model being late to the front. The draw is a
///    salted hash of the order id, independent of the `maker_fill_prob_bps`
///    draw, so a replay is still reproducible and the two dials do not move
///    together.
///
/// The depth cap itself is NOT a dial: a maker fill is always bounded by the
/// crossing depth (issue #183), because that is venue physics rather than
/// friction we choose to price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FillModel {
    pub taker_slippage_ticks: u32,
    pub maker_latency_ms: i64,
    pub maker_fill_prob_bps: u32,
    #[serde(default = "default_maker_depth_share_bps")]
    pub maker_depth_share_bps: u32,
}

impl Default for FillModel {
    fn default() -> Self {
        Self {
            taker_slippage_ticks: 0,
            maker_latency_ms: 0,
            maker_fill_prob_bps: 10_000,
            maker_depth_share_bps: default_maker_depth_share_bps(),
        }
    }
}

impl FillModel {
    /// True when no friction dial is engaged (the default).
    ///
    /// Note this is about the DIALS only: the maker depth cap applies whatever
    /// the model says, because it is how the venue works, not a choice.
    pub fn is_identity(&self) -> bool {
        self.taker_slippage_ticks == 0
            && self.maker_latency_ms <= 0
            && self.maker_fill_prob_bps >= 10_000
            && self.maker_depth_share_bps >= 10_000
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

    /// How much of a crossing maker order actually trades this time.
    ///
    /// `remaining` is what is left of the order; `depth` is
    /// [`Book::marketable_depth`] on the side this order would consume. The cap
    /// is `depth`, never `remaining` alone — a maker fills only against volume
    /// that reaches its limit, so an order larger than the crossing depth is
    /// left partially filled rather than magically whole (issue #183).
    ///
    /// The result is deterministic for a given `(order_id, remaining, depth)`:
    /// the queue share is a salted hash of the order id, so the same replay
    /// always produces the same partial fills.
    pub fn maker_fill_size(&self, order_id: &str, remaining: Decimal, depth: Decimal) -> Decimal {
        if remaining <= Decimal::ZERO || depth <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let reachable = remaining.min(depth);
        if self.maker_depth_share_bps >= 10_000 {
            return reachable;
        }
        if self.maker_depth_share_bps == 0 {
            return Decimal::ZERO;
        }
        // A share in 1..=share_bps: a knob set above zero must never silently
        // mean "never fill" just because the hash landed on zero.
        let bps = 1 + (draw_salted(order_id, DEPTH_SHARE_SALT) % self.maker_depth_share_bps);
        (reachable * Decimal::from(bps) / Decimal::from(10_000u32))
            .min(reachable)
            .max(Decimal::ZERO)
    }
}

/// Salt for the queue-share draw, so it is statistically independent of the
/// fill-probability draw on the same order id.
const DEPTH_SHARE_SALT: u8 = 0x5d;

/// FNV-1a over the order id → a stable pseudo-random draw.
fn draw(order_id: &str) -> u32 {
    draw_salted(order_id, 0)
}

/// FNV-1a over the order id, mixed with `salt` first: one stable draw per
/// (id, salt) pair, so two dials on the same id do not move together.
fn draw_salted(order_id: &str, salt: u8) -> u32 {
    let mut h: u32 = 0x811c_9dc5 ^ (salt as u32).wrapping_mul(0x9e37_79b9);
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
    fn marketable_depth_counts_only_the_crossing_levels() {
        let book = Book {
            bids: vec![(dec!(0.30), dec!(100)), (dec!(0.39), dec!(7))],
            asks: vec![
                (dec!(0.45), dec!(100)),
                (dec!(0.40), dec!(3)),
                (dec!(0.41), dec!(2)),
            ],
        };
        // BUY at 0.41 reaches the 0.40 and 0.41 asks; the 0.45 ask is out of
        // reach and must not inflate the depth.
        assert_eq!(book.marketable_depth(Side::Buy, dec!(0.41)), dec!(5));
        assert_eq!(book.marketable_depth(Side::Buy, dec!(0.45)), dec!(105));
        // SELL at 0.39 reaches only the 0.39 bid.
        assert_eq!(book.marketable_depth(Side::Sell, dec!(0.39)), dec!(7));
        assert_eq!(book.marketable_depth(Side::Sell, dec!(0.30)), dec!(107));
        // Nothing crosses at all.
        assert_eq!(book.marketable_depth(Side::Buy, dec!(0.39)), Decimal::ZERO);
        assert_eq!(book.marketable_depth(Side::Sell, dec!(0.45)), Decimal::ZERO);
    }

    /// Issue #183: the maker cap and the taker walk must read the SAME depth.
    /// Two different answers to "how much can trade" was the defect, so this
    /// pins them to each other rather than to a hard-coded number.
    #[test]
    fn maker_depth_and_taker_walk_agree_on_what_can_trade() {
        let book = Book {
            bids: vec![(dec!(0.55), dec!(4)), (dec!(0.50), dec!(6))],
            asks: vec![(dec!(0.60), dec!(3)), (dec!(0.65), dec!(7))],
        };
        for (side, limit) in [
            (Side::Buy, dec!(0.65)),
            (Side::Buy, dec!(0.60)),
            (Side::Sell, dec!(0.55)),
            (Side::Sell, dec!(0.50)),
        ] {
            let depth = book.marketable_depth(side, limit);
            assert!(depth > Decimal::ZERO, "{side:?} @ {limit}: nothing crosses");
            // Exactly the depth is walkable; one share more is what a live FOK
            // gets killed for.
            assert!(
                book.walk_marketable(side, limit, depth).is_some(),
                "{side:?} @ {limit}: the measured depth {depth} must be walkable"
            );
            assert!(
                book.walk_marketable(side, limit, depth + dec!(0.01))
                    .is_none(),
                "{side:?} @ {limit}: more than the depth {depth} must be unfillable"
            );
        }
    }

    #[test]
    fn maker_fill_is_capped_by_the_crossing_depth_not_the_order_size() {
        let m = FillModel::default();
        // A 10-share maker meets 4 shares of crossing depth: 4 trade, 6 rest.
        assert_eq!(m.maker_fill_size("dry_1", dec!(10), dec!(4)), dec!(4));
        // Depth past the order size changes nothing: the order's own remaining
        // size is still the tighter bound (this is the unchanged behaviour every
        // deep-book case already relied on).
        assert_eq!(m.maker_fill_size("dry_1", dec!(10), dec!(100)), dec!(10));
        // Nothing to take, or nothing left to fill.
        assert_eq!(
            m.maker_fill_size("dry_1", dec!(10), Decimal::ZERO),
            Decimal::ZERO
        );
        assert_eq!(
            m.maker_fill_size("dry_1", Decimal::ZERO, dec!(10)),
            Decimal::ZERO
        );
    }

    #[test]
    fn depth_share_is_deterministic_and_bounded() {
        let half = FillModel {
            maker_depth_share_bps: 5_000,
            ..FillModel::default()
        };
        assert!(!half.is_identity());
        let first = half.maker_fill_size("dry_42", dec!(10), dec!(10));
        assert!(
            first > Decimal::ZERO && first <= dec!(5),
            "a 50% queue share must leave some of the depth and take some: {first}"
        );
        // Same order id → the same partial fill, every time (replays are exact).
        for _ in 0..10 {
            assert_eq!(half.maker_fill_size("dry_42", dec!(10), dec!(10)), first);
        }
        // And the draw is not degenerate: the share spreads across ids rather
        // than pinning every order to the same fraction.
        let shares: Vec<Decimal> = (0..200)
            .map(|i| half.maker_fill_size(&format!("dry_{i}"), dec!(10), dec!(10)))
            .collect();
        assert!(
            shares.iter().all(|s| *s > Decimal::ZERO && *s <= dec!(5)),
            "every share stays inside (0, 50%]"
        );
        let below_max = shares.iter().filter(|s| **s < dec!(5)).count();
        assert!(
            below_max > 150,
            "the share must vary with the id, not sit at its ceiling ({below_max}/200 below max)"
        );

        // A zero share never fills; the ceiling share takes everything reachable
        // and is the identity.
        let none = FillModel {
            maker_depth_share_bps: 0,
            ..FillModel::default()
        };
        assert_eq!(
            none.maker_fill_size("dry_1", dec!(10), dec!(10)),
            Decimal::ZERO
        );
        assert!(!none.is_identity());
        let all = FillModel::default();
        assert_eq!(all.maker_fill_size("dry_1", dec!(10), dec!(10)), dec!(10));
        assert!(all.is_identity());
    }

    /// Replay reproducibility is a hard requirement (#183): the partial size for
    /// an order id is a fixed number. The value is PINNED so that changing the
    /// hash is a deliberate, reviewable act — a test that only checked
    /// "stable within this process" would not catch a silent replay break.
    #[test]
    fn depth_share_hash_is_pinned() {
        let m = FillModel {
            maker_depth_share_bps: 5_000,
            ..FillModel::default()
        };
        let got = m.maker_fill_size("dry_order_7", dec!(100), dec!(100));
        assert_eq!(
            got,
            dec!(3.13),
            "the queue-share draw changed: every archived dry replay would now fill differently"
        );
        // The share never depends on the probability dial: the two use
        // independent salts over the same id, so a backtest can vary one without
        // silently moving the other.
        let other_prob = FillModel {
            maker_fill_prob_bps: 1,
            ..m
        };
        assert_eq!(
            other_prob.maker_fill_size("dry_order_7", dec!(100), dec!(100)),
            got
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
            maker_depth_share_bps: 10_000,
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
