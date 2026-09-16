/**
 * 回测报告 JSON 的 TS 镜像 — 对应 core/blitzkrieg_core/src/backtest.rs 的
 * `BacktestReport` / `TradeStats` / `OrderCounts`（serde camelCase wire）。
 * 字段名与内核一一对应；`strategies` / `feed` / `blocked` 是内核 JSON Value
 * 的 verbatim 透传，按需宽松读取。
 */

/** `SourceStats` (data_source.rs) — parser/stream counters. */
export interface SourceStats {
  events: number
  malformedLines: number
  outOfOrderEvents: number
}

/** `OrderCounts` — grouped by the last status each order reached. */
export interface OrderCounts {
  orders: number
  filled: number
  cancelled: number
  rejected: number
  failed: number
  liveAtEnd: number
}

/** `TradeStats` — aggregate stats over the realized equity curve. */
export interface TradeStats {
  closed: number
  wins: number
  losses: number
  /** Trades that closed exactly flat (neither win nor loss). */
  flat: number
  grossProfitUsd: number
  grossLossUsd: number
  netPnlUsd: number
  avgPnlUsd: number
  /** Already a percentage (0..100), not a 0..1 fraction. */
  winRatePct: number
  /** gross profit / |gross loss|; omitted when there were no losing trades. */
  profitFactor?: number
  maxDrawdownUsd: number
  /** Drawdown as a % of the equity peak that preceded it. */
  maxDrawdownPct: number
  feesUsd: number
}

export interface BacktestTradeLine {
  id: string
  asset: string
  direction: string
  reason: string
  netPnlUsd: number
  netPnlPct: number
}

/** `FillModel` — the only difference between a faithful replay and a stress run. */
export interface FillModel {
  takerSlippageTicks: number
  makerLatencyMs: number
  makerFillProbBps: number
}

/** Per-strategy accounting row (`engine.stats.strategies[]`, verbatim). */
export interface BacktestStrategyRow {
  name: string
  source?: string
  enabled?: boolean
  ordersPlaced?: number
  ordersRejected?: number
  limitRejected?: number
  blockedTiming?: number
  blockedMomentum?: number
  gateExemptions?: string[]
  closedTrades?: number
  wins?: number
  losses?: number
  feesUsd?: number | string
  netPnlUsd?: number | string
  openPositions?: number
  openNotionalUsd?: number | string
  rejectionCauses?: Record<string, number> | null
}

/** `blocked` — gate rejections by strategy plus the declared exemption list. */
export interface BacktestBlocked {
  momentum?: number
  timing?: number
  byStrategy?: Record<string, { momentum?: number; timing?: number }>
  declaredExemptions?: { strategy: string; gates: string[] }[]
}

/** `feed` — engine counters (books/tops/spots/rounds/evaluations/signals…). */
export interface BacktestFeed {
  books?: number
  tops?: number
  spots?: number
  rounds?: number
  evaluations?: number
  signals?: number
  placeRejected?: number
  [extra: string]: unknown
}

export interface BacktestReport {
  source: string
  sourceStats: SourceStats
  tickMs: number
  tailMs: number
  startAtMs: number
  endAtMs: number
  virtualMs: number
  fillModel: FillModel
  entryMakerTimeoutMs: number
  orders: OrderCounts
  fills: number
  trades: TradeStats
  strategies: BacktestStrategyRow[]
  feed: BacktestFeed
  blocked: BacktestBlocked
  openPositions: number
  openNotionalUsd: number | string
  riskAlerts: string[]
  errors: string[]
  tradeLines: BacktestTradeLine[]
  /** Trades beyond the `tradeLines` cap (list truncated, counts not). */
  tradeLinesTruncated: number
  /** True when the replay had to force `Mode::Dry` (it always does). */
  forcedDry: boolean
}
