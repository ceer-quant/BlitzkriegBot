/**
 * Risk domain — E26 systemic risk readout (mirror ipc/schema.rs's
 * RiskLimitsResult), the exit-discipline triple, and their write paths.
 */
import { request } from './core.ts'

// ── E26 systemic risk readout (mirror ipc/schema.rs's RiskLimitsResult) ──────

/** WHERE a limit's effective value came from (§4.4) — flag/env/default (toml retired). */
export type LimitSource = 'default' | 'env' | 'flag'

/**
 * One limit: value AND provenance, together (§4.4) — "35" without "toml" is
 * not an answer an operator can act on. Decimals cross as STRINGS.
 */
export interface RiskBound {
  value: string
  source: LimitSource | string
}

/** The per-account matrix (§4.2, six). Risk control follows the account's
 *  capital — there is no process-wide (global) cap (issue #983). */
export interface AccountRiskLimitsView {
  maxSingleLossUsd: RiskBound
  maxDailyDrawdownUsd: RiskBound
  maxPositionSize: RiskBound
  maxConsecutiveLosses: RiskBound
  cooldownMinutes: RiskBound
  maxOrderNotionalUsd: RiskBound
}

/** The exit triple Gate 4 binds per entry (plain numbers, not Bounds). */
export interface RiskExitView {
  stopLossPct: number | string
  takeProfitPct: number | string
  forceExitSec: number
  /**
   * 移动止盈总开关（trailing stop 武装与否）。老内核缺省 → undefined：
   * 开关降级为不可用态，不造默认值。
   */
  trailingEnabled?: boolean
  /**
   * issue 935：持有到期策略的真实停损带宽（出厂 50）——面板通用 12% 不绑
   * holder，只显示 12% 会误读内核姿势。老内核缺省 → undefined，如实降级。
   */
  holderStopPct?: number | string
  /**
   * issue 935：策略出场 maker-first 总开关（出厂 false，字节保持）。
   * 老内核缺省 → undefined，开关降级为不可用态，不造默认值。
   */
  strategyExitMakerFirst?: boolean
}

/** The whole `risk.limits` readout. */
export interface RiskLimitsDoc {
  version: string
  account: { id: string; limits: AccountRiskLimitsView }
  exit: RiskExitView
  error?: string
}

/**
 * `risk.setSystemic` 请求体：六项任选（只发改动项），小数跨线是字符串（内核
 * serde 的 Decimal 约定）；`0` = 关闭该限额。`reason` 进审计行。
 * issue #983：全部为账户级限额，无全局限额。
 */
export interface RiskSetSystemicBody {
  maxSingleLossUsd?: string | number
  maxDailyDrawdownUsd?: string | number
  maxPositionSize?: string | number
  maxConsecutiveLosses?: string | number
  cooldownMinutes?: string | number
  maxOrderNotionalUsd?: string | number
  reason?: string
}

/** One bound's audited change（内核 SystemicLimitChange 的 wire 形状）。 */
export interface SystemicLimitChange {
  field: string
  from: string
  to: string
  /** live = 立即约束下一笔；next_session = 重启后生效，本会话不追溯改判。 */
  effect: 'live' | 'next_session' | string
}

/** `risk.setSystemic` 回执：改了什么、谁改的、何时、持久化到哪。persistence
 * = store（改动的限额键已由内核持久化进装配决定的 durable 载体——DB 开库时
 * 是 SQLite 的 account_risk 行，env-only 后端时是 .env 的 BK_RISK_* 键；
 * 重启后按同一载体重新武装）或 none（未接线 durable 载体或写入失败 —— 日志里
 * 有路径）；每项 applied 带 effect=live|next_session（两项 live：单笔亏损/单仓
 * 上限；三项 next_session：回撤预算/连亏熔断/冷静期）。回执必须写明 effect 与
 * persistence，禁止只显示「已保存」。 */
export interface SystemicLimitUpdateDoc {
  applied: SystemicLimitChange[]
  atMs: number
  actor: string
  reason?: string
  persisted: boolean
  persistence?: 'store' | 'none' | string
  note: string
  error?: string
}

/**
 * `risk.setExit` 请求体：退出纪律三项任选（只发改动项）。百分比跨线是
 * 字符串（内核 Decimal 约定）；`forceExitSec` 是秒数，0 = 关闭强平（出厂
 * 校准值）。`reason` 进审计行。
 */
export interface RiskSetExitBody {
  stopLossPct?: string | number
  takeProfitPct?: string | number
  forceExitSec?: number
  /** 移动止盈武装开关（出厂 false，issue 356 校准）；热配升格后进请求体。 */
  trailingEnabled?: boolean
  /** issue 935：策略出场 maker-first 总开关（出厂 false，字节保持）。 */
  strategyExitMakerFirst?: boolean
  reason?: string
}

/** One exit field's audited change（内核 ExitSetChange 的 wire 形状）。 */
export interface ExitSetChange {
  field: string
  from: string
  to: string
  /** live：新入场下一笔按新值绑定，已开仓位的退出扫描同步跟随。 */
  effect: 'live' | string
}

/** `risk.setExit` 回执：改了什么、谁改的、何时、持久化到哪。persistence
 * = store（改动的退出纪律键已由内核持久化进装配决定的 durable 载体——DB 开库
 * 时是 SQLite 的 account_exit_policy 行，env-only 后端时是 .env 的 BK_EXIT_*
 * 键；重启后按同一载体重新解析出同样的值）或 none；每项 applied 带
 * effect=live（新入场下一笔按新值绑定，已开仓位的退出扫描同步跟随）。回执必须
 * 写明 effect 与 persistence，禁止只显示「已保存」。 */
export interface ExitSetUpdateDoc {
  applied: ExitSetChange[]
  atMs: number
  actor: string
  reason?: string
  persisted: boolean
  persistence?: 'store' | 'none' | string
  note: string
  error?: string
}

export const riskApi = {
  /**
   * E26 (§4.4): the effective systemic limits and where each came from — a
   * thin proxy of the core's `risk.limits` (read-only; the core answers from
   * the LIVE risk config, so a riskSetSystemic edit shows up on the next read).
   */
  riskLimits: () => request<RiskLimitsDoc>('/risk-limits'),
  /**
   * 生效风控可编辑的写路径（`risk.setSystemic`）：六项账户级限额的热写
   * （issue #983：风控只跟随账户，无全局限额）。只发改动过的字段；内核
   * plan-first（一项非法则整体拒）+ 逐字段审计，回执里每个 bound 带
   * effect（三项 live / 三项 next_session）。persistence = store：改动的
   * 限额键由内核持久化进装配决定的 durable 载体（DB 或 env-only 后端），
   * 重启后重新武装。
   */
  riskSetSystemic: (body: RiskSetSystemicBody) =>
    request<SystemicLimitUpdateDoc>('/risk/set-systemic', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * 退出纪律的写路径（`risk.setExit`）：止损/止盈/强平三项的热写。
   * 只发改动过的字段；内核 plan-first（一项非法则整体拒）+ 逐字段审计，
   * 回执里每项带 old→new 与 effect=live（新入场下一笔按新值绑定，已开仓位
   * 的退出扫描同步跟随）。persistence = store：改动的 BK_EXIT_* 键由内核
   * 持久化进装配决定的 durable 载体（DB 或 env-only 后端），重启后按启动链
   * 重新解析出同样的值。
   */
  riskSetExit: (body: RiskSetExitBody) =>
    request<ExitSetUpdateDoc>('/risk/set-exit', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}
