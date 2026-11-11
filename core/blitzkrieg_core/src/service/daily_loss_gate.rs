//! 日内熔断闸门的 tick 级发布：`emit_daily_trip` 为本 tick 新检测到的已违约
//! 账户上 `DailyLoss` 入场闸门（到期自动清，无需手动解冻），并按账本口径
//! 发布一次性风控警报。重启后 `tripped` 不再持久化，故重启不自动重触发；
//! 解冻（`unfreeze_account`）清 latch，当日余下时间可交易。

use crate::model::*;
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;

use super::{Core, GateReason};

impl Core {
    /// Announce a tripped daily-loss breaker exactly once per process (see
    /// `DailyLossState::tripped_reported`). Consumed here AND at the losing
    /// close, so the freeze is visible at the moment it happens.
    ///
    /// Only trips newly latched THIS tick (`take_daily_trips`) get a gate;
    /// a restart starts with `tripped = false` (not persisted), so a restart
    /// never re-arms the daily-loss gate from a stale latch — cash truth wins
    /// over a remembered breaker. `unfreeze_account` clears the latch, so a
    /// mid-day unfreeze re-opens entries for the rest of the day.
    pub(super) fn emit_daily_trip(&mut self) {
        let now_ms = now_ms();
        // 下一个 UTC 日零点：日内熔断闸门在此自动到期（DailyLoss 到期自动清，
        // 无需操作员手动解冻）。
        let expires_at_ms = (crate::position::utc_day_index(now_ms) + 1) * crate::position::DAY_MS;
        let unrealized_enabled = self.config.positions.daily_loss_marks_unrealized;
        for (account, t) in self.positions.take_daily_trips() {
            let realized = t.realized_pnl_usd;
            let unrealized = self.positions.unrealized_pnl_usd_for(account.as_str());
            // Trigger口径：unrealized marks がオンなら realized + unrealized、オフなら realized のみ。
            let drawdown = if unrealized_enabled {
                realized + unrealized
            } else {
                realized
            };
            // Effective cap：実際に拘束する規則のみを報告（絶対額と equity 比を混在させない）。
            let abs = self.config.positions.max_daily_loss_usd;
            let pct = self.config.positions.max_daily_loss_equity_pct;
            let opening = t.opening_equity_usd;
            let rel = if pct > Decimal::ZERO && opening > Decimal::ZERO {
                opening * pct / Decimal::from(100)
            } else {
                Decimal::ZERO
            };
            let (cap, rule_desc) = match (abs > Decimal::ZERO, rel > Decimal::ZERO) {
                (true, true) => {
                    if abs <= rel {
                        (abs, "absolute".to_string())
                    } else {
                        (rel, format!("{}% of opening equity ${:.2}", pct, opening))
                    }
                }
                (true, false) => (abs, "absolute".to_string()),
                (false, true) => (rel, format!("{}% of opening equity ${:.2}", pct, opening)),
                (false, false) => (Decimal::ZERO, "absolute".to_string()),
            };

            // Gate message — the freeze reason shown to the operator.
            self.set_account_gate(
                account.as_str(),
                format!(
                    "Daily loss limit for account {} — drawdown ${:.2} vs cap ${:.2} ({})",
                    account, drawdown, cap, rule_desc
                ),
                GateReason::DailyLoss,
                Some(expires_at_ms),
                "daily-loss breaker",
                now_ms,
                None,
            );

            // Kernel log + risk alert — full trigger口径, English only.
            let detail = if unrealized_enabled {
                format!(
                    "(realized ${:.2} + unrealized ${:.2})",
                    realized, unrealized
                )
            } else {
                format!("(realized ${:.2}, realized only)", realized)
            };
            let message = format!(
                "DAILY LOSS LIMIT REACHED for account {}: drawdown ${:.2} {} vs cap ${:.2} ({}) — new entries for this account are frozen until the next UTC day (positions still exit normally)",
                account.as_str(),
                drawdown,
                detail,
                cap,
                rule_desc
            );
            tracing::warn!(
                day = t.day_index,
                account = %account.as_str(),
                realized = %realized,
                limit = %cap,
                "{}",
                message
            );
            self.emit_risk_alert(CoreErrorCode::RiskRejected, message);
        }
    }
}
