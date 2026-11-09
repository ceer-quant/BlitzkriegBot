use super::*;
use serde::Serialize;

/// 账户入场闸门（#1045）：每账户一份，**纯内存、永不落盘**，进程重启即清，
/// 唯一由 `account.freeze` / `account.unfreeze`、日内亏损熔断和场地自愈三条
/// 路由改写。所有入场路径只读 `gate.blocked`；出场永不阻断。
///
/// 取代旧 `FreezeBook` 里的 `account_freezes` + `auto_frozen` 两张持久表：
/// 不再有任何冻结姿态随进程重启而重播。`realized_pnl_usd` 继续持久化（资金
/// 真相），重启只清闸门；重启后若账本里的已实现亏损仍超当日上限，会在
/// 下一次 `emit_daily_trip` 时按账本重新上 `DailyLoss` 闸门——这是「按账本
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
/// `VenueRejection < DailyLoss < ConfigReadOnly < ConfigSuspended < Manual`。
///
/// 更高优先级的闸门可覆盖更低的；较低优先级的路由永远不会静默解除更高
/// 优先级的闸门（场地健康探针只解 `VenueRejection`，碰不着 `Manual`/`DailyLoss`）。
///
/// `ConfigReadOnly` / `ConfigSuspended` 由账户配置态（`account.trading.set` 的
/// read_only / suspended）产生，由 #1044 的 WebUI/TUI 呈现层消费；本单
/// 只声明它们以统一闸门模型，优先级高于 `DailyLoss`/`VenueRejection`，低
/// 于 `Manual`，且不被任何自愈路由碰触。
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
    /// 操作员 `account.freeze {frozen: true}` 手动冻结：仅 `account.unfreeze`
    /// / 手动解冻可解，健康探针与日内熔断均不得碰。
    Manual,
}

impl GateReason {
    pub(crate) fn priority(&self) -> u8 {
        match self {
            GateReason::VenueRejection => 0,
            GateReason::DailyLoss => 1,
            GateReason::ConfigReadOnly => 2,
            GateReason::ConfigSuspended => 3,
            GateReason::Manual => 4,
        }
    }

