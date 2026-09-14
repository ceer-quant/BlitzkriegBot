//! Ledger abstraction — market-agnostic funds reservation.
//!
//! The core's concrete `Ledger` (in `ledger.rs`) implements this trait so other
//! market adapters (spot/futures/options) can supply their own ledger without
//! the order pipeline knowing the difference. Trait method names are distinct
//! from the concrete ledger's inherent methods to avoid dispatch ambiguity.

use crate::model::CoreResult;
use rust_decimal::Decimal;

/// An asset identifier (e.g. "USDC", "BTC").
pub type Asset = String;

pub trait LedgerApi {
    /// Reserve `amount` of the collateral asset for a pending order.
    fn reserve_for_order(&mut self, id: &str, amount: Decimal) -> CoreResult<()>;
    /// Release a reservation (cancel/reject/expiry); returns the freed amount.
    fn release_reservation(&mut self, id: &str) -> Decimal;
    /// Settle a reservation on fill: draw down the committed amount.
    fn settle_reservation(&mut self, id: &str, filled_notional: Decimal);
    /// Available (unreserved) balance for the collateral asset.
    fn available_for(&self, asset: &Asset) -> Decimal;
}
