//! The risk knobs' IPC writers and the kernel's error/daily-budget
//! emitters, moved verbatim from `service.rs`'s risk-admission column
//! (pure code motion, no logic edits): `system.reset_trading`'s three-gate
//! reset, `risk.limits` / `systemic.limits` / `exit.set` /
//! `round_duration.set`, the intent-decision audit and the daily-loss trip
//! with its suppressed-stop reports.

use crate::ipc::schema::Event;
use crate::model::*;
use crate::position::EquityBasis;
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;

use super::Core;
use super::config::{AuditRuntime, LastError};

impl Core {
    /// Apply a hot update of the ENTRY limits (#191) — the whole safe subset
    /// ([`crate::risk::HOT_RELOADABLE`]), in memory, with an audit record.
    ///
    /// What makes this safe enough to offer at runtime is what the whitelist
    /// selects for: the order path reads every one of these LIVE (the risk gate
    /// on each check, the engine's own config on each ticket), they bound NEW
    /// exposure only — a close is exempt by construction (#174), so tightening a
    /// limit can never trap an open position — and none of them carries
    /// accumulated state, so a change cannot retroactively decide something the
    /// day's book of events already decided.
    ///
    /// Atomic: the patch is validated in full (non-negative, coherent band)
    /// before anything is written, so a refused patch leaves NO trace. Nothing is
    /// persisted — a restart re-applies the startup flags (#191: "hot" must never
    /// be read as "permanent"), and the audit says so in the same breath.
    ///
    /// The audit is one `tracing::info!` line per changed field (`target: "risk"`,
    /// old → new + actor + instant) in the run log every deployment already
    /// writes; the same facts are returned to the caller, so the reply and the log
    /// cannot tell two different stories.
    pub fn apply_risk_limits(
        &mut self,
        patch: &crate::ipc::schema::SetRiskLimitsParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::RiskLimitUpdate> {
        use crate::ipc::schema::{RISK_LIMIT_UPDATE_NOTE, RiskLimitChange, RiskLimitUpdate};
        use crate::risk::HOT_RELOADABLE;

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "risk.setLimits needs at least one field; hot-reloadable: {}",
                    HOT_RELOADABLE.join(", ")
                ),
            ));
        }

        // Plan first, write later: a bad field must not leave a half-applied
        // patch behind.
        let risk = self.risk.config();
        let (notional, notional_pct, open_notional) = (
            risk.max_order_notional,
            risk.max_order_notional_pct,
            risk.max_open_notional_usd,
        );
        let (min_shares, max_shares) = (self.config.min_shares, self.config.max_shares);
        let mut changes: Vec<RiskLimitChange> = Vec::new();
        let mut plan =
            |field: &'static str, from: Decimal, to: Option<Decimal>| -> CoreResult<()> {
                let Some(to) = to else {
                    return Ok(());
                };
                if to < Decimal::ZERO {
                    return Err(CoreError::new(
                        CoreErrorCode::InvalidParams,
                        format!("{field} must be >= 0, got {to}; nothing was applied"),
                    ));
                }
                changes.push(RiskLimitChange { field, from, to });
                Ok(())
            };
        plan("maxOrderNotional", notional, patch.max_order_notional)?;
        plan(
            "maxOrderNotionalPct",
            notional_pct,
            patch.max_order_notional_pct,
        )?;
        plan(
            "maxOpenNotionalUsd",
            open_notional,
            patch.max_open_notional_usd,
        )?;
        plan("minShares", min_shares, patch.min_shares)?;
        plan("maxShares", max_shares, patch.max_shares)?;

        // The band is a band: `effective_sizing` caps the floor by the ceiling, so
        // an incoherent pair would silently read as something the operator did not
        // ask for. Refuse it instead.
        let (new_min, new_max) = (
            patch.min_shares.unwrap_or(min_shares),
            patch.max_shares.unwrap_or(max_shares),
        );
        if new_min > new_max {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "minShares {new_min} would exceed maxShares {new_max}; \
                     the share band must stay coherent — nothing was applied"
                ),
            ));
        }

        let band_moved = changes
            .iter()
            .any(|c| c.field == "minShares" || c.field == "maxShares");
        for c in &changes {
            match c.field {
                "maxOrderNotional" => self.risk.config_mut().max_order_notional = c.to,
                "maxOrderNotionalPct" => self.risk.config_mut().max_order_notional_pct = c.to,
                "maxOpenNotionalUsd" => self.risk.config_mut().max_open_notional_usd = c.to,
                "minShares" => self.config.min_shares = c.to,
                "maxShares" => self.config.max_shares = c.to,
                // Unreachable today (both lists live ten lines apart), and loud
                // rather than silent if that ever stops being true: an arm that
                // matched nothing would leave the reply and the audit claiming a
                // change this function never made.
                other => {
                    debug_assert!(false, "unmapped hot-reloadable field {other}");
                    tracing::error!(
                        target: "risk",
                        field = other,
                        "hot-reloadable field has no write arm; the audit record would be a lie"
                    );
                }
            }
        }
        if band_moved {
            // The engine holds the band the tickets are cut from; the CoreConfig
            // copy above is what a reader with no engine sees.
            if let Some(engine) = self.engine.as_mut() {
                engine.set_share_band(self.config.min_shares, self.config.max_shares);
            }
        }

        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                reason = patch.reason.as_deref().unwrap_or(""),
                "risk limit hot-update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                RISK_LIMIT_UPDATE_NOTE,
            );
        }
        Ok(RiskLimitUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            persisted: false,
            note: RISK_LIMIT_UPDATE_NOTE,
        })
    }

    /// 生效风控可编辑（`risk.setSystemic`）：九项系统性限额的运行时写路径，
    /// 仿 [`Self::apply_risk_limits`] 的 plan-first + 审计姿态。
    ///
    /// 三个生效档位都是真实的，不是文档话术：
    ///
    /// - **live（六项）**—— `max_single_loss_usd` / `max_position_size` /
    ///   `max_total_position` / `max_total_exposure_usd` / `max_correlation_usd`
    ///   / `global_kill_switch_loss_usd`。入场流水线每笔现场
    ///   `systemic_snapshot()` 判读，只约束新敞口（平仓从不走这条路），不携带
    ///   计数器 —— 写进内存后下一笔入场立即按新值裁决。
    /// - **next_session（两项）**—— `max_consecutive_losses` /
    ///   `cooldown_minutes`。连亏计数器已在跑（`record_with` 的 breaker 带着
    ///   出生时的阈值对过一辈子），热改会追溯改判在案连亏 ——
    ///   `HOT_RELOAD_REFUSED` 的理由对这两项仍然成立。读数照写（操作员的
    ///   意图进入 matrix，重启后 `from_file` 的解析与它一致），但本会话的
    ///   熔断行为不变，回执里逐项注明。
    /// - **next_session（一项）**—— `max_daily_drawdown_usd`。它与日亏熔断
    ///   是同一个预算；当日已发生的已实现亏损账本不能被中途改动的帽追溯
    ///   改判。同上：matrix 照写、本会话的 `PositionConfig::max_daily_loss_usd`
    ///   不动。
    ///
    /// Atomic：patch 先全量校验（非负）再落笔，被拒的 patch 不留半点痕迹。
    /// 不持久化 —— 重启后 `from_file` 重走启动层，与 `risk.setLimits` 的
    /// 「热≠永久」同一句真话。
    pub fn apply_systemic_limits(
        &mut self,
        patch: &crate::ipc::schema::SetSystemicLimitsParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::SystemicLimitUpdate> {
        use crate::ipc::schema::{
            SYSTEMIC_LIMIT_UPDATE_NOTE, SystemicLimitChange, SystemicLimitUpdate,
        };

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setSystemic needs at least one of the nine systemic fields; \
                 read the current values with risk.limits"
                    .to_string(),
            ));
        }

        // Plan first, write later —— 被拒的 patch 一个字节都不动。
        let matrix = self.risk.config().systemic.clone();
        let mut changes: Vec<SystemicLimitChange> = Vec::new();
        let mut plan = |field: &'static str,
                        from: &crate::risk::limits::Bound,
                        to: Option<Decimal>,
                        effect: &'static str|
         -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to < Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("{field} must be >= 0, got {to}; nothing was applied"),
                ));
            }
            changes.push(SystemicLimitChange {
                field,
                from: from.value().to_string(),
                to: to.to_string(),
                effect,
            });
            Ok(())
        };

        plan(
            "maxSingleLossUsd",
            &matrix.account.max_single_loss_usd,
            patch.max_single_loss_usd,
            "live",
        )?;
        plan(
            "maxDailyDrawdownUsd",
            &matrix.account.max_daily_drawdown_usd,
            patch.max_daily_drawdown_usd,
            "next_session",
        )?;
        plan(
            "maxPositionSize",
            &matrix.account.max_position_size,
            patch.max_position_size,
            "live",
        )?;
        plan(
            "maxConsecutiveLosses",
            &matrix.account.max_consecutive_losses,
            patch.max_consecutive_losses,
            "next_session",
        )?;
        plan(
            "cooldownMinutes",
            &matrix.account.cooldown_minutes,
            patch.cooldown_minutes,
            "next_session",
        )?;
        plan(
            "maxTotalPosition",
            &matrix.global.max_total_position,
            patch.max_total_position,
            "live",
        )?;
        plan(
            "maxTotalExposureUsd",
            &matrix.global.max_total_exposure_usd,
            patch.max_total_exposure_usd,
            "live",
        )?;
        plan(
            "maxCorrelationUsd",
            &matrix.global.max_correlation_usd,
            patch.max_correlation_usd,
            "live",
        )?;
        plan(
            "globalKillSwitchLossUsd",
            &matrix.global.global_kill_switch_loss_usd,
            patch.global_kill_switch_loss_usd,
            "live",
        )?;

        if changes.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setSystemic: no field carried a change — nothing to audit",
            ));
        }

        // Write phase: the six live bounds land in the matrix the pipeline
        // snapshots per entry; the three next-session rows also land there
        // (the readout must show the operator's intent and the restart
        // resolution agrees), but nothing re-judges this session — the
        // breaker keeps its birth pair, the day's loss book stays closed.
        {
            let cfg = self.risk.config_mut();
            let matrix = &mut cfg.systemic;
            let set = |bound: &mut crate::risk::limits::Bound,
                       to: Option<Decimal>,
                       source: crate::risk::limits::LimitSource| {
                if let Some(v) = to {
                    *bound = crate::risk::limits::Bound::new(v, source);
                }
            };
            let flag = crate::risk::limits::LimitSource::Flag;
            set(
                &mut matrix.account.max_single_loss_usd,
                patch.max_single_loss_usd,
                flag,
            );
            set(
                &mut matrix.account.max_daily_drawdown_usd,
                patch.max_daily_drawdown_usd,
                flag,
            );
            set(
                &mut matrix.account.max_position_size,
                patch.max_position_size,
                flag,
            );
            set(
                &mut matrix.account.max_consecutive_losses,
                patch.max_consecutive_losses,
                flag,
            );
            set(
                &mut matrix.account.cooldown_minutes,
                patch.cooldown_minutes,
                flag,
            );
            set(
                &mut matrix.global.max_total_position,
                patch.max_total_position,
                flag,
            );
            set(
                &mut matrix.global.max_total_exposure_usd,
                patch.max_total_exposure_usd,
                flag,
            );
            set(
                &mut matrix.global.max_correlation_usd,
                patch.max_correlation_usd,
                flag,
            );
            set(
                &mut matrix.global.global_kill_switch_loss_usd,
                patch.global_kill_switch_loss_usd,
                flag,
            );
        }

        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                effect = c.effect,
                reason = patch.reason.as_deref().unwrap_or(""),
                "systemic limit update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                SYSTEMIC_LIMIT_UPDATE_NOTE,
            );
        }
        Ok(SystemicLimitUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            persisted: false,
            note: SYSTEMIC_LIMIT_UPDATE_NOTE,
        })
    }

    /// 退出纪律可编辑（`risk.setExit`）：Gate 4 绑定的止损/止盈/强平三项的
    /// 运行时写路径，仿 [`Self::apply_systemic_limits`] 的 plan-first + 审计
    /// 姿态。
    ///
    /// 生效语义是真实的，不是文档话术：
    ///
    /// - **每笔新入场 live** —— 仲裁管线（Gate 4）每个意图现场从
    ///   `self.config.positions.exit` 取 [`ExitConfig`]（`process_intent` 的
    ///   调用点），写入后下一笔入场立即按新值绑定退出纪律投影。
    /// - **已开仓位的退出扫描同样 live** —— [`PositionManager`] 的退出梯
    ///   （止损/止盈/强平判读）读自己的 `config.exit` 副本，所以写路径必须
    ///   同步两处：`config.positions.exit`（入场绑定）+ `positions.set_config`
    ///   （退出扫描）。漏掉后者会造出「新仓位一种纪律、旧仓位另一种纪律」
    ///   的静默分裂。
    ///
    /// 校准护栏：`forceExitSec: 0` 是文档化的「关闭强平」开关（出厂校准值），
    /// 照常接受；两个百分比拒绝 `<= 0`（`stopLossPct: 0` 会在入场 tick 就
    /// 止损，`takeProfitPct: 0` 语义同样荒谬 —— 负数/零不是操作员想要的
    /// 「关闭」，是笔误）。强平接受 `0`、拒绝负数。
    ///
    /// 不持久化 —— 重启后回到启动 flag/env/TOML 的解析结果，与
    /// `risk.setSystemic` 的「热≠永久」同一句真话。
    pub fn apply_exit_set(
        &mut self,
        patch: &crate::ipc::schema::SetExitParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::ExitSetUpdate> {
        use crate::ipc::schema::{EXIT_SET_UPDATE_NOTE, ExitSetChange, ExitSetUpdate};

        if patch.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setExit needs at least one of stopLossPct / takeProfitPct / \
                 forceExitSec / trailingEnabled; read the current values with risk.limits"
                    .to_string(),
            ));
        }

        // Plan first, write later —— 被拒的 patch 一个字节都不动。
        let current = self.config.positions.exit.clone();
        let mut changes: Vec<ExitSetChange> = Vec::new();
        let mut plan = |field: &'static str,
                        from: Decimal,
                        to: Option<Decimal>,
                        effect: &'static str|
         -> CoreResult<()> {
            let Some(to) = to else { return Ok(()) };
            if to <= Decimal::ZERO {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "{field} must be > 0 (a percentage; there is no `off` \
                             spelling here), got {to}; nothing was applied"
                    ),
                ));
            }
            changes.push(ExitSetChange {
                field,
                from: from.to_string(),
                to: to.to_string(),
                effect,
            });
            Ok(())
        };

        plan(
            "stopLossPct",
            current.stop_loss_pct,
            patch.stop_loss_pct,
            "live",
        )?;
        plan(
            "takeProfitPct",
            current.take_profit_pct,
            patch.take_profit_pct,
            "live",
        )?;

        if let Some(sec) = patch.force_exit_sec {
            if sec < 0 {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!(
                        "forceExitSec must be >= 0 (0 = the guillotine is OFF, \
                             the calibrated default), got {sec}; nothing was applied"
                    ),
                ));
            }
            changes.push(ExitSetChange {
                field: "forceExitSec",
                from: current.force_exit_sec.to_string(),
                to: sec.to_string(),
                effect: "live",
            });
        }

        // A bool has no illegal value to refuse — the patch either names it or
        // it doesn't. The from/to audit strings are `true`/`false` verbatim,
        // same convention as the decimal fields' `to_string`.
        if let Some(on) = patch.trailing_enabled {
            changes.push(ExitSetChange {
                field: "trailingEnabled",
                from: current.trailing_enabled.to_string(),
                to: on.to_string(),
                effect: "live",
            });
        }

        if changes.is_empty() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "risk.setExit: no field carried a change — nothing to audit",
            ));
        }

        // Write phase: BOTH copies. `config.positions.exit` is what Gate 4
        // binds per new intent; `PositionManager.config.exit` is what the exit
        // sweep (stop/take/force judgement, the dynamic-stop ramp) reads every
        // tick. set_config keeps everything else (budgets, cooldowns) as-is —
        // only the exit triple's two fields move.
        {
            let mut exit = self.config.positions.exit.clone();
            if let Some(v) = patch.stop_loss_pct {
                exit.stop_loss_pct = v;
            }
            if let Some(v) = patch.take_profit_pct {
                exit.take_profit_pct = v;
            }
            if let Some(sec) = patch.force_exit_sec {
                exit.force_exit_sec = sec;
            }
            if let Some(on) = patch.trailing_enabled {
                exit.trailing_enabled = on;
            }
            self.config.positions.exit = exit.clone();
            let mut positions = self.config.positions.clone();
            positions.exit = exit;
            self.positions.set_config(positions);
        }

        for c in &changes {
            tracing::info!(
                target: "risk",
                actor = %actor,
                field = c.field,
                from = %c.from,
                to = %c.to,
                effect = c.effect,
                reason = patch.reason.as_deref().unwrap_or(""),
                "exit discipline update: {} {} -> {} ({})",
                c.field,
                c.from,
                c.to,
                EXIT_SET_UPDATE_NOTE,
            );
        }
        // Issue #464, the ladder/stopLossPct semantic note: the two are
        // INDEPENDENT mechanisms — `stop_loss_pct` is a percentage of the
        // entry price re-judged every tick, a ladder's `move_stop_to` an
        // absolute price floor recorded per step — so neither overwrite the
        // other and there is no state to reconcile. The one interaction worth
        // saying out loud: a changed `stop_loss_pct` re-bases the policy side
        // of the ladder's `max(policy stop, ladder floor)` on the NEXT exit
        // tick (already-open positions included), while floors already raised
        // by fired steps stay exactly where they are.
        if patch.stop_loss_pct.is_some() && !self.config.positions.exit_ladder.is_empty() {
            tracing::info!(
                target: "risk",
                actor = %actor,
                "exit ladder is active: the changed stop_loss_pct re-bases the \
                 ladder's stop floor on the next exit tick (fired move_stop_to \
                 floors are absolute and unaffected)"
            );
        }
        Ok(ExitSetUpdate {
            applied: changes,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            persisted: false,
            note: EXIT_SET_UPDATE_NOTE,
        })
    }

    /// 轮次时间周期的运行时写路径（`round_duration.set`，UI 设置卡）：
    /// **next_session** —— 白名单校验通过后只写持久格（生产
    /// `data/runtime/config.json`），本会话的网格一个字节都不动。
    ///
    /// 为什么不是热旋钮：轮次周期是网格坐标系 —— `round_slot` 是用会话的
    /// duration 推导的 UTC 网格索引，仓位恢复（`OpenParams.expires_at_ms`）、
    /// #457 过期扫描都用**当前** config 重算存量订单/仓位的过期时间；热切
    /// duration 会把旧 slot 全部错解（15m 轮的 slot 3 在 5m 网格里是另一个
    /// 时间窗）。重启后启动层读回持久格，新网格从干净状态开始。
    ///
    /// `reset: true` 清格 —— 下次会话回落启动解析（显式 flag > env >
    /// TOML > 默认）。`durationSec` 与 `reset` 恰好给一个，都给或都不给
    /// 拒绝。`runtime_config_path: None`（回测器/进程内测试）拒绝写并如实
    /// 回报 —— 测试不往调用者恰好在的 checkout 里落文件。
    pub fn apply_round_duration_set(
        &mut self,
        patch: &crate::ipc::schema::SetRoundDurationParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::RoundDurationUpdate> {
        use crate::ipc::schema::{ROUND_DURATION_SET_NOTE, RoundDurationUpdate};

        if patch.reset && patch.duration_sec.is_some() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "round_duration.set: give durationSec or reset, not both".to_string(),
            ));
        }
        let sec = patch.duration_sec;
        if !patch.reset && sec.is_none() {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "round_duration.set needs durationSec (one of {:?}) or reset: true",
                    self.config.allowed_round_secs
                ),
            ));
        }

        let persist = |value: Option<i64>| -> CoreResult<()> {
            let Some(path) = self.config.runtime_config_path.as_deref() else {
                return Err(CoreError::new(
                    CoreErrorCode::InvalidParams,
                    "round_duration.set: no runtime config path configured — \
                     persistence is disabled for this core (backtests and \
                     in-process tests); nothing was applied"
                        .to_string(),
                ));
            };
            let env_path = self
                .config
                .env_file_path
                .as_deref()
                .map(std::path::Path::new);
            crate::ipc::runtime_config::persist_round_duration_to_with_env(
                std::path::Path::new(path),
                value,
                actor,
                now_ms,
                env_path,
            )
            .map_err(|e| {
                CoreError::new(
                    CoreErrorCode::InvalidParams,
                    format!("round_duration.set: persist failed, nothing changed: {e}"),
                )
            })
        };

        // reset 路径：清格，下次会话回落启动解析。
        if patch.reset {
            persist(None)?;
            tracing::info!(
                target: "risk",
                actor = %actor,
                reason = patch.reason.as_deref().unwrap_or(""),
                "round duration cell cleared — next session re-resolves from \
                 the boot flags/env/TOML ({ROUND_DURATION_SET_NOTE})"
            );
            return Ok(RoundDurationUpdate {
                next_session_sec: self.config.round_duration_sec,
                reset: true,
                at_ms: now_ms,
                actor: actor.to_string(),
                reason: patch.reason.clone(),
                note: ROUND_DURATION_SET_NOTE,
            });
        }

        // 写路径：声明并集校验（一格越界值就是一台没人能解读的网格），然后
        // 原子写持久格。并集来自插件申报（`supported_round_secs`），不是内核
        // 知识；空并集 = 没有插件申报任何档 = 全拒（fail-closed）。
        let Some(sec) = sec else {
            unreachable!("the reset path returned above; a write path has a durationSec");
        };
        if !self.config.allowed_round_secs.contains(&sec) {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                format!(
                    "round_duration.set: {sec} is not declared by any market plugin \
                     (declared union: {:?}) — nothing was applied",
                    self.config.allowed_round_secs
                ),
            ));
        }
        persist(Some(sec))?;
        tracing::info!(
            target: "risk",
            actor = %actor,
            from = self.config.round_duration_sec,
            to = sec,
            current_session_unchanged = true,
            reason = patch.reason.as_deref().unwrap_or(""),
            "round duration set to {sec}s — next session ({ROUND_DURATION_SET_NOTE})"
        );
        Ok(RoundDurationUpdate {
            next_session_sec: sec,
            reset: false,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: patch.reason.clone(),
            note: ROUND_DURATION_SET_NOTE,
        })
    }

    /// #458: 交易环境重置（`system.reset_trading`，UI 设置卡 + 确认词）。
    /// 把本核拥有的交易状态整表清零并重新 seed —— 停用全部已启用策略、撤销
    /// 全部 resting 挂单、清空订单/成交/持仓记录（内存 + 磁盘）、按
    /// `dry_seed_balance` 复原每个账户现金、重置连亏熔断。
    ///
    /// 三道门：Live 拒绝（真实资金记录是 append-only 证据，不是 UI 可擦的
    /// 演示盘）；`confirm` 必须逐字等于 `RESET`（自由文本确认门，不是勾选框
    /// —— 误触必须不可能被归咎于双击）；`manual_ops_audit_path: None`
    /// （回测器/进程内测试）拒绝 —— manual-ops 审计行是重置留下的唯一痕迹，
    /// 无处审计的擦除必须失败而不是静默发生。
    ///
    /// 保留（显式不动）：配置、策略文件、全部既有审计流、
    /// strategy-state.json（停用策略会走 `set_strategy_enabled` 的既有持久化
    /// 路径重写 enabled 集 —— 这是行为语义的一部分，不是泄漏）。
    pub fn reset_trading(
        &mut self,
        params: &crate::ipc::schema::ResetTradingParams,
        actor: &str,
        now_ms: i64,
    ) -> CoreResult<crate::ipc::schema::ResetTradingResult> {
        use crate::ipc::runtime_config::append_manual_op;
        use crate::ipc::schema::{RESET_TRADING_LIVE_REFUSAL, ResetPreSummary, ResetTradingResult};

        if self.config.mode == Mode::Live {
            return Err(CoreError::new(
                CoreErrorCode::RiskRejected,
                RESET_TRADING_LIVE_REFUSAL.to_string(),
            ));
        }
        if params.confirm != "RESET" {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "system.reset_trading: confirm must be exactly \"RESET\" — \
                 this wipes every trading record this core owns"
                    .to_string(),
            ));
        }
        // issue 561 账户维隔离：params.account_id 在派发层已填缺省
        // （session → display → 进程 default 链，RAW params 判定显式 id），
        // 这里照字面执行。显式拒绝未知账户 —— 一个静默的空清洗比拒绝更糟。
        let account_id = match params.account_id.as_deref() {
            Some(id) => {
                let id = crate::account::AccountId::from(id);
                if self.accounts.account(&id).is_none() {
                    return Err(CoreError::new(
                        CoreErrorCode::InvalidParams,
                        format!(
                            "system.reset_trading: unknown account \"{id}\" — \
                             the reset scope must name a configured account"
                        ),
                    ));
                }
                id
            }
            // 兜底（直连内核、不经 IPC 派发层的调用方）：进程 display 格。
            None => crate::account::AccountId::from(self.accounts.display_id()),
        };
        let audit_path = self
            .config
            .manual_ops_audit_path
            .clone()
            .map(std::path::PathBuf::from);
        let Some(audit_path) = audit_path.as_deref() else {
            return Err(CoreError::new(
                CoreErrorCode::InvalidParams,
                "system.reset_trading: no manual-ops audit path configured — \
                 a wipe with no audit trail is refused (backtests and \
                 in-process tests carry None)"
                    .to_string(),
            ));
        };
        let audit_path = std::path::Path::new(audit_path);

        // 重置前快照：行数 + 余额 + 活跃集合。这是审计行与回执里唯一存活的
        // 「删了什么」的证据 —— 记录本身不会留下。issue 561: 全部行数按
        // 账户过滤 —— 全局数会谎报清洗范围（重置 B 不删 A 的行）。
        let pre = ResetPreSummary {
            order_log_lines: self
                .order_db
                .as_ref()
                .map(|db| {
                    db.load()
                        .iter()
                        .filter(|o| o.account_id.as_str() == account_id.as_str())
                        .count() as u64
                })
                .unwrap_or(0),
            trade_log_lines: self
                .trade_db
                .as_ref()
                .map(|db| {
                    crate::trade_db::read_recent(db.jsonl_path(), 0)
                        .iter()
                        .filter(|r| {
                            let rid = r
                                .get("accountId")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default();
                            if rid.is_empty() {
                                account_id.as_str() == crate::account::default_account_id().as_str()
                            } else {
                                rid == account_id.as_str()
                            }
                        })
                        .count() as u64
                })
                .unwrap_or(0),
            position_log_lines: self
                .position_db
                .as_ref()
                .map(|db| {
                    db.load()
                        .iter()
                        .filter(|p| p.account_id.as_str() == account_id.as_str())
                        .count() as u64
                })
                .unwrap_or(0),
            open_positions: self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.account_id.as_str() == account_id.as_str())
                .count(),
            live_orders: self
                .ome
                .live_orders()
                .iter()
                .filter(|o| o.account_id.as_str() == account_id.as_str())
                .count(),
            balance: self
                .accounts
                .ledger_of(&account_id)
                .map(|l| l.balance())
                .unwrap_or(Decimal::ZERO),
        };

        // 1) 停用目标账户 scopes 里的已启用策略（复用 strategy.enable 的
        //    停用路径）。账户维裁决：只动该账户的 scopes —— 全局
        //    `enabled_strategy_names` 会连其他账户的同名 scope 一起停。
        //    accounts.toml 的启用集（持久真相）不写回：重置是运行时清洗
        //    （会话级 OFF），不是授权变更，重启后启用集原样生效。
        let account_scopes: Vec<String> = self
            .engine
            .as_ref()
            .map(|e| {
                e.strategy_scopes_all()
                    .into_iter()
                    .filter(|(a, _)| a == account_id.as_str())
                    .map(|(_, n)| n)
                    .collect()
            })
            .unwrap_or_default();
        let disabled: Vec<String> = {
            let mut names: Vec<String> = Vec::new();
            if let Some(e) = self.engine.as_ref() {
                for name in &account_scopes {
                    if e.enabled_strategies().iter().any(|n| n == name) {
                        names.push(name.clone());
                    }
                }
            }
            names.sort();
            names.dedup();
            names
        };
        for name in &disabled {
            self.set_strategy_enabled(name, false);
        }

        // 2) 撤销目标账户的 resting 挂单。逐单走 `cancel` 释放 BUY 预留
        //    （在订单自己的账簿里，E28）、清 engine 的 pending-token 闩、记
        //    fast_orders_dirty；随后 remove_account_orders 负责「连记录都
        //    消失」的部分。账户维裁决：token=None 改为账户过滤，其他账户
        //    的挂单原样保留。
        let cancelled = {
            let ids: Vec<String> = self
                .ome
                .live_orders()
                .into_iter()
                .filter(|o| o.account_id.as_str() == account_id.as_str())
                .map(|o| o.order_id.clone())
                .collect();
            let mut n = 0usize;
            for id in &ids {
                if let Err(e) = self.cancel(id, now_ms) {
                    tracing::warn!(error = %e, order = %id, "reset_trading: cancel failed mid-reset");
                } else {
                    n += 1;
                }
            }
            n
        };

        // 3) 内存态按账户维清洗（issue 561 清单 A 项）+ 全局维照旧（清单
        //    B 项）。OME 不再整替：整替会把其他账户的订单/幂等表一起抹掉，
        //    只拿掉目标账户的订单行。副作用（tx 事件）不重放：重置不是一
        //    个需要逐单通知的操作。
        let live_before = pre.live_orders;
        self.ome.remove_account_orders(account_id.as_str());
        self.inflight_takers.clear();
        self.fast_dirty_tokens.clear();
        self.fast_eval_wake = false;
        self.fast_orders_dirty = true;
        self.fast_positions_dirty = true;
        // exit 路由三态：exit_reasons 的键是 token——只清仍归属目标账户的
        // token（其他账户可能持有同一 token 的独立行，账户↔venue 解耦后
        // 双账户可同持一 token）。strategy_exits 按意图自带 account_id 过滤。
        let account_tokens: std::collections::HashSet<String> = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id.as_str() == account_id.as_str())
            .map(|p| p.token_id.clone())
            .chain(
                self.ome
                    .all()
                    .iter()
                    .filter(|o| o.account_id.as_str() == account_id.as_str())
                    .map(|o| o.token_id.clone()),
            )
            .collect();
        // 摘除该账户持仓/订单前先记下它的 token 集（上面），此刻 positions
        // 还未动 —— remove_account_positions 在第 4 步执行，token 集先取。
        let doomed_tokens: Vec<String> = {
            let before = self
                .positions
                .open_positions()
                .iter()
                .filter(|p| p.account_id.as_str() == account_id.as_str())
                .map(|p| p.token_id.clone())
                .collect::<std::collections::HashSet<_>>();
            account_tokens.union(&before).cloned().collect()
        };
        for t in &doomed_tokens {
            if let Some(reason) = self.exit_reasons.remove(t) {
                self.exit_reasons_evicted += 0; // 计数语义未变：清的是重置面
                let _ = reason;
            }
            self.exit_reason_order.retain(|k| k != t);
        }
        self.strategy_exits
            .retain(|x| x.account_id.as_str() != account_id.as_str());
        self.place_cooldowns.clear();
        self.escalation_skips.clear();
        self.pending_venue_cancels.clear();
        self.books.clear();
        self.consecutive_venue_rejects = 0;
        self.consecutive_venue_rejects_by_account
            .remove(account_id.as_str());
        self.account_freezes.remove(account_id.as_str());
        self.auto_frozen.remove(account_id.as_str());
        self.consecutive_sweep_failures = 0;
        self.last_error = None;
        // #537: the strategy page's PnL counters are the OTHER survivor —
        // trade_db and the cash ledger reset, but these per-(strategy,account)
        // slices kept their closed_trades/wins/net_pnl_usd, so the panel's
        // 「真实余额 = 本金 + 净利」 kept folding a history the reset had
        // erased. A wiped environment reports zero for every counter.
        // issue 561: 只摘目标账户的切片（键尾 `\u{1}account`），其他账户
        // 的计数原样保留。
        let acct_suffix = format!("\u{1}{}", account_id.as_str());
        self.strategy_accounting
            .retain(|k, _| !k.ends_with(&acct_suffix));
        if let Some(l) = self.applied_log.as_ref() {
            l.compact(&[]);
        }
        self.breaker = crate::risk::LossBreakers::new(
            self.config.max_consecutive_losses,
            self.config.breaker_cooldown_sec,
        );
        self.risk.resume();

        // 4) 持仓簿账户维摘除 + 磁盘记录按账户截断（不重启进程）。
        let open_before = pre.open_positions;
        let (removed_open, _removed_closed) =
            self.positions.remove_account_positions(account_id.as_str());
        let _ = removed_open;
        if let Some(db) = &self.position_db {
            // 保留其他账户的行原样落盘 —— 整簿 save(&[]) 会把 A 的持仓也
            // 抹掉（账户维裁决）。
            let remaining: Vec<crate::position::OpenPosition> =
                self.positions.open_positions().to_vec();
            db.save(&remaining);
        }
        if let Some(db) = &self.order_db {
            db.compact_account(account_id.as_str());
        }
        if let Some(db) = &mut self.trade_db {
            db.retract_account(account_id.as_str(), now_ms);
        }
        // #178/#175 的幂等簿与结算簿：重置后历史结算不再需要去重（记录文件
        // 已空，结算簿对着空文件重放会凭空恢复 claims）。全局维（清单 B）：
        // 结算簿键无账户归属，保持整簿重置 —— 账户维清算绝不产生跨账户
        // claims 复活。
        self.settlement = crate::settlement::SettlementBook::new(None);
        self.recon_watermark_ms = 0;

        // 记账审计与交易记录同生共死：trade_db 清零后，带着旧 realized 的锚点
        // 会让 trade_ledger_identity 永久假红（锚点不动但账目已归零），
        // halt_entries 把之后每一笔意图都拦在门外。重置即回到 boot 同款
        // AuditRuntime（保留 audit_log_path）——下一次审计在干净账目上重新
        // 取锚，与「重置=回到全新环境」的语义一致。
        let audit_log_path = self.audit.audit_log_path.clone();
        self.audit = AuditRuntime {
            audit_log_path,
            ..Default::default()
        };

        // 5) 重新 seed 目标账户到配置值（issue 561 账户维原语）。语义与
        //    启动 seed 一致（余额回到种子值），journal 重写为单行 `Reset`
        //    行 —— boot 回放后 balance == 种子（与自检断言一致，裁决 3 修
        //    复），其他账户的 journal 一字不动。
        let journal_rewrote = self
            .accounts
            .reset_account_to_seed(&account_id, self.config.dry_seed_balance)?;
        if !journal_rewrote {
            tracing::warn!(
                account = %account_id.as_str(),
                "issue 561: account journal rewrite failed at reset — the \
                 next boot may replay pre-reset rows"
            );
        }
        // issue 723: the reset rewrote this account's journal into a single
        // `Reset` row — a NEW evidence generation with its own row-seq space.
        // The bound venues' cursors die here: the pre-reset numbers must
        // never judge post-reset facts.
        for v in self
            .venue_fact_cursor
            .keys()
            .filter(|v| self.venue_cash_account(v.as_str()).as_ref() == Some(&account_id))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.venue_fact_cursor.remove(&v);
        }

        // 6) 自校验：目标账户 live 索引空 / 该账户 open 持仓空 / 该账户在
        //    三个记录文件中的行数归零 / 余额 == seed。其他账户的行不在断言
        //    面 —— 它们本就不该被清洗。
        let mut problems: Vec<String> = Vec::new();
        let leftover_live = self
            .ome
            .live_orders()
            .iter()
            .filter(|o| o.account_id.as_str() == account_id.as_str())
            .count();
        if leftover_live != 0 {
            problems.push(format!(
                "account {account_id} live index not empty after reset ({leftover_live} orders)"
            ));
        }
        let leftover_open = self
            .positions
            .open_positions()
            .iter()
            .filter(|p| p.account_id.as_str() == account_id.as_str())
            .count();
        if leftover_open != 0 {
            problems.push(format!(
                "account {account_id} open positions not empty after reset ({leftover_open} remain)"
            ));
        }
        if let Some(db) = &self.order_db {
            let n = db
                .load()
                .iter()
                .filter(|o| o.account_id.as_str() == account_id.as_str())
                .count();
            if n != 0 {
                problems.push(format!(
                    "account {account_id} order log still has {n} lines after reset"
                ));
            }
        }
        if let Some(db) = &self.trade_db {
            let n = crate::trade_db::read_recent(db.jsonl_path(), 0)
                .iter()
                .filter(|r| {
                    let rid = r
                        .get("accountId")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if rid.is_empty() {
                        account_id.as_str() == crate::account::default_account_id().as_str()
                    } else {
                        rid == account_id.as_str()
                    }
                })
                .count();
            if n != 0 {
                problems.push(format!(
                    "account {account_id} trade log still has {n} lines after reset"
                ));
            }
        }
        if let Some(db) = &self.position_db {
            let n = db
                .load()
                .iter()
                .filter(|p| p.account_id.as_str() == account_id.as_str())
                .count();
            if n != 0 {
                problems.push(format!(
                    "account {account_id} position log still has {n} lines after reset"
                ));
            }
        }
        let balances: Vec<(String, Decimal)> = vec![(
            account_id.as_str().to_string(),
            self.accounts
                .ledger_of(&account_id)
                .map(|l| l.balance())
                .unwrap_or(Decimal::ZERO),
        )];
        for (id, bal) in &balances {
            if *bal != self.config.dry_seed_balance {
                problems.push(format!(
                    "account {id} balance {bal} != seed {} after reset",
                    self.config.dry_seed_balance
                ));
            }
        }
        if !problems.is_empty() {
            let msg = format!(
                "system.reset_trading: post-reset self-check FAILED — {}",
                problems.join("; ")
            );
            // 自检失败也要留审计行（审计写失败再叠加错误）。
            let _ = append_manual_op(
                audit_path,
                &serde_json::json!({
                    "op": "reset_trading",
                    "phase": "self_check_failed",
                    "atMs": now_ms,
                    "actor": actor,
                    "account": account_id.as_str(),
                    "reason": params.reason,
                    "problems": problems,
                }),
            );
            return Err(CoreError::new(CoreErrorCode::Internal, msg));
        }

        // 7) manual-ops 审计行：重置留下的唯一痕迹。写失败 = 整体失败
        //    （状态已清但审计缺席是「不可追溯的擦除」，必须显式暴露）。
        append_manual_op(
            audit_path,
            &serde_json::json!({
                "op": "reset_trading",
                "atMs": now_ms,
                "actor": actor,
                "account": account_id.as_str(),
                "reason": params.reason,
                "mode": format!("{:?}", self.config.mode).to_lowercase(),
                "pre": {
                    "orderLogLines": pre.order_log_lines,
                    "tradeLogLines": pre.trade_log_lines,
                    "positionLogLines": pre.position_log_lines,
                    "openPositions": pre.open_positions,
                    "liveOrders": pre.live_orders,
                    "balance": pre.balance.to_string(),
                },
                "post": {
                    "disabledStrategies": disabled,
                    "cancelledOrders": cancelled,
                    "balances": balances.iter()
                        .map(|(id, b)| serde_json::json!({
                            "accountId": id, "balance": b.to_string()
                        }))
                        .collect::<Vec<_>>(),
                },
            }),
        )
        .map_err(|e| {
            CoreError::new(
                CoreErrorCode::Internal,
                format!("system.reset_trading: manual-ops audit write failed: {e}"),
            )
        })?;

        tracing::info!(
            target: "risk",
            actor = %actor,
            account = %account_id.as_str(),
            live_orders = live_before,
            open_positions = open_before,
            cancelled = cancelled,
            disabled = disabled.len(),
            "trading environment reset (account-scoped) — the account's \
             records wiped, its cash re-seeded; other accounts untouched"
        );

        Ok(ResetTradingResult {
            disabled_strategies: disabled,
            cancelled_orders: cancelled,
            cleared_open_positions: open_before,
            cleared_closed_trades: pre.trade_log_lines as usize,
            account_id: account_id.as_str().to_string(),
            balances,
            pre_summary: pre,
            at_ms: now_ms,
            actor: actor.to_string(),
            reason: params.reason.clone(),
        })
    }

    /// The ONE writer of the panel-visible last-error slot (#180). Every
    /// internal refusal path funnels through here — directly, or via
    /// [`Core::emit_error`] — so the slot cannot be written with a message no
    /// code classifies, and cannot be skipped because one path forgot. It is
    /// also what makes the slot a *fresh* fact: each write carries the instant
    /// the failure was observed, so a reader can tell a new failure from an old
    /// banner (`tsMs` moves, `message` is the newest reason).
    pub(super) fn note_error(&mut self, err: &CoreError, now_ms: i64) {
        self.last_error = Some(LastError {
            ts_ms: now_ms,
            code: err.code,
            message: err.message.clone(),
        });
    }

    /// Broadcast a structured error to Node (never swallowed) AND record it in
    /// the one panel-visible error slot.
    pub fn emit_error(&mut self, e: CoreError) {
        self.emit_error_at(e, now_ms());
    }

    /// [`Core::emit_error`] with the caller's own instant, for error paths that
    /// carry the event time (a sweep at its `ts`) rather than the time of this
    /// call.
    pub fn emit_error_at(&mut self, e: CoreError, now_ms: i64) {
        self.note_error(&e, now_ms);
        self.emit_error_event(e);
    }

    /// E25 (#331): ONE audit line per arbitrated suggestion — never folded,
    /// never blocking (a failed append warns once and the trade goes on) —
    /// plus AT MOST one `INTENT_DECISION` push per `(strategy, gate, reason)`
    /// per second (§12.3): the PUSH folds, the audit does not.
    pub(super) fn record_intent_decision(
        &mut self,
        intent: &crate::arbitration::StrategyIntent,
        outcome: &crate::arbitration::Outcome,
        now_ms: i64,
    ) {
        let intent_json = serde_json::to_value(&intent.request).unwrap_or(serde_json::Value::Null);
        let record = self.intent_audit.record(
            now_ms,
            intent.request.account_id.as_str(),
            &intent.request.strategy,
            &intent_json,
            &outcome.decision,
            &outcome.gates,
            outcome.latency_us,
        );
        // The throttle key is (strategy, gate, reason); for a non-rejection the
        // "reason" slot carries the status, so an approval storm folds too.
        let (gate, key_reason) = match &outcome.decision {
            crate::arbitration::Decision::Rejected { gate, reason, .. } => (
                format!("{gate:?}").to_uppercase(),
                format!("{reason:?}").to_uppercase(),
            ),
            _ => (
                outcome
                    .gates
                    .last()
                    .map(|g| format!("{:?}", g.gate).to_uppercase())
                    .unwrap_or_else(|| "UNKNOWN".into()),
                outcome.decision.status().to_string(),
            ),
        };
        if let Some(count) =
            self.intent_audit
                .throttle(&intent.request.strategy, &gate, &key_reason, now_ms)
        {
            let push = crate::arbitration::AuditSink::push_for(&record, count);
            self.emit(Event::IntentDecision {
                account_id: record.account_id.clone(),
                strategy: push.strategy,
                intent_id: push.intent_id,
                status: push.status,
                gate: push.gate,
                detail: push.detail,
                ts_ms: push.ts_ms,
                count: push.count,
            });
        }
    }

    /// Emit an error event WITHOUT touching the slot, for a consequence whose
    /// cause is already recorded: the slot must keep showing the error an
    /// operator has to act on (the raw venue refusal), not the kernel's own
    /// reaction to it (the freeze). Everything else goes through
    /// [`Core::emit_error`].
    pub(super) fn emit_error_event(&self, e: CoreError) {
        self.emit(Event::Error { error: e });
    }

    /// Broadcast a risk alert.
    pub fn emit_risk_alert(&self, code: CoreErrorCode, message: String) {
        self.emit(Event::RiskAlert { code, message });
    }

    /// Roll the daily-loss budget when the UTC day changes (P0 #173).
    ///
    /// The budget's base is the account's CASH EQUITY — the ledger's
    /// venue-reconciled balance (host `seed_balance`/`venue_free_balance` keep
    /// it honest in live, `dry_seed_balance` in dry). Only the kernel can read
    /// it: the percentage cap is relative to the day's opening equity, so a
    /// fixed dollar default stops being meaningless on a small book. Called
    /// once per tick, before the exit checks, so a position closed right after
    /// the boundary is already counted in the new day.
    pub(super) fn roll_daily_budget(&mut self, now_ms: i64) {
        // `balance` is already gross of local reservations — adding `reserved`
        // here would double-count resting BUY commitments. The daily budget is
        // PER-ACCOUNT (operator verdict 2026-10-25, overriding the old §9.3
        // global reading): each account's day opens on ITS OWN ledger equity,
        // and one account's losses never burn a sibling's budget.
        let accounts: Vec<(crate::account::AccountId, Decimal)> = self
            .accounts
            .account_list()
            .iter()
            .filter(|a| a.status == crate::account::AccountStatus::Active)
            .map(|a| {
                (a.id.clone(), {
                    self.accounts
                        .ledger_of(&a.id)
                        .map(|l| l.balance())
                        .unwrap_or_default()
                })
            })
            .collect();
        for (id, equity) in accounts {
            if let Some(roll) =
                self.positions
                    .roll_daily(id.as_str(), now_ms, equity, self.equity_basis())
            {
                let message = match roll.previous_day_index {
                    Some(prev) => format!(
                        "daily loss budget: UTC day {prev} closed at ${} realized{} — day {} opens with equity ${}, cap ${}",
                        roll.previous_realized_pnl_usd,
                        if roll.previous_tripped {
                            " (TRIPPED: entries were frozen)"
                        } else {
                            ""
                        },
                        roll.day_index,
                        roll.opening_equity_usd,
                        roll.limit_usd
                    ),
                    None => format!(
                        "daily loss budget: UTC day {} opens with equity ${}, cap ${} (realized loss resets at the UTC day boundary)",
                        roll.day_index, roll.opening_equity_usd, roll.limit_usd
                    ),
                };
                if roll.previous_tripped {
                    tracing::warn!(day = roll.day_index, account = %id.as_str(), "{}", message);
                } else {
                    tracing::info!(day = roll.day_index, account = %id.as_str(), "{}", message);
                }
                self.emit_risk_alert(
                    CoreErrorCode::RiskRejected,
                    format!("[{}] {}", id.as_str(), message),
                );
            }
        }
    }

    /// Which book the equity handed to [`Self::roll_daily_budget`] came from
    /// (P1 #235): the day's percentage cap is a share of an account, and the
    /// ledger balance means a different thing in each of them.
    ///
    /// Derived from the SAME predicate `Core::new` uses to decide which book the
    /// ledger is seeded from — a locally-settling mode gets `dry_seed_balance`,
    /// live gets the venue's money — so the label cannot drift from the number
    /// it describes. A mode that settles locally is trading the simulation even
    /// when it is called read-only, and it must not be told its base came from
    /// the venue.
    pub(super) fn equity_basis(&self) -> EquityBasis {
        if self.config.mode.settles_locally() {
            EquityBasis::Dry
        } else {
            EquityBasis::Live
        }
    }

    /// Announce a tripped daily-loss breaker exactly once per process (see
    /// `DailyLossState::tripped_reported`). Consumed here AND at the losing
    /// close, so the freeze is visible at the moment it happens and again after
    /// a restart restored an already-breached budget.
    pub(super) fn emit_daily_trip(&mut self) {
        let trips = self.positions.take_daily_trips();
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

    /// Report every exit withheld from becoming an order (P0 #177, F6): the
    /// audit trail must show "should have triggered" instead of a silent hold.
    ///
    /// The log line names the cause instead of calling all of them a suppressed
    /// protective stop (#267): a held TRAILING stop is not a held protective
    /// stop, and a reviewer reading "protective stop suppressed" about a profit
    /// rule is being told the wrong story. The wording lives on the event
    /// (`SuppressedStopEvent::message`), so the panel and the log cannot drift.
    pub(super) fn emit_suppressed_stops(&mut self) {
        for ev in self.positions.drain_suppressed_stops() {
            let message = ev.message();
            // #268 item 2: past `N x max_book_age_sec` unpriceable, this is no
            // longer a routine hold — it is a position that cannot act, and it
            // is raised to error level so it survives a warn filter. Two call
            // sites rather than a dynamic level: tracing's level is a compile-
            // time property of the callsite.
            if ev.escalated {
                tracing::error!(
                    position = %ev.position_id,
                    token = %ev.token_id,
                    cause = ?ev.cause,
                    bid = %ev.bid,
                    mid = %ev.mid,
                    stop_pct = %ev.stop_pct,
                    book_age_ms = ?ev.book_age_ms,
                    unpriceable_for_ms = ev.unpriceable_for_ms,
                    "{}",
                    message
                );
            } else {
                tracing::warn!(
                    position = %ev.position_id,
                    token = %ev.token_id,
                    cause = ?ev.cause,
                    bid = %ev.bid,
                    mid = %ev.mid,
                    stop_pct = %ev.stop_pct,
                    book_age_ms = ?ev.book_age_ms,
                    unpriceable_for_ms = ev.unpriceable_for_ms,
                    "{}",
                    message
                );
            }
            self.emit_risk_alert(CoreErrorCode::RiskRejected, message);
        }
    }
}