    /// 统一措辞，供面板与回执文案；前端不得据此推断 `kind` 之外的语义，
    /// 也不得编造恢复时间——恢复时间只读内核 `expires_at_ms`。
    pub(crate) fn label(&self) -> &'static str {
        match self {
            GateReason::VenueRejection => "venue rejection",
            GateReason::DailyLoss => "daily loss limit",
            GateReason::ConfigReadOnly => "account read-only",
            GateReason::ConfigSuspended => "account suspended",
            GateReason::Manual => "manual freeze",
        }
    }
}

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
    /// by_venue sweep streak 不在此清——场地维的失败事实不属于账户重置。
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

    /// 设置或升级一账户的闸门。新原因的优先级 **严格高于** 现存闸门时才
    /// 覆盖（先到者胜、更高优先级者覆盖），防止低优先级路由（如场地
    /// 自愈）静默解除高优先级闸门（`Manual`/`DailyLoss`）。返回是否写入。
    pub(crate) fn set_account_gate(
        &mut self,
        account: &str,
        reason: String,
        reason_kind: GateReason,
        since_ms: i64,
        expires_at_ms: Option<i64>,
        actor: &str,
    ) -> bool {
        let should_set = match self.account_gates.get(account) {
            None => true,
            Some(existing) => reason_kind.priority() > existing.reason_kind.priority(),
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
                since_ms,
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
    /// 闸门纯内存、重启即清：重启首 tick 前 `emit_daily_trip` 尚未复立
    //   DailyLoss 闸门，此时回退到持久化的 `daily_loss_tripped_for` 门闩，
    //   避免重启洗今天的亏损预算。`emit_daily_trip` 会在下一 tick 复立闸门。
    pub(crate) fn account_gate_permits(&self, req: &OrderRequest, now_ms: i64) -> CoreResult<()> {
        let account_id = req.account_id.as_str();
        let is_close = crate::risk::is_close_intent(&req.internal_key);

        // 1) 显式账户闸门（由 emit_daily_trip、手动冻结等设置）。
        if let Some(gate) = self.account_gate(account_id) {
            if gate.blocked && gate.expires_at_ms.is_none_or(|expires| now_ms < expires) && !is_close {
                let message = match gate.reason_kind {
                    GateReason::DailyLoss => format!(
                        "Daily loss limit for account {} — entries frozen until the next UTC day (exits keep flowing)",
                        account_id
                    ),
                    _ => format!(
                        "account `{}` is frozen (account-scoped gate, {}): {} — new orders refused, exits keep flowing",
                        account_id,
                        gate.reason_kind.label(),
                        gate.reason
                    ),
                };
                return Err(CoreError::new(CoreErrorCode::KillSwitchActive, message));
            }
        }

        // 2) 重启首 tick 回退：闸门尚未复立，但门闩已落盘，仍拒新单。
        if !is_close && self.positions.daily_loss_tripped_for(account_id) {
            return Err(CoreError::new(
                CoreErrorCode::KillSwitchActive,
                format!(
                    "Daily loss limit for account {} — entries frozen until the next UTC day (exits keep flowing)",
                    account_id
                ),
            ));
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
            now_ms,
            None,
            &format!("operator:{account}"),
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
            now_ms,
            None,
            &format!("auto:{account}"),
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
    pub(crate) fn unfreeze_account(
        &mut self,
        account: &str,
        actor: &str,
        now_ms: i64,
    ) -> UnfreezeResult {
        let previous = self.clear_account_gate(account);
        let previous_reason = previous.as_ref().map(|g| g.reason.clone());
        // 解冻后立即复读：单闸门模型下本账户无其他并行闸门。
        let entries_allowed = !self.account_gate_blocked(account, now_ms);
        if previous.is_some() {
            tracing::info!(
                actor = %actor,
                account = %account,
                previous_reason = ?previous_reason,
                entries_allowed,
                "account unfrozen (gate cleared, in-memory only; realized ledger untouched)"
            );
            self.emit(Event::RiskAlert {
                code: blitzkrieg_market_api::CoreErrorCode::RiskRejected,
                message: format!(
                    "account {account} unfrozen by {actor} — previous gate: {}",
                    previous_reason
                        .as_ref()
                        .map_or_else(|| "none".to_string(), |r| r.clone())
                ),
            });
        }
        UnfreezeResult {
            unfrozen: previous.is_some(),
            previous_reason,
            entries_allowed,
        }
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
            "venue-rejection gate auto-lifted — venue proved healthy"
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
    /// re-probes either way. The freeze is logged at error level (#184) — a kill
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
            return; // already frozen — do not re-alarm on every rejection
        }
        let _ = now_ms;
        self.risk.kill(reason.clone());
        // #861: the kill switch outlives the process — the reason an operator
        // must see travels with it.
        if let Some(c) = &mut self.risk_state {
            c.file.kill = Some(KillCell {
                reason: reason.clone(),
            });
            c.file.saved_at_ms = now_ms;
        }
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
        // read-only order is not live — stamping it `live_` would put a
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
        // #861: same durable kill as freeze_trading — the direct path (no
        // freeze_trading alarm) still must not die with the process.
        if let Some(c) = &mut self.risk_state {
            c.file.kill = Some(KillCell {
                reason: reason.clone(),
            });
            c.file.saved_at_ms = blitzkrieg_market_api::net::now_ms();
        }
        self.persist_risk_state();
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
        // #861: a lifted kill stays lifted across the restart.
        if let Some(c) = &mut self.risk_state {
            c.file.kill = None;
            c.file.saved_at_ms = blitzkrieg_market_api::net::now_ms();
        }
        self.persist_risk_state();
    }

    pub fn is_killed(&self) -> bool {
        self.risk.is_killed()
    }

    /// TEST SEAM ONLY (test cfg or feature `test-support`; #780/#825 boundary
    /// hygiene — the same gate [`Core::ledger_mut`] carries): blunt access to
    /// the live risk gate configuration. The runtime-config path does NOT come
    /// through here — it applies audited, hot-reload-checked limits via
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

    /// The live risk gate config — the read side of
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
