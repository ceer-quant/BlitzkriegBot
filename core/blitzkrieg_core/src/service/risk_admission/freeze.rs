use super::*;
use crate::account::AccountStatus;
use serde::Serialize;

/// 账户入场闸门（#1045）：每账户一份，**纯内存、永不落盘**，进程重启即清，
/// 唯一由 `account.freeze` / `account.unfreeze`、日内亏损熔断和场地自愈三条
/// 路由改写。所有入场路径只读 `gate.blocked`；出场永不阻断。
///
/// 取代旧 `FreezeBook` 里的 `account_freezes` + `auto_frozen` 两张持久表：
/// 不再有任何冻结姿态随进程重启而重播。`realized_pnl_usd` 继续持久化（资金
/// 真相），重启只清闸门；重启后若账本里的已实现亏损仍超当日上限，会在
/// 下一次 `emit_daily_trip` 时按账本重新上 `DailyLoss` 闸门--这是「按账本
/// 推导」，不是「回放冻结」。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountGate {
    pub(crate) blocked: bool,
    /// 运营可读的原因文案（算法给定：如 "daily loss limit reached" / 手动冻结
    /// 的 operator 原因 / 场地拒单原因）。
    pub(crate) reason: String,
    /// 原因种类，决定优先级与谁可以自解。
    pub(crate) reason_kind: GateReason,
    /// 闸门首次上锁的时点（ms），供 UI 倒计时与审计。
    pub(crate) since_ms: i64,
    /// 自动到期点（ms）：`DailyLoss` 是下一个 UTC 日零点；`None` = 仅由解冻
    /// 操作或配置改写清除（`Manual`、`ConfigReadOnly`、`ConfigSuspended`）。
    pub(crate) expires_at_ms: Option<i64>,
    pub(crate) actor: String,
}

/// 闸门原因，声明顺序即优先级从低到高：
/// `VenueRejection < DailyLoss < ConfigReadOnly < ConfigSuspended <
/// TradingDisabled < Manual < KillSwitch`。
///
/// 更高优先级的闸门可覆盖更低的；较低优先级的路由永远不会静默解除更高
/// 优先级的闸门（场地健康探针只解 `VenueRejection`，碰不着 `Manual`/`DailyLoss`/`KillSwitch`）。
///
/// `ConfigReadOnly` / `ConfigSuspended` 由账户配置态（read_only /
/// suspended）产生；`TradingDisabled` 由交易开关 `trading_enabled = false`
/// 产生；`Manual` 由操作员 `account.freeze {frozen: true}` 产生；
/// `KillSwitch` 是进程级全局熔断（`risk.kill` / `risk.resume`）。配置态
/// 闸门只在启动时从 `.env` 再推导（#1057），`unfreeze` 只清运行时
/// 路由设置的闸门（Manual / DailyLoss / VenueRejection），不清配置态。
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GateReason {
    /// 场地拒单/自检失败/sweep 连击造成的自动冻结：场地健康探针可自愈。
    VenueRejection,
    /// 日内亏损熔断：UTC 日边界自动到期，无需操作。
    DailyLoss,
    /// 账户配置为 read_only：交易开关关，新单拒、平仓放行。
    ConfigReadOnly,
    /// 账户配置为 suspended：同上，且带运营原因。
    ConfigSuspended,
    /// 账户交易开关关闭：新单拒、平仓放行；由 `account.trading.set` 写
    /// 回 `.env` 后重启再生，运行时仅反映当前开关状态。
    TradingDisabled,
    /// 操作员 `account.freeze {frozen: true}` 手动冻结：仅 `account.unfreeze`
    /// / 手动解冻可解，健康探针与日内熔断均不得碰。
    Manual,
    /// 进程级全局熔断（`risk.kill`）：对全账户拒新单、放平仓，纯内存、
    /// 重启即清，`risk.resume` 或 `unfreeze {target: "global"}` 解除。
    KillSwitch,
}

impl GateReason {
    pub(crate) fn priority(&self) -> u8 {
        match self {
            GateReason::VenueRejection => 0,
            GateReason::DailyLoss => 1,
            GateReason::ConfigReadOnly => 2,
            GateReason::ConfigSuspended => 3,
            GateReason::TradingDisabled => 4,
            GateReason::Manual => 5,
            GateReason::KillSwitch => 6,
        }
    }

