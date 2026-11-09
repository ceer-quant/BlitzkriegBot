//! 日内熔断闸门的 tick 级发布：`emit_daily_trip` 为每个已违约账户上/刷新
//! `DailyLoss` 入场闸门（到期自动清，无需手动解冻），并按账本口径发布
//! 一次性风控警报。纯代码迁移，无逻辑改动。

use crate::model::*;
use blitzkrieg_market_api::net::now_ms;

use super::{Core, GateReason};

impl Core {
    /// Announce a tripped daily-loss breaker exactly once per process (see
    /// `DailyLossState::tripped_reported`). Consumed here AND at the losing
    /// close, so the freeze is visible at the moment it happens and again after
    /// a restart restored an already-breached budget.
    pub(super) fn emit_daily_trip(&mut self) {
        let now_ms = now_ms();
        // 下一个 UTC 日零点：日内熔断闸门在此自动到期（DailyLoss 到期自动清，
        // 无需操作员手动解冻）。
        let expires_at_ms = (crate::position::utc_day_index(now_ms) + 1) * crate::position::DAY_MS;
        // 先摘未报的 trip（同时把此处检测到的已违约账户标记 tripped），再对
        // 所有 tripped 账户上/刷新 DailyLoss 闸门。emit_daily_trip 每 tick 执行，
        // 所以「中途解冻也于下一 tick 被日内熔断重新拒绝」——解冻只放行入场，
        // 不擦除账本上的已实现亏损。
        let trips = self.positions.take_daily_trips();
        // 先把 tripped 快照出来（Clone，开销极小），再分别上闸门：
        // 避免 `daily_states()` 的不可变借贷与 `set_account_gate` 的可变借贷在循环体内重叠。
        let tripped: Vec<(String, crate::position::DailyLossState)> = self
            .positions
            .daily_states()
            .iter()
            .filter(|(_, s)| s.tripped)
            .map(|(id, s)| (id.to_string(), s.clone()))
            .collect();
        for (account, state) in tripped {
            self.set_account_gate(
                &account,
                format!(
                    "Daily loss limit for account {} — net ${} against a ${} cap ({}% of the day's opening equity ${})",
                    account,
                    state.realized_pnl_usd,
                    state.tripped_limit_usd,
                    self.config.positions.max_daily_loss_equity_pct,
                    state.opening_equity_usd,
                ),
                GateReason::DailyLoss,
                state.tripped_at_ms,
                Some(expires_at_ms),
                "daily-loss breaker",
            );
        }
        for (account, t) in trips {
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
