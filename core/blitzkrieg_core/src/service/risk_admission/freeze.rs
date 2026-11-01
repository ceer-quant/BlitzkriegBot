use super::*;

/// 冻结/熔断状态簿（#834，Core 上帝结构第一环）：原散落在 `Core` 上的 7 个
/// 裸字段收拢为一份内聚状态，`freeze.rs` 是它的唯一管理者。字段语义与
/// 收拢前逐字一致；`Core` 只持有 `freeze_book: FreezeBook` 一个字段。
///
/// - `account_freezes`：account-scoped kill book（account id → freeze
///   reason）。冻结账户在提交闸拒新单、引擎镜像丢 entry 候选，平仓腿照走
///   ——「拒开仓、放平仓」合同。全局 kill switch（`risk.kill`）依旧压过
///   这里的一切。
/// - `auto_frozen`：冻结自愈簿（2026-10-28 真线事故）——account id → 冻结
///   时间戳 (ms)。只有三条自动冻结路由（拒单 streak / 自检失败 / sweep
///   失败 streak）登记，`auto_lift_if_venue_healthy` 据此区分「内核凭间接
///   证据冻的」与操作员手动 `account.freeze`——后者永不自解。手动冻结路由
///   会先摘戳（操作员裁决压过自愈）。
/// - `consecutive_venue_rejects`（全局）与 `.._by_account`（按账户）：
///   LIVE venue 拒单连击，成功下单清零；顶到阈值触发冻结。冻结决策读
///   by_account 图——一个场地的故障只冻绑它的账户，不冻整个进程。
/// - `consecutive_sweep_failures`（全局，#673 退避 streak）与
///   `.._by_venue`（按场地，#624 venue 维冻结，2026-10-25）：对账 sweep
///   连败 streak，成功 sweep 清零；顶到阈值冻结——瞎的安全网才是让幽灵
///   和孤儿堆积的失败模式（E31-b）。每个插件以自己的名字上报，一个场地
///   的失败不再搭全局计数便车、也不冻兄弟场地的账户。
/// - `venue_free_by_venue`：各场地最近一次上报的自由现金 venue →
///   `(at_ms, free)`——审计的场地腿只读被审计（活跃）账户绑定场地的上报
///   ，兄弟场地的现金是另一本账的真值。
#[derive(Default)]
pub(crate) struct FreezeBook {
    pub(crate) account_freezes: HashMap<String, String>,
    pub(crate) auto_frozen: HashMap<String, i64>,
    pub(crate) consecutive_venue_rejects: u32,
    pub(crate) consecutive_venue_rejects_by_account: HashMap<String, u32>,
    pub(crate) venue_free_by_venue: HashMap<String, (i64, Decimal)>,
    pub(crate) consecutive_sweep_failures: u32,
    pub(crate) consecutive_sweep_failures_by_venue: HashMap<String, u32>,
}

impl FreezeBook {
    /// 单账户重置（`trading_reset` 路径，原 941-946 五连的收拢）：该账户
    /// 的冻结簿/自愈簿条目移除、按账户的拒单 streak 移除；全局拒单与
    /// sweep streak 归零（一次账户级重置视为一次新鲜的开始，历史语义）。
    /// by_venue sweep streak 不在此清——场地维的失败事实不属于账户重置。
    pub(crate) fn reset_for_account(&mut self, account_id: &str) {
        self.consecutive_venue_rejects = 0;
        self.consecutive_venue_rejects_by_account.remove(account_id);
        self.account_freezes.remove(account_id);
        self.auto_frozen.remove(account_id);
        self.consecutive_sweep_failures = 0;
    }
}

impl Core {
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

