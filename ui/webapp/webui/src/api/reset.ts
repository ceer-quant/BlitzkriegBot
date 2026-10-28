/**
 * Reset domain — the trading-environment reset card (mirror core schema
 * ResetTradingResult / ResetPreSummary).
 */
import { request } from './core.ts'

// ── 交易环境重置卡（mirror core schema ResetTradingResult / ResetPreSummary）──

/** 重置前的状态快照：行数 + 活跃集合 + 余额 —— 审计行里唯一存活的「删了什么」。 */
export interface ResetPreSummaryDoc {
  orderLogLines: number
  tradeLogLines: number
  positionLogLines: number
  openPositions: number
  liveOrders: number
  balance: number | string
}

/**
 * `system.reset_trading` 请求体：confirm 必须逐字 "RESET"（自由文本确认门）。
 * issue 561：accountId = 重置哪个账户（缺省 = 面板 display 账户，经内核
 * effective_default_account 链解析）；重置按账户隔离，其他账户分文不动。
 */
export interface ResetTradingBody {
  confirm: string
  reason?: string
  accountId?: string
}

/** `system.reset_trading` 回执：清理计数 + 自检后的余额 + 重置前快照。 */
export interface ResetTradingResultDoc {
  disabledStrategies: string[]
  cancelledOrders: number
  clearedOpenPositions: number
  clearedClosedTrades: number
  /** issue 561：本次重置的账户范围（显式 id 或连接缺省账户的解析值）。 */
  accountId: string
  balances: [string, number | string][]
  preSummary: ResetPreSummaryDoc
  atMs: number
  actor: string
  reason?: string
  error?: string
}

export const resetApi = {
  /**
   * 交易环境重置的写路径（`system.reset_trading`，#458）：确认词逐字
   * "RESET"；Live 模式内核拒绝；重置动作写 `data/audit/manual-ops.jsonl`
   * 审计行（含重置前状态快照）。停策略、撤挂单、清订单/成交/持仓记录、
   * 按 seed 复原现金 —— 一次调用全部完成。
   */
  systemResetTrading: (body: ResetTradingBody) =>
    request<ResetTradingResultDoc>('/system/reset-trading', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}