    /// 统一措辞，供面板与回执文案；前端不得据此推断 `kind` 之外的语义，
    /// 也不得编造恢复时间--恢复时间只读内核 `expires_at_ms`。
    pub(crate) fn label(&self) -> &'static str {
        match self {
            GateReason::VenueRejection => "venue rejection",
            GateReason::DailyLoss => "daily loss limit",
            GateReason::ConfigReadOnly => "account read-only",
            GateReason::ConfigSuspended => "account suspended",
            GateReason::TradingDisabled => "trading disabled",
            GateReason::Manual => "manual freeze",
            GateReason::KillSwitch => "kill switch",
        }
    }
}

/// 进程级全局熔断闸门的账户键：不与任何配置账户 id 冲突，仅供
/// `risk.kill` / `risk.resume` / `unfreeze {target: "global"}` 使用。
pub(crate) const GLOBAL_KILL_ACCOUNT: &str = "global";

/// 解冻操作的内核回执（#1045 API：`account.unfreeze`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UnfreezeResult {
    pub(crate) unfrozen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) previous_reason: Option<String>,
    /// 解冻后该账户是否可开新单（本闸已清且无更高优先级闸门）。
    pub(crate) entries_allowed: bool,
}

/// 冻结/熔断剩余状态簿（#834 收拢后的上帝结构一环）：`account_freezes` 与
/// `auto_frozen` 已被 `Core.account_gates` 单一闸门取代（#1045），本簿只保
/// 留与账户无关的全局/场地维连击与自由现金事实。
#[derive(Default)]
pub(crate) struct FreezeBook {
    pub(crate) consecutive_venue_rejects: u32,
    pub(crate) consecutive_venue_rejects_by_account: HashMap<String, u32>,
    pub(crate) venue_free_by_venue: HashMap<String, (i64, Decimal)>,
    pub(crate) consecutive_sweep_failures: u32,
    pub(crate) consecutive_sweep_failures_by_venue: HashMap<String, u32>,
}

impl FreezeBook {
    /// 单账户重置（`trading_reset` 路径）：该账户的按账户拒单 streak 移除；
    /// 全局拒单与 sweep streak 归零（一次账户级重置视为一次新鲜的开始）。
    /// by_venue sweep streak 不在此清--场地维的失败事实不属于账户重置。
    /// 账户入场闸门由 `Core` 同步清除（`reset_for_account` 调用方自行决定）。
    pub(crate) fn reset_for_account(&mut self, account_id: &str) {
        self.consecutive_venue_rejects = 0;
        self.consecutive_venue_rejects_by_account.remove(account_id);
        self.consecutive_sweep_failures = 0;
    }
}

impl Core {
    /// 单账户入场闸门（只读）。
    pub(crate) fn account_gate(&self, account: &str) -> Option<&AccountGate> {
        self.account_gates.get(account)
    }

    /// 全部闸门（stats 读出与 UI 呈现）。
    pub(crate) fn account_gates(&self) -> &HashMap<String, AccountGate> {
        &self.account_gates
    }

    /// 该账户当前是否被闸门拒绝入场。已过 `expires_at_ms` 的闸门按不阻断
    /// 处理（自动到期，无需额外清理动作）。
    pub(crate) fn account_gate_blocked(&self, account: &str, now_ms: i64) -> bool {
        let Some(g) = self.account_gates.get(account) else {
            return false;
        };
        g.blocked && g.expires_at_ms.is_none_or(|expires| now_ms < expires)
    }

    /// 设置或升级一账户的闸门。新原因的优先级 **严格高于** 现存**未过期**
    /// 闸门时才覆盖（先到者胜、更高优先级者覆盖），防止低优先级路由
    ///（如场地自愈）静默解除高优先级闸门（`Manual`/`DailyLoss`）。已过期
    /// 的闸门（DailyLoss 过 UTC 日翻界）不阻挡新闸门：过期的 DailyLoss(1)
    /// 不得吞掉随后的 VenueRejection(0)。返回是否写入。
    pub(crate) fn set_account_gate(
        &mut self,
        account: &str,
        reason: String,
        reason_kind: GateReason,
        expires_at_ms: Option<i64>,
        actor: &str,
        now_ms: i64,
    ) -> bool {
        let should_set = match self.account_gates.get(account) {
            None => true,
            Some(existing) => {
                // 已过期的闸门（expires_at_ms 已到）不再生效，视为不存在，
                // 避免过期的 DailyLoss(1) 吞掉随后的 VenueRejection(0)。
                let existing_live = existing
                    .expires_at_ms
                    .is_none_or(|expires| now_ms < expires);
                existing_live && reason_kind.priority() > existing.reason_kind.priority()
            }
        };
        if !should_set {
            return false;
        }
        self.account_gates.insert(
            account.to_string(),
            AccountGate {
                blocked: true,
                reason,
                reason_kind,
                since_ms: now_ms,
                expires_at_ms,
                actor: actor.to_string(),
            },
        );
        true
    }

