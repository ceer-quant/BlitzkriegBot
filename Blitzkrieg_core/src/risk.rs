//! Minimal P0 risk gate that every order must pass inside the core.
//!
//! Full daily-loss / consecutive-loss / cooldown risk lands in P1/P2 with
//! positions. P0 enforces the invariants no order must ever bypass:
//!  - global kill switch
//!  - per-order notional cap
//!  - (the ledger independently prevents overspending)

use crate::model::{CoreError, CoreErrorCode, CoreResult, OrderRequest, Side};
use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub max_order_notional: Decimal,
    pub min_price: Decimal,
    pub max_price: Decimal,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            // Mirrors the Node HFT defaults ($2.5/order, prices within the band).
            max_order_notional: Decimal::from(100),
            min_price: Decimal::ZERO,
            max_price: Decimal::ONE,
        }
    }
}

#[derive(Debug, Default)]
pub struct RiskGate {
    config: RiskConfig,
    killed: bool,
    kill_reason: Option<String>,
}

/// Consecutive-loss circuit breaker (mirrors the Node engine's MAX_CONSECUTIVE_LOSSES).
#[derive(Debug)]
pub struct LossBreaker {
    max_consecutive_losses: u32,
    cooldown_sec: i64,
    consecutive_losses: u32,
    halted_until_ms: i64,
}

impl LossBreaker {
    pub fn new(max_consecutive_losses: u32, cooldown_sec: i64) -> Self {
        Self { max_consecutive_losses, cooldown_sec, consecutive_losses: 0, halted_until_ms: 0 }
    }

    pub fn record(&mut self, net_pnl: Decimal, now_ms: i64) -> bool {
        if net_pnl < Decimal::ZERO {
            self.consecutive_losses += 1;
            if self.consecutive_losses >= self.max_consecutive_losses {
                self.halted_until_ms = now_ms + self.cooldown_sec * 1000;
                return true; // tripped
            }
        } else if net_pnl > Decimal::ZERO {
            self.consecutive_losses = 0;
        }
        false
    }

    pub fn is_halted(&self, now_ms: i64) -> bool {
        now_ms < self.halted_until_ms
    }

    /// Clear the halt if its cooldown elapsed; returns true if it just resumed.
    pub fn maybe_resume(&mut self, now_ms: i64) -> bool {
        if self.halted_until_ms > 0 && now_ms >= self.halted_until_ms {
            self.halted_until_ms = 0;
            self.consecutive_losses = 0;
            return true;
        }
        false
    }

    pub fn consecutive_losses(&self) -> u32 {
        self.consecutive_losses
    }
    pub fn halted_until_ms(&self) -> i64 {
        self.halted_until_ms
    }
}

impl RiskGate {
    pub fn new(config: RiskConfig) -> Self {
        Self { config, killed: false, kill_reason: None }
    }

    pub fn kill(&mut self, reason: impl Into<String>) {
        self.killed = true;
        self.kill_reason = Some(reason.into());
    }
    pub fn resume(&mut self) {
        self.killed = false;
        self.kill_reason = None;
    }
    pub fn is_killed(&self) -> bool {
        self.killed
    }
    pub fn set_config(&mut self, c: RiskConfig) {
        self.config = c;
    }

    pub fn check(&self, req: &OrderRequest) -> CoreResult<()> {
        if self.killed {
            return Err(CoreError::new(
                CoreErrorCode::KillSwitchActive,
                self.kill_reason.clone().unwrap_or_else(|| "kill switch active".into()),
            ));
        }
        if req.price <= self.config.min_price || req.price > self.config.max_price {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                format!("price {} outside ({}..{}]", req.price, self.config.min_price, self.config.max_price),
            ));
        }
        if req.size <= Decimal::ZERO {
            return Err(CoreError::new(CoreErrorCode::RiskRejected, "size must be positive"));
        }
        // Notional cap applies to BUY commitment; SELL is bounded by position (P2).
        if req.side == Side::Buy {
            let notional = req.price * req.size;
            if notional > self.config.max_order_notional {
                return Err(CoreError::new(
                    CoreErrorCode::RiskRejected,
                    format!("notional {notional} exceeds per-order cap {}", self.config.max_order_notional),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FillPolicy, Side};
    use rust_decimal_macros::dec;

    fn req(side: Side, price: Decimal, size: Decimal) -> OrderRequest {
        OrderRequest {
            token_id: "t".into(),
            condition_id: "c".into(),
            side,
            mode: FillPolicy::Taker,
            price,
            size,
            internal_key: "k".into(),
            strategy: "s".into(),
            asset: "BTC".into(),
            direction: "up".into(),
            round_slot: 1,
        }
    }

    #[test]
    fn gates_notional_and_kill() {
        let mut g = RiskGate::new(RiskConfig {
            max_order_notional: dec!(3),
            ..Default::default()
        });
        // 0.5 * 4 = 2.0 within cap; 0.5 * 8 = 4.0 rejected.
        g.check(&req(Side::Buy, dec!(0.5), dec!(4))).unwrap();
        g.check(&req(Side::Buy, dec!(0.5), dec!(8))).unwrap_err();
        g.kill("manual");
        assert_eq!(
            g.check(&req(Side::Buy, dec!(0.5), dec!(1))).unwrap_err().code,
            CoreErrorCode::KillSwitchActive
        );
        g.resume();
        g.check(&req(Side::Buy, dec!(0.5), dec!(1))).unwrap();
    }

    #[test]
    fn breaker_trips_on_consecutive_losses_and_resumes() {
        let mut b = LossBreaker::new(3, 300);
        assert!(!b.record(dec!(-1), 0));
        assert!(!b.record(dec!(-1), 1));
        assert!(b.record(dec!(-1), 2)); // 3rd loss trips
        assert!(b.is_halted(1000));
        // A win resets the streak but the halt persists until cooldown.
        assert!(!b.record(dec!(5), 2000));
        assert!(b.is_halted(2000));
        assert!(b.maybe_resume(300_100));
        assert!(!b.is_halted(300_100));
        assert_eq!(b.consecutive_losses(), 0);
    }
}
