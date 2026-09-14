//! Order semantics — market-agnostic order intent.
//!
//! The `OrderIntent` contract and its enums now live in `blitzkrieg-market-api`
//! (single source of truth shared with market plugins) and are re-exported here
//! so existing `crate::order::…` paths keep working unchanged.

pub use blitzkrieg_market_api::{MarketType, OrderIntent, OrderKind, TimeInForce};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Side;
    use rust_decimal_macros::dec;

    fn intent(kind: OrderKind, price: Option<rust_decimal::Decimal>) -> OrderIntent {
        OrderIntent {
            market: MarketType::Prediction,
            symbol: "tok".into(),
            side: Side::Buy,
            order_kind: kind,
            price,
            size: dec!(10),
            time_in_force: TimeInForce::Gtc,
            metadata: Default::default(),
        }
    }

    #[test]
    fn validates_limit_and_market() {
        assert!(intent(OrderKind::Limit, Some(dec!(0.4))).validate().is_ok());
        assert!(intent(OrderKind::Limit, None).validate().is_err());
        // Market orders carry no price and still validate.
        assert!(intent(OrderKind::Market, None).validate().is_ok());
        // Prediction prices must be within (0,1].
        assert!(intent(OrderKind::Limit, Some(dec!(1.2))).validate().is_err());
    }
}