    /// 清除一账户的闸门（解冻）。返回被清除的闸门，供调用方审计与回执。
    pub(crate) fn clear_account_gate(&mut self, account: &str) -> Option<AccountGate> {
        self.account_gates.remove(account)
    }

    /// 该账户的冻结原因文案（仅在被闸门阻断时有意义），供 `account.list`/
    /// `account.freeze` 回执与面板呈现。
    pub(crate) fn account_freeze_reason(&self, account: &str, now_ms: i64) -> Option<String> {
        self.account_gate(account).and_then(|g| {
            if g.blocked && g.expires_at_ms.is_none_or(|expires| now_ms < expires) {
                Some(g.reason.clone())
            } else {
                None
            }
        })
    }

    /// 入场闸判定：账户被闸门阻断，且意图不是平仓（平仓腿永远放行，#174
    /// 逃生舱，按账户独立计）。新单拒、平仓放行。
    ///
    /// 两条依据：
    /// 1. `account_gates` 内存表（由 `emit_daily_trip`、手动冻结、
    ///    场地自愈、进程级熔断设置） - 运行时闸门；
    /// 2. 进程级全局熔断闸门 `GLOBAL_KILL_ACCOUNT` - 对全账户生效。
    ///
    /// `ConfigReadOnly` 是唯一也拦平仓的姿态（`ReadOnly` 配置态映射其上；
    /// `Frozen`/`TradingDisabled`/`Manual`/`KillSwitch` 均放行平仓，绝不
    /// 困住仓位）。**不**回退到持久化的 `daily_loss_tripped_for` 闩锁：
    /// 重启后 `tripped` 不持久化，故重启不自动重触发日内熔断；解冻后
    /// 当日余下时间可交易。这是 #1045 裁决 v3：「删除 tripped* 落盘字段、
    /// 删读时重闩、重启/解冻后当日不再自动重触发」。
    pub(crate) fn account_gate_permits(&self, req: &OrderRequest, now_ms: i64) -> CoreResult<()> {
        let account_id = req.account_id.as_str();
        let is_close = crate::risk::is_close_intent(&req.internal_key);

        // 进程级全局熔断：对每个账户的新单都拒，平仓永远放行。
        if let Some(gate) = self.account_gate(GLOBAL_KILL_ACCOUNT)
            && gate.blocked
            && gate.expires_at_ms.is_none_or(|expires| now_ms < expires)
            && !is_close
        {
            return Err(CoreError::new(
                CoreErrorCode::KillSwitchActive,
                format!(
                    "kill switch active ({}) - new orders refused on all accounts, exits keep flowing",
                    gate.reason
                ),
            ));
        }

        if let Some(gate) = self.account_gate(account_id)
            && gate.blocked
            && gate.expires_at_ms.is_none_or(|expires| now_ms < expires)
        {
            // ConfigReadOnly 是唯一也拦平仓的姿态；其余闸门（Frozen /
            // TradingDisabled / Manual / VenueRejection / DailyLoss / KillSwitch）
            // 均放行平仓，绝不因闸门而困仓。
            let blocks_closes = matches!(gate.reason_kind, GateReason::ConfigReadOnly);
            if !is_close || blocks_closes {
                let message = match gate.reason_kind {
                    GateReason::DailyLoss => format!(
                        "Daily loss limit for account {} - entries frozen until the next UTC day (exits keep flowing)",
                        account_id
                    ),
                    _ => format!(
                        "account `{}` is frozen (account-scoped gate, {}): {} - {}",
                        account_id,
                        gate.reason_kind.label(),
                        gate.reason,
                        if is_close {
                            "closes refused too (read_only)"
                        } else {
                            "new orders refused, exits keep flowing"
                        }
                    ),
                };
                return Err(CoreError::new(CoreErrorCode::AccountLimit, message));
            }
        }

        Ok(())
    }

