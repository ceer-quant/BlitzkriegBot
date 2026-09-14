//! Dry-mode fill simulation, ported from the Node engine's dry semantics so a
//! DRY run exercises the exact same OME/ledger/risk code path as LIVE.
//!
//!  - Taker fills immediately and fully at its buffered limit price
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
}

pub fn rests_on_book(mode: FillPolicy) -> bool {
    matches!(mode, FillPolicy::Maker | FillPolicy::MakerThenTaker)
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
    fn mode_classification() {
        assert!(FillPolicy::Taker.immediate());
        assert!(!FillPolicy::Maker.immediate());
        assert_eq!(FillPolicy::Taker.venue_order_type(), OrderType::Fok);
        assert_eq!(FillPolicy::Maker.post_only(), true);
        assert_eq!(FillPolicy::MakerThenTaker.venue_order_type(), OrderType::Gtc);
        assert!(rests_on_book(FillPolicy::MakerThenTaker));
        assert!(!rests_on_book(FillPolicy::Taker));
    }
}
