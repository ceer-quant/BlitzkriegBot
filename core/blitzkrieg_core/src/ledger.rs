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

    /// Set a BUY order's outstanding commitment to exactly `remaining_notional`
    /// (= `limit_price * (size - filled_size)`, zero when nothing is left).
    ///
    /// Unlike [`Ledger::reserve`] this is a SET, not a gated add, and it is the
    /// single mechanism that keeps the reservation table equal to the order's
    /// real outstanding commitment (issue #181):
    ///  - after every applied fill (shrinks it by the filled notional, so a
    ///    price-improved fill leaves no residue behind);
    ///  - on the terminal transition (zero, driving the entry out of the table);
    ///  - at startup for orders re-adopted from the order log, where the local
    ///    table was just rebuilt from disk and must not start empty while real
    ///    orders rest at the venue.
    ///
    /// It never refuses: a refusal would silently UNDER-reserve and let the next
    /// entry over-commit, which is the failure this exists to prevent. The
    /// accounting audit reports the over-commit if the target exceeds what the
    /// local cash can cover. Returns the previous value.
    pub fn sync_buy_reservation(&mut self, order_id: &str, remaining_notional: Decimal) -> Decimal {
        let target = remaining_notional.max(Decimal::ZERO);
        let prev = self.reservations.remove(order_id).unwrap_or(Decimal::ZERO);
        if target > Decimal::ZERO {
            self.reservations.insert(order_id.to_string(), target);
        }
        prev
    }

    /// Settle a BUY fill EXACTLY: release the notional the filled shares had
    /// reserved and spend the cash they actually cost.
    ///
    /// The two are not the same number. A resting BUY reserved
    /// `limit_price * size` up front, so `filled` shares release
    /// `limit_price * filled` — while the cash that leaves is the EXECUTION price
    /// actually paid. Under the old `min(reserved, cost)` draw the difference
    /// (a price improvement of `(limit − execution) * filled`) stayed in the
    /// table forever once the order reached `Filled`: no cancel/reject path runs
    /// for a filled order, so nothing ever released it and `available()` bled
    /// down for the rest of the session (issue #181).
    ///
    /// The release is clamped to what is actually held, so a fill for an order
    /// with no local reservation (an order re-adopted from the venue, or a SELL
    /// rollback passing zero) is harmless. Returns the notional released.
    pub fn settle_buy_fill(
        &mut self,
        order_id: &str,
        cost_usd: Decimal,
        reserved_released: Decimal,
    ) -> Decimal {
        let held = self
            .reservations
            .get(order_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        let draw = reserved_released.max(Decimal::ZERO).min(held);
        if draw > Decimal::ZERO {
            let left = held - draw;
            if left > Decimal::ZERO {
                self.reservations.insert(order_id.to_string(), left);
            } else {
                self.reservations.remove(order_id);
            }
        } else if reserved_released > held {
            // Either the order was never reserved locally (a venue-only order)
            // or the reservation table drifted. Say so: the audit compares the
            // table against the OME's live BUY commitments.
            tracing::debug!(
                order = %order_id,
                requested = %reserved_released,
                held = %held,
                "buy settlement released more than was reserved"
            );
        }
        self.balance = (self.balance - cost_usd).max(Decimal::ZERO);
        draw
    }

    /// The cash identity `available + reserved == balance` (issue #181). It
    /// fails exactly when reserved exceeds balance, i.e. when local commitments
    /// no longer fit under the last venue-reported balance — which is also what
    /// [`Ledger::unfunded_reserved`] measures.
    pub fn is_balanced(&self) -> bool {
        self.available() + self.reserved() == self.balance
    }

    /// Reserved notional the local balance does not cover (zero when balanced).
    pub fn unfunded_reserved(&self) -> Decimal {
        (self.reserved() - self.balance).max(Decimal::ZERO)
    }

    /// Every live reservation, sorted by order id — the audit's view of what the
    /// ledger thinks is outstanding.
    pub fn reservations_snapshot(&self) -> Vec<(OrderId, Decimal)> {
        let mut v: Vec<(OrderId, Decimal)> = self
            .reservations
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
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

    /// Credit collateral redeemed on-chain (issue #175): the payout of a settled
    /// position moving out of the receivable and into spendable cash. Called
    /// exactly once per claim — the settlement book's durable journal is the
    /// idempotency guard, so a replayed redemption result cannot credit twice.
    pub fn credit_redemption(&mut self, payout_usd: Decimal) {
        if payout_usd <= Decimal::ZERO {
            return;
        }
        self.balance += payout_usd;
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
        // Market-agnostic lifecycle: the caller states one number, so the
        // reserved notional and the cash cost coincide (no price improvement
        // information reaches this trait).
        self.settle_buy_fill(id, filled_notional, filled_notional);
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
        l.settle_buy_fill("o1", dec!(4), dec!(4));
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

    // ── #181: reservation must reach zero on every path ──────────────────────

    /// Acceptance: a price-improved fill leaves NO residue. Resting BUY 10 @ 0.40
    /// reserves 4.00; the fill executes at 0.35, so 4.00 of reservation is
    /// released while only 3.50 of cash moves. The old `min(reserved, cost)` draw
    /// released just 3.50 and stranded 0.50 in the table — and since the order is
    /// now Filled, no cancel/reject ever released it.
    #[test]
    fn price_improved_fill_releases_the_full_reservation() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        l.reserve("o1", dec!(4)).unwrap();
        l.settle_buy_fill("o1", dec!(3.50), dec!(4));
        assert_eq!(l.reserved(), dec!(0), "no residue after a full fill");
        assert_eq!(l.balance(), dec!(6.50), "cash pays the execution price");
        assert!(l.is_balanced());
        assert_eq!(l.available(), dec!(6.50));
    }

    /// Acceptance: partial fill then cancel leaves nothing behind. Both the
    /// per-fill release (at the LIMIT price the notional was reserved at) and the
    /// cancel's `release` drive the entry to zero, and the identity holds
    /// throughout.
    #[test]
    fn partial_fill_then_cancel_leaves_no_residue() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        l.reserve("o1", dec!(4)).unwrap();
        assert!(l.is_balanced());
        // 4 of 10 shares fill at a better price.
        l.settle_buy_fill("o1", dec!(1.40), dec!(1.60));
        assert_eq!(l.reserved(), dec!(2.40), "remaining 6 shares stay reserved");
        assert!(l.is_balanced());
        // The kernel re-syncs to the true outstanding commitment...
        l.sync_buy_reservation("o1", dec!(2.40));
        assert_eq!(l.reserved(), dec!(2.40));
        // ...and the cancel retires the rest.
        l.release("o1");
        assert_eq!(l.reserved(), dec!(0));
        assert_eq!(l.balance(), dec!(8.60));
        assert!(l.is_balanced());
    }

    /// The identity is enforced by the reserve gate, not merely asserted: a
    /// commitment larger than the balance is refused rather than recorded.
    #[test]
    fn reserve_never_breaks_the_cash_identity() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        assert!(l.reserve("o1", dec!(11)).is_err());
        assert_eq!(l.reserved(), dec!(0));
        assert!(l.is_balanced());
        // A venue-reported balance drop below the outstanding commitments is the
        // one way the identity can break, and it is reported as such.
        l.reserve("o1", dec!(6)).unwrap();
        l.set_balance(dec!(5));
        assert!(!l.is_balanced());
        assert_eq!(l.unfunded_reserved(), dec!(1));
        assert_eq!(l.available(), Decimal::ZERO);
    }

    /// Sync is a set, not an add: it is idempotent, it can shrink a stale
    /// reservation, and zero removes the entry outright.
    #[test]
    fn sync_buy_reservation_sets_exactly() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        assert_eq!(l.sync_buy_reservation("o1", dec!(3)), Decimal::ZERO);
        assert_eq!(l.reserved(), dec!(3));
        assert_eq!(l.sync_buy_reservation("o1", dec!(3)), dec!(3));
        assert_eq!(l.reserved(), dec!(3), "idempotent");
        assert_eq!(l.sync_buy_reservation("o1", dec!(1)), dec!(3));
        assert_eq!(l.reserved(), dec!(1));
        assert_eq!(l.sync_buy_reservation("o1", Decimal::ZERO), dec!(1));
        assert_eq!(l.reserved(), Decimal::ZERO);
        assert!(l.reservations_snapshot().is_empty());
        // A negative target clamps to zero rather than crediting.
        l.sync_buy_reservation("o2", dec!(-5));
        assert_eq!(l.reserved(), Decimal::ZERO);
    }

    /// A fill for an order with no local reservation must not create one (the
    /// SELL rollback path passes zero, and a re-adopted venue order may have
    /// none): the release is clamped, never negative.
    #[test]
    fn settlement_never_releases_more_than_held() {
        let mut l = Ledger::new();
        l.set_balance(dec!(10));
        assert_eq!(l.settle_buy_fill("ghost", dec!(2), dec!(5)), Decimal::ZERO);
        assert_eq!(l.reserved(), Decimal::ZERO);
        assert_eq!(l.balance(), dec!(8));
        l.reserve("o1", dec!(4)).unwrap();
        assert_eq!(l.settle_buy_fill("o1", dec!(1), dec!(9)), dec!(4));
        assert_eq!(l.reserved(), Decimal::ZERO);
        assert!(l.is_balanced());
    }
}
