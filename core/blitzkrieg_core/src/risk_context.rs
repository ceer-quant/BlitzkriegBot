//! Risk abstraction — market-agnostic risk context.
//!
//! The risk engine (`risk.rs`) is market-agnostic; venue/asset-specific limits
//! are supplied through a `RiskContext`. Prediction markets have no leverage or
//! liquidation; futures/options do, so those fields are `Option`.

use rust_decimal::Decimal;

/// The limits and account facts the risk engine needs, independent of market.
pub trait RiskContext: Send + Sync {
    /// Maximum position size in base units.
    fn max_position_size(&self) -> Decimal;
    /// Maximum daily loss in quote currency.
    fn max_daily_loss(&self) -> Decimal;
    /// Maximum leverage, if the market supports leverage.
    fn max_leverage(&self) -> Option<Decimal>;
    /// Liquidation price for the current position, if applicable.
    fn liquidation_price(&self) -> Option<Decimal>;
}

/// Default context for binary prediction markets (no leverage/liquidation).
#[derive(Debug, Clone)]
pub struct PredictionRiskContext {
    pub max_position_size: Decimal,
    pub max_daily_loss: Decimal,
}

impl Default for PredictionRiskContext {
    fn default() -> Self {
        Self {
            max_position_size: Decimal::from(100),
            max_daily_loss: Decimal::from(200),
        }
    }
}

impl RiskContext for PredictionRiskContext {
    fn max_position_size(&self) -> Decimal {
        self.max_position_size
    }
    fn max_daily_loss(&self) -> Decimal {
        self.max_daily_loss
    }
    fn max_leverage(&self) -> Option<Decimal> {
        None
    }
    fn liquidation_price(&self) -> Option<Decimal> {
        None
    }
}

/// Context for leveraged futures markets.
#[derive(Debug, Clone)]
pub struct FuturesRiskContext {
    pub max_position_size: Decimal,
    pub max_daily_loss: Decimal,
    pub max_leverage: Decimal,
    pub liquidation_price: Option<Decimal>,
}

impl RiskContext for FuturesRiskContext {
    fn max_position_size(&self) -> Decimal {
        self.max_position_size
    }
    fn max_daily_loss(&self) -> Decimal {
        self.max_daily_loss
    }
    fn max_leverage(&self) -> Option<Decimal> {
        Some(self.max_leverage)
    }
    fn liquidation_price(&self) -> Option<Decimal> {
        self.liquidation_price
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn prediction_context_has_no_leverage() {
        let c = PredictionRiskContext::default();
        assert!(c.max_leverage().is_none());
        assert_eq!(c.max_position_size(), dec!(100));
    }

    #[test]
    fn futures_context_exposes_leverage() {
        let c = FuturesRiskContext {
            max_position_size: dec!(1),
            max_daily_loss: dec!(500),
            max_leverage: dec!(10),
            liquidation_price: Some(dec!(50000)),
        };
        assert_eq!(c.max_leverage(), Some(dec!(10)));
        assert_eq!(c.liquidation_price(), Some(dec!(50000)));
    }
}