    /// Freeze ONE account (the account-scoped kill switch). Same 「拒开仓、
    /// 放平仓」 contract as the global kill and the trading switch: the
    /// submission gate refuses the account's new orders, the engine mirror
    /// drops its entry candidates, its exits keep flowing. The reason is
    /// shown by `account.list` (`accountFreezes`) and the doctor's accounts
    /// section. Idempotent: the first reason wins — a re-freeze while
    /// already frozen does not overwrite the cause an operator first saw.
    pub fn freeze_account(&mut self, account: &str, reason: String) {
        if self.freeze_book.account_freezes.contains_key(account) {
            return;
        }
        self.freeze_book
            .account_freezes
            .insert(account.to_string(), reason.clone());
        self.engine_set_account_frozen(account, true);
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!("account {account} frozen: {reason}"),
        });
        tracing::error!(account = %account, reason = %reason, "account frozen (account-scoped kill switch)");
    }

    /// The AUTOMATIC freeze route: [`Self::freeze_account`] plus a timestamp
    /// in `auto_frozen` — but only when the freeze actually landed. The
    /// freeze is idempotent (first reason wins), so a re-freeze of an
    /// already-frozen account keeps the ORIGINAL stamp: the clock an
    /// auto-lift debounces against is the moment the operator first lost
    /// the account, not the latest symptom.
    ///
    /// 手动冻结（`account.freeze` 直达 `freeze_account`）从不登记 —— 操作员
    /// 的裁决只能由操作员推翻，健康探针不得越权。
    pub(crate) fn freeze_account_auto(&mut self, account: &str, reason: String, now_ms: i64) {
        let fresh = !self.freeze_book.account_freezes.contains_key(account);
        self.freeze_account(account, reason);
        if fresh {
            self.freeze_book
                .auto_frozen
                .insert(account.to_string(), now_ms);
        }
    }

    /// The OPERATOR freeze route: [`Self::freeze_account`] plus dropping any
    /// `auto_frozen` stamp first.
    ///
    /// `freeze_account` is idempotent, so freezing an account the kernel had
    /// ALREADY auto-frozen used to leave the auto stamp in place — the next
    /// healthy-venue sweep (`auto_lift_if_venue_healthy`) then resumed the
    /// account and the operator's kill switch was silently lifted. Clearing the
    /// ledger makes the operator verdict outrank the self-heal by construction.
    pub fn freeze_account_manual(&mut self, account: &str, reason: String) {
        self.freeze_book.auto_frozen.remove(account);
        self.freeze_account(account, reason);
    }

    /// 冻结自愈的解冻半边：only an AUTO-frozen account may self-lift, and
    /// only on positive evidence the venue recovered (`trigger` names it).
    /// A manual freeze is never touched — `auto_frozen` is the ledger that
    /// separates kernel circumstantial freezes from operator verdicts.
    pub(crate) fn auto_lift_if_venue_healthy(&mut self, account: &str, trigger: &str, now_ms: i64) {
        let Some(frozen_at) = self.freeze_book.auto_frozen.get(account).copied() else {
            return;
        };
        self.resume_account(account);
        self.freeze_book.auto_frozen.remove(account);
        let minutes = (now_ms - frozen_at).max(0) / 60_000;
        tracing::warn!(
            account = %account,
            frozen_duration_ms = now_ms - frozen_at,
            trigger = %trigger,
            "auto-frozen account auto-lifted — venue proved healthy"
        );
        self.emit(Event::RiskAlert {
            code: blitzkrieg_market_api::CoreErrorCode::KillSwitchActive,
            message: format!(
                "自动冻结已解除（{trigger}）：账户 {account} 冻结 {minutes} 分钟后场地恢复"
            ),
        });
    }

    /// Lift one account's freeze (`account.freeze` with `frozen: false`, the
    /// reset path, or a future operator verb). Unknown account = no-op: the
    /// freeze book only ever contains accounts the kernel knows.
    pub fn resume_account(&mut self, account: &str) {
        if self.freeze_book.account_freezes.remove(account).is_some() {
            self.engine_set_account_frozen(account, false);
            tracing::info!(account = %account, "account freeze lifted");
        }
        // 手动解冻也要摘掉自愈时间戳：freeze book 与 auto ledger 对同一
        // 账户要么都有意、要么都干净，半悬空状态只会让下一次 auto-lift
        // 读到一个已不存在的冻结。
        self.freeze_book.auto_frozen.remove(account);
    }

    /// Why an account is frozen, when it is.
    pub fn account_freeze_reason(&self, account: &str) -> Option<&str> {
        self.freeze_book
            .account_freezes
            .get(account)
            .map(String::as_str)
    }

    /// The whole freeze book, for the stats readout.
    pub fn account_freezes(&self) -> &HashMap<String, String> {
        &self.freeze_book.account_freezes
    }

    /// Is this account's order admitted under the account-scoped kill
    /// switch? CLOSING intents always pass (the #174 escape hatch is per
    /// account here, exactly as it is globally). Called from the submission
    /// gate right beside Gate 2.
    pub(crate) fn account_freeze_permits(&self, req: &OrderRequest) -> CoreResult<()> {
        if let Some(reason) = self
            .freeze_book
            .account_freezes
            .get(req.account_id.as_str())
            && !crate::risk::is_close_intent(&req.internal_key)
        {
            return Err(CoreError::new(
                CoreErrorCode::KillSwitchActive,
                format!(
                    "account `{}` is frozen (account-scoped kill switch): {reason} — \
                     new orders refused, exits keep flowing",
                    req.account_id
                ),
            ));
        }
        Ok(())
    }

    /// Push one account's freeze fact into the engine mirror. `None` = the
    /// engine is not installed (backtest/in-process shapes) — nothing to
    /// mirror.
    pub(crate) fn engine_set_account_frozen(&mut self, account: &str, frozen: bool) {
        if let Some(e) = self.engine.as_mut() {
            e.set_account_frozen(account, frozen);
        }
    }

    pub(crate) fn emit(&self, ev: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
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
    }

    pub fn is_killed(&self) -> bool {
        self.risk.is_killed()
    }

    /// TEST SEAM ONLY (feature `test-support`; #780 boundary hygiene — the
    /// same gate [`Core::ledger_mut`] carries): blunt access to the live risk
    /// gate configuration. The runtime-config path does NOT come through
    /// here — it applies audited, hot-reload-checked limits via
    /// [`Core::apply_risk_limits`] / [`Core::apply_systemic_limits`]
    /// (`place()` reads the config on every check).
    #[cfg(feature = "test-support")]
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
}
