/**
 * Market domain — the market-plugin readout and credential plumbing (#485),
 * plus the E29 K-line series (mirror core/market_api/src/kline.rs wire shape).
 */
import { request } from './core.ts'

// ── E29 K-line (mirror core/market_api/src/kline.rs wire shape) ─────────────

/** Wire interval spelling (serde `snake_case`): sec1…day1. */
export type KlineIntervalWire =
  | 'sec1' | 'sec5' | 'sec15'
  | 'min1' | 'min5' | 'min15'
  | 'hour1' | 'hour4' | 'day1'

/** The nine intervals, in ascending span — dropdown order. */
export const KLINE_INTERVALS: { value: KlineIntervalWire; label: string }[] = [
  { value: 'sec1', label: '1秒' },
  { value: 'sec5', label: '5秒' },
  { value: 'sec15', label: '15秒' },
  { value: 'min1', label: '1分' },
  { value: 'min5', label: '5分' },
  { value: 'min15', label: '15分' },
  { value: 'hour1', label: '1时' },
  { value: 'hour4', label: '4时' },
  { value: 'day1', label: '1天' },
]

/**
 * One OHLCV bar (camelCase wire). Prices are 0..1 prediction tokens; market_api
 * decimals cross as JSON numbers (finite values serialize as numbers).
 */
export interface KlineBar {
  symbol: string
  interval: KlineIntervalWire | string
  openTimeMs: number
  closeTimeMs: number
  open: number | string
  high: number | string
  low: number | string
  close: number | string
  volume: number | string
  tradeCount: number
  /** True exactly once the bar has closed; the LAST bar is usually false (growing). */
  isClosed: boolean
}

/** The `kline.history` readout (proxied at GET /api/kline-history). */
export interface KlineHistoryDoc {
  symbol: string
  interval: KlineIntervalWire | string
  klines: KlineBar[]
  error?: string
}

// ── #485 插件读数（market.plugin.get；绑定即启用后写路径已移交账户绑定）────

/** `market.plugin.get` readout：本会话 active、注册表、下次会话将选什么。 */
export interface MarketPluginReadoutDoc {
  activePlugin: string
  registered: { name: string; type: string; active: boolean }[]
  /** 下次会话将选什么：持久格里的已注册名，否则本会话 active。 */
  nextSessionPlugin: string
  /** 持久格里持有已注册名（写过某次；reset 清格后为 false）。 */
  persisted: boolean
  /** 本会话由显式 `--market-plugin` 解析 —— 写入动不了旗标钉住的会话。 */
  flagPinned: boolean
  /**
   * 本会话启用来源（boot 四层解析的披露）：`flag`（显式旗标）、
   * `accounts`（绑定即启用 —— 由账户 venue 绑定派生）、`persisted`
   * （旧持久格，仅在无绑定时生效）、`fallback`（全空回落首个注册者）。
   */
  source?: 'accounts' | 'persisted' | 'flag' | 'fallback'
  /** 派生出的 venue 集合（去重后的非空账户绑定）；source 非 accounts 时为空。 */
  derivedVenues?: string[]
  error?: string
}

/** 一个插件一行：需要哪些 env 变量、下一会话哪些解析到了、ready 与否。 */
export interface PluginCredentialRowDoc {
  plugin: string
  /** executor 在 boot 读的变量名（表序）。 */
  requiredVars: string[]
  /** 下一会话将解析到的名字（环境或持久格，env 优先）——只有名字。 */
  resolvedVars: string[]
  /** 全部所需名都解析到 → executor 下一会话能动。 */
  ready: boolean
}

/** `market.credentials.status` 读数。 */
export interface CredentialsStatusDoc {
  rows: PluginCredentialRowDoc[]
  error?: string
}

/** `market.credentials.set` 回执 —— written 只含变量名，值从不回显。 */
export interface CredentialsUpdateDoc {
  plugin?: string
  written?: string[]
  reset?: boolean
  atMs?: number
  actor?: string
  reason?: string | null
  note?: string
  error?: string
}

export const marketApi = {
  /**
   * 插件页行情插件的读路径（`market.plugin.get`，#485，只读）：本会话
   * active、编译进内核的注册表、下次会话将选什么、是否被显式旗标钉住。
   * 绑定即启用（2026-10-16 裁决）后本面板不再有 market.plugin.set 与
   * extension.enable/disable 写路径 —— 启用集合由账户绑定派生（账户切换器）。
   */
  marketPlugin: () => request<MarketPluginReadoutDoc>('/market-plugin'),
  /**
   * 行情插件凭据的读路径（`market.credentials.status`，只读）：每插件所需
   * env 变量名 + 下一会话解析到没有（真实 env 或持久格，env 优先），ready
   * = executor 下一会话能动。值永不过线 —— 只有名字和有无。
   */
  marketCredentials: () => request<CredentialsStatusDoc>('/market-credentials'),
  /**
   * 行情插件凭据的写路径（`market.credentials.set`）：next_session ——
   * 值 upsert 进 .env（唯一持久凭据格，仅属主可读），启动器下次启动时
   * 加载（进程环境里已有的同名变量始终 outrank 文件）。
   * 未注册插件名 / 需求表外变量名在内核 plan-first 拒绝。
   */
  marketCredentialsSet: (body: {
    plugin: string
    vars?: Record<string, string>
    reset?: boolean
    reason?: string
  }) =>
    request<CredentialsUpdateDoc>('/market-credentials/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),

  /**
   * E29 (§12.2): the K-line chart's bars — a thin proxy of the core's
   * `kline.history` (read-only). `limit` clamps to 1..=1000 kernel-side;
   * the last bar is the GROWING one (`isClosed: false`), re-polled on the
   * panel's fast tick. The panel has no WebSocket, so this is the whole
   * data channel: poll, redraw, poll.
   */
  klineHistory: (params: { symbol: string; interval: KlineIntervalWire; limit?: number }) =>
    request<KlineHistoryDoc>('/kline-history', {}, {
      symbol: params.symbol,
      interval: params.interval,
      limit: params.limit,
    }),
}