    /// Freeze ONE account (operator route, `account.freeze {frozen: true}`):
    /// `Manual` 闸门，优先级最高，健康探针与日内熔断均不得自解。会话内
    /// 立即生效，重启即清，永不落盘。同一账户重复冻结保留首次原因。
    pub fn freeze_account(&mut self, account: &str, reason: String) {
        let now_ms = blitzkrieg_market_api::net::now_ms();
        let _ = self.set_account_gate(
            account,
            reason.clone(),
            GateReason::Manual,
            None,
            &format!("operator:{account}"),
            now_ms,
        );
        tracing::error!(account = %account, reason = %reason, "account frozen (manual gate, in-memory only, restart-clear)");
    }

    /// The AUTOMATIC freeze route (venue-rejection streak etc.): `VenueRejection`
    /// 闸门，优先级最低，仅在账户当前无同等或更高优先级闸门时上锁；场地
    /// 健康探针可解（仅此一条），`Manual`/`DailyLoss` 绝不自解。不登记任何
    /// 持久态。
    pub(crate) fn freeze_account_auto(&mut self, account: &str, reason: String, now_ms: i64) {
        let _ = self.set_account_gate(
            account,
            reason.clone(),
            GateReason::VenueRejection,
            None,
            &format!("auto:{account}"),
            now_ms,
        );
        tracing::warn!(account = %account, reason = %reason, "account auto-frozen (venue rejection gate, in-memory only)");
    }

    /// `account.freeze {frozen: true}` 的显式运营路由：`Manual` 闸门，与
    /// [`Self::freeze_account`] 同一优先级，保留为调用方语义清晰的别名。
    pub fn freeze_account_manual(&mut self, account: &str, reason: String) {
        self.freeze_account(account, reason);
    }

    /// Lift one account's gate (`account.unfreeze` / `account.freeze {frozen: false}`
    /// / the reset path). Unknown account = no-op.
    pub fn resume_account(&mut self, account: &str) -> bool {
        self.clear_account_gate(account).is_some()
    }

