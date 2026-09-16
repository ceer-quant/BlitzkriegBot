//! Inventory ledger — local USDC reservation/release so the core never places
//! orders whose notional exceeds available collateral. Balances are periodically
//! reconciled against the venue (set_balance); P0 enforces the local gate, the
//! on-chain/REST reconciliation loop lands with P1.

use crate::model::{CoreError, CoreErrorCode, CoreResult, OrderId};
use rust_decimal::Decimal;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Ledger {
    /// Last venue-reported cash balance (gross, before local reservations).
    balance: Decimal,
    /// Notional reserved per resting BUY order.
    reservations: HashMap<OrderId, Decimal>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed/refresh the venue balance during reconciliation.
    pub fn set_balance(&mut self, balance: Decimal) {
        self.balance = balance.max(Decimal::ZERO);
    }

    pub fn balance(&self) -> Decimal {
        self.balance
    }

    pub fn reserved(&self) -> Decimal {
        self.reservations
            .values()
            .copied()
            .fold(Decimal::ZERO, |a, b| a + b)
    }

    /// Collateral free to commit to new BUY orders.
    pub fn available(&self) -> Decimal {
        (self.balance - self.reserved()).max(Decimal::ZERO)
    }

    /// Reserve notional for a resting BUY (price * size). Idempotent per order.
    pub fn reserve(&mut self, order_id: &str, notional: Decimal) -> CoreResult<()> {
        if notional < Decimal::ZERO {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "negative notional",
            ));
        }
        if self.reservations.contains_key(order_id) {
            return Ok(());
        }
        if notional > self.available() {
            return Err(CoreError::new(
                CoreErrorCode::InsufficientFunds,
                format!(
                    "reserve {notional} exceeds available {} (balance {}, reserved {})",
                    self.available(),
                    self.balance,
                    self.reserved()
                ),
            ));
        }
        self.reservations.insert(order_id.to_string(), notional);
        Ok(())
    }

    /// Release a BUY reservation on cancel/reject/expiry.
    pub fn release(&mut self, order_id: &str) -> Decimal {
        self.reservations.remove(order_id).unwrap_or(Decimal::ZERO)
    }

    /// Settle a BUY fill: spend `cost` cash and draw down its reservation by up
    /// to the same amount (a resting buy reserved price*size; a fill realises it).
    pub fn settle_buy_fill(&mut self, order_id: &str, cost: Decimal) {
        let reserved = self
            .reservations
            .get(order_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        let draw = reserved.min(cost);
        if draw > Decimal::ZERO {
            let left = reserved - draw;
            if left > Decimal::ZERO {
                self.reservations.insert(order_id.to_string(), left);
            } else {
                self.reservations.remove(order_id);
            }
        }
        self.balance = (self.balance - cost).max(Decimal::ZERO);
    }

    /// Settle a SELL fill: add proceeds, minus the venue trading fee charged
    /// on the way out. Without the fee the cash ledger credits the full
    /// notional and the cash view drifts above the realized-net view the trade
    /// records use.
    pub fn settle_sell_fill(&mut self, proceeds: Decimal, fee_usd: Decimal) {
        self.balance = (self.balance + proceeds - fee_usd).max(Decimal::ZERO);
    }

    /// Charge an entry-side cash fee (buy fills pay it on top of the cost).
    pub fn charge_fee(&mut self, fee_usd: Decimal) {
        self.balance = (self.balance - fee_usd).max(Decimal::ZERO);
    }
}

/// Market-agnostic reservation lifecycle (see `ledger_api`). The core pipeline
/// can talk to this trait; prediction markets use the whole balance as the
/// single collateral asset.
impl crate::ledger_api::LedgerApi for Ledger {
    fn reserve_for_order(&mut self, id: &str, amount: Decimal) -> CoreResult<()> {
        self.reserve(id, amount)
    }
    fn release_reservation(&mut self, id: &str) -> Decimal {
        self.release(id)
    }
    fn settle_reservation(&mut self, id: &str, filled_notional: Decimal) {
        self.settle_buy_fill(id, filled_notional)
    }
    fn available_for(&self, _asset: &crate::ledger_api::Asset) -> Decimal {
        // Single-collateral (USDC) model: the asset argument is informational.
        self.available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn reservation_caps_overspend() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        l.reserve("o1", dec!(6)).unwrap();
        assert_eq!(l.available(), dec!(4));
        let err = l.reserve("o2", dec!(5)).unwrap_err();
        assert_eq!(err.code, CoreErrorCode::InsufficientFunds);
        // Cancel frees it, then a smaller order fits.
        assert_eq!(l.release("o1"), dec!(6));
        l.reserve("o2", dec!(5)).unwrap();
        assert_eq!(l.available(), dec!(5));
    }

    #[test]
    fn buy_fill_draws_reservation_and_cash() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        l.reserve("o1", dec!(4)).unwrap();
        l.settle_buy_fill("o1", dec!(4));
        assert_eq!(l.balance(), dec!(6));
        assert_eq!(l.reserved(), dec!(0));
        l.settle_sell_fill(dec!(7), Decimal::ZERO);
        assert_eq!(l.balance(), dec!(13));
    }

    #[test]
    fn fees_leave_the_cash_ledger() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        // Sell 10 notional with 1 fee: 10 + 10 − 1 = 19 cash.
        l.settle_sell_fill(dec!(10), dec!(1));
        assert_eq!(l.balance(), dec!(19));
        // Entry fee also comes out of cash.
        l.charge_fee(dec!(2));
        assert_eq!(l.balance(), dec!(17));
    }
}
