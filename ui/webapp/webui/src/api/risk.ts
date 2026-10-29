/**
 * Risk domain — E26 systemic risk readout (mirror ipc/schema.rs's
 * RiskLimitsResult), the exit-discipline triple, and their write paths.
 */
import { request } from './core.ts'

// ── E26 systemic risk readout (mirror ipc/schema.rs's RiskLimitsResult) ──────

/** WHERE a limit's effective value came from (§4.4). */
export type LimitSource = 'default' | 'toml' | 'env' | 'flag'

/**
 * One limit: value AND provenance, together (§4.4) — "35" without "toml" is
 * not an answer an operator can act on. Decimals cross as STRINGS.
 */
export interface RiskBound {
  value: string
  source: LimitSource | string
}

/** The per-account matrix (§4.2, five). */
export interface AccountRiskLimitsView {
  maxSingleLossUsd: RiskBound
  maxDailyDrawdownUsd: RiskBound
  maxPositionSize: RiskBound
  maxConsecutiveLosses: RiskBound
  cooldownMinutes: RiskBound
}

/** The process-wide matrix (§4.2, four). */
export interface GlobalRiskLimitsView {
  maxTotalPosition: RiskBound
  maxTotalExposureUsd: RiskBound
  maxCorrelationUsd: RiskBound
  globalKillSwitchLossUsd: RiskBound
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
}

/** The whole `risk.limits` readout. */
export interface RiskLimitsDoc {
  version: string
  account: { id: string; limits: AccountRiskLimitsView }
  global: { limits: GlobalRiskLimitsView }
  exit: RiskExitView
  error?: string
}

/**
 * `risk.setSystemic` 请求体：九项任选（只发改动项），小数跨线是字符串（内核
 * serde 的 Decimal 约定）；`0` = 关闭该限额。`reason` 进审计行。
 */
export interface RiskSetSystemicBody {
  maxSingleLossUsd?: string | number
  maxDailyDrawdownUsd?: string | number
  maxPositionSize?: string | number
  maxConsecutiveLosses?: string | number
  cooldownMinutes?: string | number
  maxTotalPosition?: string | number
  maxTotalExposureUsd?: string | number
  maxCorrelationUsd?: string | number
  globalKillSwitchLossUsd?: string | number
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

/** `risk.setSystemic` 回执：改了什么、谁改的、何时、未持久化。 */
export interface SystemicLimitUpdateDoc {
  applied: SystemicLimitChange[]
  atMs: number
  actor: string
  reason?: string
  persisted: boolean
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

/** `risk.setExit` 回执：改了什么、谁改的、何时、未持久化。 */
export interface ExitSetUpdateDoc {
  applied: ExitSetChange[]
  atMs: number
  actor: string
  reason?: string
  persisted: boolean
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
   * 生效风控可编辑的写路径（`risk.setSystemic`）：九项系统限额的内存热写。
   * 只发改动过的字段；内核 plan-first（一项非法则整体拒）+ 逐字段审计，回执
   * 里每个 bound 带效果等级（live / next_session）。未持久化：重启回到
   * 启动 flag/env/TOML 的解析结果。
   */
  riskSetSystemic: (body: RiskSetSystemicBody) =>
    request<SystemicLimitUpdateDoc>('/risk/set-systemic', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * 退出纪律的写路径（`risk.setExit`）：止损/止盈/强平三项的内存热写。
   * 只发改动过的字段；内核 plan-first（一项非法则整体拒）+ 逐字段审计，
   * 回执里每项带 old→new 与 effect=live（新入场下一笔按新值绑定，已开仓位
   * 的退出扫描同步跟随）。未持久化：重启回到启动 flag/env/TOML 的解析结果。
   */
  riskSetExit: (body: RiskSetExitBody) =>
    request<ExitSetUpdateDoc>('/risk/set-exit', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}