    /// #1045 API: 解冻一账户并返回内核回执 `{unfrozen, previousReason, entriesAllowed}`，
    /// 并写审计日志（actor/reason/时间）。解冻只放行入场，不碰账本任何数字。
    ///
    /// #1057（裁决 A 真单门）：
    /// - 阻断 1：先 persist 配置态（把 `trading_enabled` 置 true、`status`
    ///   置 `Active`，落盘到 `.env`），**再** clear gate。persist 失败则
    ///   什么都不变，gate 仍在，绝不假成功。
    /// - 阻断 3：全局熔断键 `"global"` 解冻时同时 `risk.resume()`，使
    ///   tradingFrozen 与 accountGates 单点化。
    /// - 阻断 6：`entries_allowed` 仅由 gate 得出（clear 后读
    ///   `account_gate_blocked`），不再并读 `entry_open`。
    pub(crate) fn unfreeze_account(
        &mut self,
        account: &str,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<UnfreezeResult> {
        let id = crate::model::AccountId::from(account);
        let previous = self.account_gate(account).cloned();

        // #1057 阻断 3：全局熔断解冻同时清 risk 标志。
        if account == GLOBAL_KILL_ACCOUNT && previous.is_some() {
            self.risk.resume();
        }

        // #1057 阻断 1 & 4：先 persist 配置态（unfreeze 写回 Active /
        // trading_enabled=true 并落盘），失败则什么都不变、不 clear gate。
        if account != GLOBAL_KILL_ACCOUNT
            && let Some(acct) = self.accounts.account(&id)
            && (!acct.trading_enabled || !acct.status.allows_entries())
        {
            self.set_account_trading_persisting(&id, true, actor, "unfreeze", now_ms)?;
            // persist 成功后，把 lifecycle status 也重置为 Active
            //（loosening，直接修改内存；set_status 是 tighten-only
            // 不能用，故这里直写）。
            if let Some(acct) = self.accounts.account_mut(&id)
                && !acct.status.allows_entries()
            {
                acct.status = AccountStatus::Active;
                acct.updated_at_ms = now_ms;
            }
        }

        // 然后 clear gate：先 persist 后 clear，失败则 gate 仍在。
        let previous = self.clear_account_gate(account);
        let previous_reason = previous.as_ref().map(|g| g.reason.clone());

        // 清 session trip latch：解冻后当日余下时间可交易，不再被旧 latch 重拒。
        self.positions.clear_daily_tripped_for(&id);

        // 阻断 6：entries_allowed 仅由 gate 得出（clear 后复读），显示
        // 与执行同源，不再并读 entry_open。
        let entries_allowed = !self.account_gate_blocked(account, now_ms);

        if previous.is_some() {
            tracing::info!(
                actor = %actor,
                account = %account,
                previous_reason = ?previous_reason,
                entries_allowed,
                "account unfrozen (config written back first, then gate cleared; realized ledger untouched)"
            );
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::RiskRejected,
                message: format!(
                    "account {account} unfrozen by {actor} - previous gate: {}",
                    previous_reason
                        .as_ref()
                        .map_or_else(|| "none".to_string(), |r| r.clone())
                ),
            });
        }
        Ok(UnfreezeResult {
            unfrozen: previous.is_some(),
            previous_reason,
            entries_allowed,
        })
    }

    /// 冻结自愈的解冻半边（#1045：只清 `VenueRejection`，不得清 `Manual`/`DailyLoss`。
    /// 手动冻结与日内熔断均不属于「场地不健康」的自愈范围。
    ///
    /// 每个调用方（reconcile sweep、venue ack）先等出 `FREEZE_AUTO_LIFT_MIN_MS`
    /// (#867) 再来，避免波动期内的反复解锁。
    pub(crate) fn auto_lift_if_venue_healthy(&mut self, account: &str, trigger: &str, now_ms: i64) {
        let Some(gate) = self.account_gate(account) else {
            return;
        };
        if gate.reason_kind != GateReason::VenueRejection {
            return;
        }
        if now_ms - gate.since_ms < crate::service::FREEZE_AUTO_LIFT_MIN_MS {
            return;
        }
        let since_ms = gate.since_ms;
        self.clear_account_gate(account);
        let minutes = (now_ms - since_ms).max(0) / 60_000;
        tracing::warn!(
            account = %account,
            frozen_duration_ms = now_ms - since_ms,
            trigger = %trigger,
            "venue-rejection gate auto-lifted - venue proved healthy"
        );
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!(
                "自动冻结已解除（{trigger}）：账户 {account} 场地拒单闸门 {minutes} 分钟后场地恢复"
            ),
        });
    }

    /// Freeze trading (kill switch) with a reason the panel can show. A
    /// restart or an explicit `risk.resume` clears it; the startup self-check
    /// re-probes either way. The freeze is logged at error level (#184) - a kill
    /// switch is the loudest thing this kernel does, and every path that can
    /// reach it (venue-refusal streak, failed self-check, failed sweep, audit
    /// halt) must be countable in a run log.
    ///
    /// It deliberately does NOT write the error slot: the slot keeps the ROOT
    /// CAUSE (the raw venue error that pushed a streak over the line), and
    /// `engine.stats.tradingFrozen.reason` already carries this message for the
    /// panel's freeze banner. Overwriting the slot here is what would make it
    /// show a paraphrase instead of the error an operator has to act on.
    pub(crate) fn freeze_trading(&mut self, reason: String, now_ms: i64) {
        if self.risk.is_killed() {
            return; // already frozen - do not re-alarm on every rejection
        }
        self.risk.kill(reason.clone());
        // #1056: the kill switch is pure in-memory - it never outlives the
        // process; a restart comes up clear and the operator re-arms it via
        // `risk.resume` / `unfreeze {target: "global"}`.
        let _ = self.set_account_gate(
            GLOBAL_KILL_ACCOUNT,
            reason.clone(),
            GateReason::KillSwitch,
            None,
            "system",
            now_ms,
        );
        self.persist_risk_state();
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!("trading frozen: {reason}"),
        });
        tracing::error!(reason = %reason, "trading frozen (kill switch)");
        self.emit_error_event(crate::model::CoreError::new(
            blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            reason,
        ));
    }

    pub(crate) fn emit_order(&self, id: &str) {
        if let Some(o) = self.ome.get(id) {
            // Durable persistence: every status change flows through here (submit /
            // confirm / reject / bind / cancel), so the order log always reflects
            // the latest state and a restart can rebuild it (orphan guard).
            self.persist_order(id);
            self.emit(Event::OrderUpdate { order: o.clone() });
        }
    }

    pub(crate) fn emit_fill(&self, d: FillDelta) {
        if let Some(o) = self.ome.get(&d.order_id) {
            self.persist_order(&d.order_id);
            self.emit(Event::Fill {
                delta: d.into(),
                order: o.clone(),
            });
        }
    }

    pub(crate) fn new_order_id(&mut self) -> String {
        // "live" is reserved for orders that can actually reach a venue. A
        // read-only order is not live - stamping it `live_` would put a
        // misleading label on the one field an operator reads to decide what is
        // real, so it is named for what it is.
        let prefix = match self.config.mode {
            Mode::Live => "live",
            Mode::ReadOnly => "readonly",
            Mode::Dry => "dry",
        };
        let id = format!("{prefix}_{}", self.next_id);
        self.next_id += 1;
        id
    }

    // ── Risk ───────────────────────────────────────────────────────────────
    pub fn kill(&mut self, reason: String) {
        self.risk.kill(reason.clone());
        // #1056: the kill switch is pure in-memory - it never outlives the
        // process; a restart comes up clear and the operator re-arms it.
        let _ = self.set_account_gate(
            GLOBAL_KILL_ACCOUNT,
            reason.clone(),
            GateReason::KillSwitch,
            None,
            "system",
            blitzkrieg_market_api::net::now_ms(),
        );
        self.persist_risk_state();
        tracing::error!(reason = %reason, "kill switch engaged (in-memory, restart-clear)");
        self.emit(Event::RiskAlert {
            code: CoreErrorCode::KillSwitchActive,
            message: reason,
        });
    }

    /// Flush pending near-miss records to disk (call on shutdown).
    pub fn flush_near_misses(&mut self) {
        if let Some(path) = self.config.near_miss_path.clone() {
            let recs = self
                .engine
                .as_mut()
                .map(|e| e.flush_near_misses())
                .unwrap_or_default();
            if !recs.is_empty()
                && let Err(e) =
                    crate::shadow::persist_near_misses(std::path::Path::new(&path), &recs)
            {
                tracing::warn!(error = %e, path = %path, "near-miss flush failed");
            }
        }
    }

    pub fn resume(&mut self) {
        self.risk.resume();
        // #1056: the kill switch never outlives the process - resume clears the
        // in-memory risk gate AND the global `GLOBAL_KILL_ACCOUNT` gate. Nothing
        // is persisted, so a restart comes up clear and the operator must
        // explicitly re-arm.
        self.account_gates.remove(GLOBAL_KILL_ACCOUNT);
        self.persist_risk_state();
    }

    pub fn is_killed(&self) -> bool {
        self.risk.is_killed()
    }

    /// TEST SEAM ONLY (test cfg or feature `test-support`; #780/#825 boundary
    /// hygiene - the same gate [`Core::ledger_mut`] carries): blunt access to
    /// the live risk gate configuration. The runtime-config path does NOT come
    /// through here - it applies audited, hot-reload-checked limits via
    /// [`Core::apply_risk_limits`] / [`Core::apply_systemic_limits`]
    /// (`place()` reads the config on every check).
    #[cfg(any(test, feature = "test-support"))]
    pub fn risk_config_mut(&mut self) -> &mut RiskConfig {
        self.risk.config_mut()
    }

    /// Test-only: re-arm the virtual-account book after construction (the
    /// production surface is `CoreConfig::virtual_limits` at boot; tests
    /// build the core first and opt a strategy in afterwards).
    #[cfg(test)]
    pub(crate) fn set_virtual_limits_for_test(
        &mut self,
        limits: HashMap<String, VirtualAccountLimit>,
    ) {
        self.virtual_accounts = crate::risk::virtual_account::VirtualAccounts::new(&limits);
    }

    /// The live risk gate config - the read side of
    /// [`Self::risk_config_mut`]: a `risk.setLimits`/`risk.setSystemic` write
    /// is visible here immediately, which is what keeps the `risk.limits`
    /// readout honest after a hot edit.
    pub fn risk_config(&self) -> &RiskConfig {
        self.risk.config()
    }

    pub(crate) fn emit(&self, ev: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }
}
