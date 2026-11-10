//! 日内熔断闸门的 tick 级发布：`emit_daily_trip` 为本 tick 新检测到的已违约
//! 账户上 `DailyLoss` 入场闸门（到期自动清，无需手动解冻），并按账本口径
//! 发布一次性风控警报。重启后 `tripped` 不再持久化，故重启不自动重触发；
//! 解冻（`unfreeze_account`）清 latch，当日余下时间可交易。

use crate::model::*;
use blitzkrieg_market_api::net::now_ms;

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
        for (account, t) in self.positions.take_daily_trips() {
            self.set_account_gate(
                account.as_str(),
                format!(
                    "Daily loss limit for account {} — net ${} against a ${} cap ({}% of the day's opening equity ${})",
                    account,
                    t.realized_pnl_usd,
                    t.limit_usd,
                    self.config.positions.max_daily_loss_equity_pct,
                    t.opening_equity_usd,
                ),
                GateReason::DailyLoss,
                Some(expires_at_ms),
                "daily-loss breaker",
                now_ms,
                None,
            );
            let message = format!(
                "DAILY LOSS LIMIT REACHED for account {account}: realized ${} + unrealized ${} = net ${} against a ${} cap ({}% of the day's opening equity ${}) — new entries for this account are frozen until the next UTC day (positions still exit normally)",
                t.realized_pnl_usd,
                self.positions.unrealized_pnl_usd_for(account.as_str()),
                t.realized_pnl_usd + self.positions.unrealized_pnl_usd_for(account.as_str()),
                t.limit_usd,
                self.config.positions.max_daily_loss_equity_pct,
                t.opening_equity_usd
            );
            tracing::warn!(
                day = t.day_index,
                account = %account.as_str(),
                realized = %t.realized_pnl_usd,
                limit = %t.limit_usd,
                "{}",
                message
            );
            self.emit_risk_alert(CoreErrorCode::RiskRejected, message);
        }
    }
}
