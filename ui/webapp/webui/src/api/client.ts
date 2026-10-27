/**
 * Gateway API client for the Blitzkrieg panel.
 *
 * Auth: user/password login (`POST /api/login`, set via
 * `BLITZKRIEG_PANEL_USER`/`BLITZKRIEG_PANEL_PASSWORD` on the server) issues a
 * session token kept in localStorage; every /api call carries it as
 * `X-Auth-Token`. (#460: there is no `?token=` hand-off — the query channel is
 * gone server-side because it leaks into logs/history/Referer.)
 */

import type { BacktestReport } from '../backtest'

const LS_KEY = 'blitzkrieg-panel-token'
/** When the current session token was issued (login click), for the 设置 page. */
export const LOGIN_AT_KEY = 'blitzkrieg-panel-login-at'

/** When the current session was issued, or null when unknown/pre-restart. */
export function loginAt(): number | null {
  const raw = localStorage.getItem(LOGIN_AT_KEY)
  const n = raw ? Number(raw) : 0
  return Number.isFinite(n) && n > 0 ? n : null
}

let token = (() => localStorage.getItem(LS_KEY) ?? '')()

export function getToken(): string {
  return token
}

export function clearToken(): void {
  token = ''
  localStorage.removeItem(LS_KEY)
}

export function hasToken(): boolean {
  return token.length > 0
}

/**
 * A non-2xx gateway response.
 *
 * Written without a TypeScript parameter property (`constructor(readonly
 * status: …)`) so the class stays loadable under Node's strip-only TypeScript
 * mode — the panel's `check:*` gates import `src/` directly rather than through
 * Vite, and parameter properties are syntax that strip-only refuses to
 * transform. One assignment is a small price for keeping the modules testable
 * as-is.
 */
export class ApiError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.status = status
    this.name = 'ApiError'
  }
}

/**
 * Gateway liveness probe (`GET /api/ping`).
 *
 * The one endpoint that answers without a session — deliberately, and with
 * nothing in the reply but this. Its purpose is to let the panel tell three
 * situations apart that otherwise look identical:
 *
 *   * gateway unreachable → this resolves `null`;
 *   * gateway alive, session gone → resolves with `authRequired: true`;
 *   * gateway alive, session valid → the ordinary `/api/*` calls just work.
 *
 * Without it, a rejected token and a crashed gateway both surface as "network
 * error", which is how a stale `localStorage` token used to strand the panel on
 * an alert with no way back to the login form.
 */
export async function ping(): Promise<{ ok: boolean; authRequired: boolean } | null> {
  try {
    const res = await fetch('/api/ping', { headers: { Accept: 'application/json' } })
    if (!res.ok) return null
    const doc = (await res.json()) as { ok?: boolean; authRequired?: boolean }
    return { ok: doc.ok ?? false, authRequired: doc.authRequired ?? false }
  } catch {
    return null
  }
}

export function setToken(next: string): void {
  token = next.trim()
  if (token) localStorage.setItem(LS_KEY, token)
  else localStorage.removeItem(LS_KEY)
}

/** Exchange user/password for a session token. Throws ApiError on failure. */
export async function login(user: string, password: string): Promise<void> {
  const res = await fetch('/api/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ user, password }),
  })
  if (!res.ok) {
    let msg = '用户名或密码错误'
    try {
      const doc = (await res.json()) as { error?: string }
      if (doc?.error) msg = doc.error
    } catch {
      /* server returned no body; keep default */
    }
    throw new ApiError(res.status, msg)
  }
  const doc = (await res.json()) as { ok: boolean; token?: string; error?: string }
  if (!doc.ok || !doc.token) throw new ApiError(res.status, doc.error ?? '登录失败')
  setToken(doc.token)
  localStorage.setItem(LOGIN_AT_KEY, String(Date.now()))
}

export async function logout(): Promise<void> {
  try {
    await fetch('/api/logout', { headers: { 'X-Auth-Token': token } })
  } catch {
    /* network errors during logout are non-fatal */
  }
  clearToken()
  localStorage.removeItem(LOGIN_AT_KEY)
  window.location.reload()
}

/**
 * Optional query params are encoded HERE (URLSearchParams), never spliced into
 * `path` by callers: the path stays a static literal, so a stray value can
 * never turn the request into something it was not written to be (the
 * request-forgery shape the push gate refuses).
 */
async function request<T>(
  path: string,
  init?: RequestInit,
  query?: Record<string, string | number | undefined>,
): Promise<T> {
  const qs = query
    ? new URLSearchParams(
        Object.entries(query)
          .filter(([, v]) => v !== undefined && v !== '')
          .map(([k, v]) => [k, String(v)] as [string, string]),
      ).toString()
    : ''
  const res = await fetch(`/api${path}${qs ? `?${qs}` : ''}`, {
    ...init,
    headers: {
      'X-Auth-Token': token,
      'Content-Type': 'application/json',
      ...(init?.headers ?? {}),
    },
  })
  if (res.status === 401) {
    throw new ApiError(401, '未登录或会话已失效，请重新登录。')
  }
  if (res.status === 403) {
    throw new ApiError(403, 'Origin 被拒（CORS）：请从面板地址访问。')
  }
  if (!res.ok) {
    throw new ApiError(res.status, `网关返回 ${res.status}`)
  }
  return (await res.json()) as T
}

export interface CommandDoc {
  ok: boolean
  action?: string
  message?: string
  /** E27 (§8.2): the mode-handshake refusal when a `strategy … on` is refused. */
  reason?: string
  [extra: string]: unknown
}

// ── E25 arbitration audit (mirror arbitration/audit.rs's IntentAuditRecord) ──

/** One gate's verdict inside an audited suggestion (§3.2). */
export interface GateTraceView {
  gate: 'LEGALITY' | 'RISK' | 'RESERVATION' | 'PHYSICS' | string
  outcome: 'PASS' | 'MODIFY' | 'REJECT' | string
  /** The kernel's own justification — the UI prints it verbatim (§13.4). */
  detail: string
}

/** The decision, tagged by `status` — same vocabulary as the tail filter. */
export interface DecisionView {
  status: 'APPROVED' | 'MODIFIED' | 'REJECTED' | string
  /** Present on REJECTED. */
  reason?: string
  gate?: string
  detail?: string
  request_id?: string
  shares?: string | number
  price?: string | number
  physics?: {
    stopPrice: string | number
    forceExitSec: number
    ladder: { atPct: string | number; closeRatio: string | number; moveStopTo: string | number | null }[]
  }
  modification?: { kind: string; suggested?: string | number; approved?: string | number; limit?: string; tick?: string | number }
}

/** One audited suggestion (§3.4). Decimal values cross as STRINGS. */
export interface IntentAuditRecordView {
  tsMs: number
  accountId: string
  strategy: string
  intentId: string
  intent: unknown
  decision: DecisionView
  gates: GateTraceView[]
  latencyUs: number
}

export interface IntentAuditTailDoc {
  records: IntentAuditRecordView[]
  total?: number
  error?: string
}

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

// ── 策略 tunables 编辑卡（issue 481；mirror core schema StrategyParamsReadout）──

/** 一个声明过的 tunable：名称 + 声明域 [min, max] + 当前生效值（十进制字符串）。 */
export interface TunableViewDoc {
  name: string
  /** 当前生效值（注册表 cell 或 manifest 默认），十进制字符串。 */
  value: string
  min: string
  max: string
}

/** `strategy.params.get` 回执：一个策略的声明旋钮与生效值。 */
export interface StrategyParamsReadoutDoc {
  name: string
  found: boolean
  /** 空数组 = 该策略没有可调参数（未声明 tunables 域）。 */
  tunables: TunableViewDoc[]
  evolvable: boolean
  /** 注册表覆盖层是否挂载（写入后为 true）。 */
  paramsAttached: boolean
  note?: string
}

/**
 * `strategy.params.set` 请求体：一个策略 + 要改的旋钮（十进制字符串）+
 * 审计备注。多旋钮补丁是原子的：一个非法值整体拒。
 */
export interface StrategyParamsSetBody {
  name: string
  params: Record<string, string>
  reason?: string
}

/** 一个旋钮的审计变更（内核 StrategyParamChange 的 wire 形状）。 */
export interface StrategyParamChangeDoc {
  knob: string
  from: string
  to: string
  /** live：策略下一次 evaluate 即读到新值（cell 合并到 manifest 默认包上）。 */
  effect: 'live' | string
}

/** `strategy.params.set` 回执：改了什么、谁改的、何时、未持久化。 */
export interface StrategyParamsSetUpdateDoc {
  name: string
  applied: StrategyParamChangeDoc[]
  atMs: number
  actor: string
  reason?: string
  persisted: boolean
  note: string
  error?: string
}

// ── 跨所配对视图（issue 481；mirror core unified.list JSON shape）────────────

/** 一个映射里的一侧 venue（数据行 —— venue 是数据，不是分支）。 */
export interface UnifiedVenueRow {
  venue: string
  conditionId: string
  asset?: string | null
  expiresAtMs: number
  status: string
}

/** `unified.list` 的一个事件行：配对状态 + 两侧 venue + discrepancy 标记。 */
export interface UnifiedEventRow {
  id: string
  title: string
  status: 'paired' | 'singleLegAvailable' | string
  settlementSource?: string
  venues: UnifiedVenueRow[]
  discrepancies: string[]
}

/** `unified.list` 回执。 */
export interface UnifiedListDoc {
  version?: string
  events: UnifiedEventRow[]
  error?: string
}

// ── ④ 统一映射候选（unified.candidates / confirm / revoke）────────────────

/** 一个候选提议里的一侧 listing —— 就是 `unified.confirm` 回传的形状（原样
 *  往返，ledger 侧会重新验证一切）。 */
export interface UnifiedCandidateListing {
  venue: string
  conditionId: string
  upTokenId: string
  downTokenId: string
  asset?: string | null
  title: string
  settlementRules: string
  settlementSource: string
  expiresAtMs: number
  status: string
}

/** `unified.candidates` 的一行：两侧 listing + 匹配分。 */
export interface UnifiedCandidateRow {
  a: UnifiedCandidateListing
  b: UnifiedCandidateListing
  score: number
  expiryDeltaMs: number
}

/** `unified.candidates` 回执。 */
export interface UnifiedCandidatesDoc {
  version?: string
  candidates: UnifiedCandidateRow[]
  error?: string
}

/** `unified.confirm` 回执（事件已落账本 + 引擎可见）。 */
export interface UnifiedConfirmDoc {
  version?: string
  event?: { id: string; title: string; status: string; venues: string[] }
  error?: string
}

/** `unified.revoke` 回执。 */
export interface UnifiedRevokeDoc {
  version?: string
  revoked?: string
  error?: string
}

// ── 挂单卡（issue 456；mirror core model.rs TrackedOrder wire shape）────────

/** `orders.list` 的一行：内核记账的订单，status 是大写枚举（LIVE/FORCE_CLOSED/…）。 */
export interface OrderRow {
  orderId: string
  strategy: string
  asset: string
  direction: string
  tokenId: string
  side: string
  mode: string
  price: number | string
  size: number | string
  filledSize: number | string
  status: string
  roundSlot: number
  /**
   * E28 (§9.2): the account the order belongs to. Empty/absent = pre-E28
   * core — unattributed, shown regardless of the active account.
   */
  accountId?: string
  /**
   * #427-style venue tag. The orders wire does not carry it yet (positions
   * and trades do), so it is always absent today: the venue filter keeps
   * such rows visible — an order must not be silently dropped just because
   * the kernel cannot say which venue it trades. When the kernel starts
   * tagging orders this field lights up with no UI change.
   */
  venue?: string
}

/** `orders.force_close` 请求体：只点名订单，判定（幽灵/活轮）在内核。 */
export interface ForceCloseOrderBody {
  orderId: string
}

// ── 轮次时间周期设置卡（mirror core schema RoundDurationResult / runtime cell）──

/** 白名单一行（内核 RoundDurationOption 的 wire 形状；label 缺省 = 非标档位）。 */
export interface RoundDurationOption {
  sec: number
  label?: string
}

/** `round_duration.get` 回执：本会话生效值 + 来源 + 白名单 + 下次会话将解析到什么。 */
export interface RoundDurationDoc {
  effectiveSec: number
  /** cli_or_env = 显式旗标/环境变量在管（本卡写入不会赢）；default/runtime/toml/env。 */
  source: string
  allowed: RoundDurationOption[]
  nextSessionSec: number
  persisted: boolean
  error?: string
}

/** `round_duration.set` 请求体：`durationSec`（白名单档位）或 `reset`（清格）。 */
export interface RoundDurationSetBody {
  durationSec?: number
  reset?: boolean
  reason?: string
}

/** `round_duration.set` 回执：下次会话将跑的值 + 是否为清格操作。 */
export interface RoundDurationUpdateDoc {
  nextSessionSec: number
  reset: boolean
  atMs: number
  actor: string
  reason?: string
  note: string
  error?: string
}

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

// ── 钱包现金流水（account.journal，#509 内核 / #511 动词）────────────────────

/** 一行流水：JournalEntry 的 camelCase wire 形状，amount/balanceAfter 是
 *  crate::decimal 字符串（内核约定：小数一律字符串过线）。 */
export interface AccountJournalRowDoc {
  atMs: number
  /** snake_case：seed/buy_fill/sell_fill/fee/merge/redemption/cashflow/realign/reset。 */
  kind: string
  amount: string
  balanceAfter: string
  /** 产生该行的来源标记（boot / venue / 订单 id / 审计行 …）。 */
  ref: string
}

/** `account.journal` 回执：最近 limit 行（升序）；truncated = 内存窗口比返回
 *  的多 —— 读者看到的是窗口（JOURNAL_CAP = 10000），不是全史。 */
export interface AccountJournalDoc {
  accountId: string
  rows: AccountJournalRowDoc[]
  truncated: boolean
  error?: string
}

// ── 账户↔交易所绑定（account.venue.get / account.venue.set）─────────────────

/** 一行绑定：venue 空串 = 未绑定（可交易任意交易所）。 */
export interface AccountVenueRowDoc {
  accountId: string
  venue: string
  /** 谁写的（boot / panel / venue-derive-migration …）。 */
  actor: string
  updatedAtMs: number
}

/** `account.venue.get` 回执。 */
export interface AccountVenueDoc {
  version: string
  venues: AccountVenueRowDoc[]
  /** 本连接的默认账户（会话切换后 = 会话值）。 */
  active: string
  error?: string
}

/** `account.venue.set` 回执：persisted=false = 内存生效但未落盘。 */
export interface AccountVenueSetDoc {
  accountId: string
  venue: string
  persisted: boolean
  atMs: number
  actor: string
  note: string
  error?: string
}

// ── 跨所套利创建（arb.config.set / arb.config.get，合同 v1）─────────────────
// 配置意图模型：A 腿（交易所+账户）+ B 腿（交易所+账户）+ 币对多选，创建即
// 持久化（内核写 accounts.toml，面板零持久化职责）。专用代理路由（body 即
// params 逐字转发，同 account-venue/set 的纪律）。B≠A（venue 与账户两个维度）
// 的校验在内核是 APPLICATION 错误 —— 消息点名哪个字段非法，弹窗把原文转述。

/** `arb.config.set` 请求体（camelCase wire 与合同 v1 对齐；version 恒 1）。 */
export interface ArbConfigSetBody {
  version: 1
  legAVenue: string
  legAAccount: string
  legBVenue: string
  legBAccount: string
  pairs: string[]
}

/** `arb.config.get` / `arb.config.set` 的回执（configured 分支预填与回显用）。 */
export interface ArbConfigDoc {
  version: number
  configured: boolean
  legAVenue: string | null
  legAAccount: string | null
  legBVenue: string | null
  legBAccount: string | null
  pairs: string[]
  error?: string
}

// ── 显示账户（issue 559）───────────────────────────────────────────────────
// 身份唯一源改造：行情页倒计时卡的「这是谁的市场」不再从账户绑定派生，而
// 是内核的 display_account 独立事实（accounts.toml 顶层持久化 + 运行时生
// 效）。写动词 `display.account.set`（payload {accountId}）——内核回执里
// 的 displayVenue 是该显示账户绑定 venue 的读数（空串 = 未绑定，可交易任
// 意交易所），与本面板会话默认账户彻底解耦：切换器选新账户 ≠ 行情身份
// 跟着跳。老内核没有该事实 → 快照缺字段，页面回落账户绑定链（现行为）。

/** `display.account.set` 回执：persisted=false = 内存生效但未落盘。 */
export interface DisplayAccountSetDoc {
  /** 内核收下的显示账户 id。 */
  accountId: string
  /** 该显示账户绑定的 venue 读数（空串 = 未绑定）。 */
  displayVenue: string
  persisted: boolean
  atMs: number
  note: string
  error?: string
}

// ── 多账户扇出（account.trading.set / account.strategies.set）───────────────
// 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。

/** `account.trading.set` 回执：关 = 拒开仓、平仓照放；persist 失败内核整体拒绝。 */
export interface AccountTradingSetDoc {
  accountId: string
  enabled: boolean
  persisted: boolean
  atMs: number
  reason?: string
  note: string
  error?: string
}

/**
 * `account.strategies.set` 回执。keptExitsOnly = 该账户仍持仓但被移出分配
 * 的策略 —— 内核以 exits-only 保活到平完，安全网永不孤儿化持仓。
 */
export interface AccountStrategiesSetDoc {
  accountId: string
  strategies: string[]
  keptExitsOnly: string[]
  persisted: boolean
  atMs: number
  reason?: string
  note: string
  error?: string
}

// ── 账户簿运行时增删（account.create / account.remove，#516 内核面）─────────

/** `account.create` 回执。 */
export interface AccountCreateDoc {
  id: string
  persisted: boolean
  atMs: number
  actor: string
  reason?: string
  note: string
  error?: string
}

/** `account.remove` 回执。 */
export interface AccountRemoveDoc {
  id: string
  persisted: boolean
  atMs: number
  actor: string
  reason?: string
  note: string
  error?: string
}

// ── 轮次周期集（account.rounds.set，轮次多族 B2）─────────────────────────────
// 账户的轮长族声明写动词：params `{accountId, rounds, reason?}`（camelCase
// wire，rounds = 白名单秒数数组，空数组 = 不限制）。移除有敞口的族会被内核
// 整体拒绝（错误消息点名族列表）—— 面板不预判敞口，拒绝原文透出。老内核
// 不认识该动词 → JSON-RPC unknown-method 错误原样返回，卡面据此降级只读。

/** `account.rounds.set` 回执：persisted=false = 内存生效但未落盘。 */
export interface AccountRoundsSetResult {
  accountId: string
  /** 内核收下的族集（秒）；空数组 = 未声明限制（许可轴空集放行）。 */
  rounds: number[]
  persisted: boolean
  atMs: number
  actor: string
  reason?: string
  note: string
  runtimeEnabled?: boolean
  error?: string
}

export const api = {
  snapshot: () => request<Snapshot>('/snapshot'),
  plugins: () => request<PluginsDoc>('/plugins'),
  /**
   * E25 (#331): the arbitration audit tail — a thin proxy of the core's
   * `intent.audit.tail` (§12.3). Filters pass through untouched.
   */
  intentAuditTail: (params?: { limit?: number; strategy?: string; decision?: string }) =>
    request<IntentAuditTailDoc>('/intent-audit', {}, {
      limit: params?.limit,
      strategy: params?.strategy,
      decision: params?.decision,
    }),
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
  /**
   * 策略 tunables 编辑卡的读路径（`strategy.params.get`，issue 481，只读）：
   * 一个策略声明的全部可调参数（名称 + 声明域 [min, max]）与当前生效值。
   * schema 源是策略自己的 manifest tunables —— 内核不发明域。
   */
  strategyParams: (name: string) =>
    request<StrategyParamsReadoutDoc>(
      `/strategy-params?name=${encodeURIComponent(name)}`,
    ),
  /**
   * 策略 tunables 编辑卡的写路径（`strategy.params.set`，issue 481）：
   * `risk.setExit` 同款姿态 —— 内核对每个旋钮按声明域校验（plan-first，
   * 一个非法整体拒），热写注册表 cell，回执逐项 old→new 带 effect=live
   * （策略下一次 evaluate 即读到新值）。未持久化：重启回落 manifest 默认
   * （+ 已采纳的进化晋升）。
   */
  strategyParamsSet: (body: StrategyParamsSetBody) =>
    request<StrategyParamsSetUpdateDoc>('/strategy-params/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * 跨所配对视图的读路径（`unified.list`，issue 481，只读）：已确认的
   * 统一映射快照 —— 每个事件的配对状态、两侧 venue、discrepancy 标记。
   * venue 一律数据渲染。
   */
  unifiedList: () => request<UnifiedListDoc>('/unified'),
  /**
   * ④ 统一映射候选的读路径（`unified.candidates`，只读）：当前轮的跨所
   * 配对提议 —— 自动匹配只提议，confirm 才生效。空数组 = 本轮没有
   * ≥2 个已声明 venue 的配对（或单插件世界）。
   */
  unifiedCandidates: () => request<UnifiedCandidatesDoc>('/unified-candidates'),
  /**
   * ④ 候选确认写路径（`unified.confirm`）：`{title, listings, actor?}` 原样
   * 转发。ledger 侧重新验证（标题相似度、资产一致性、一 venue 一事件），
   * 被拒的 confirm 原地不动。
   */
  unifiedConfirm: (body: { title: string; listings: UnifiedCandidateListing[]; actor?: string }) =>
    request<UnifiedConfirmDoc>('/unified/confirm', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * ④ 映射撤销写路径（`unified.revoke`）：`{eventId, actor?}` 原样转发，
   * 账本落审计、引擎视图同步拔除（策略立刻看不见）。
   */
  unifiedRevoke: (body: { eventId: string; actor?: string }) =>
    request<UnifiedRevokeDoc>('/unified/revoke', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
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
   * 轮次时间周期设置卡的读路径（`round_duration.get`，只读）：本会话生效
   * 秒数、来源、可切换白名单、下次会话将解析到什么（持久格 outranks TOML/
   * 默认，但显式 `--round-sec` outranks 持久格 —— source 字段如实报告）。
   */
  roundDuration: () => request<RoundDurationDoc>('/round-duration'),
  /**
   * 轮次时间周期的写路径（`round_duration.set`）：next_session 语义 ——
   * 轮次周期是网格坐标系（round_slot 是 UTC 网格索引），本会话网格不动，
   * 写持久格重启生效。`reset: true` 清格回落启动解析。
   */
  roundDurationSet: (body: RoundDurationSetBody) =>
    request<RoundDurationUpdateDoc>('/round-duration/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
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
  /**
   * 挂单卡的读路径（issue 456）：内核记账的全部订单（orders.list 的
   * TrackedOrder 行）。挂单卡据此渲染，每行按 status 决定是否给强平按钮。
   */
  orders: () => request<{ orders: OrderRow[] }>('/orders'),
  /**
   * 挂单卡的强平写路径（issue 456）：把订单标记为 ForceClosed 并撤场地侧
   * 残留。轮次仍活着的订单内核拒绝（错误信息原样抛回）——强平只兜幽灵，
   * 活单走撤单。每次调用内核写一条 force_close 审计。
   */
  forceCloseOrder: (body: ForceCloseOrderBody) =>
    request<{ success: boolean }>('/orders/force-close', {
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
  /**
   * 钱包现金流水卡的读路径（`account.journal`，#511，只读）：一个账户的
   * append-only 现金流水最近 limit 行（升序）。accountId 缺省 = 会话默认
   * 账户（§9.5）；limit 缺省 100、内核 clamp 1..=500。truncated = true 时
   * 内存窗口比返回的多，面板必须把「这是窗口不是全史」说出来。
   */
  accountJournal: (params?: { accountId?: string; limit?: number }) =>
    request<AccountJournalDoc>('/account-journal', {}, {
      accountId: params?.accountId,
      limit: params?.limit,
    }),
  // ── 账户级聚合读数（PR H 账户筛选修复）─────────────────────────────────────
  // 专用代理路由（同 account-journal 的纪律：JSON-RPC 信封不走 /command
  // 文本动词通道）。accountId 缺省 = 全局口径（与旧面板逐字节一致）。

  /**
   * 闭环统计（`trades.summary` 薄代理）。带 accountId 时内核按行过滤
   * 重算该账户的 summary；缺席 = 全局持久 headline。
   */
  tradesSummary: (params?: { accountId?: string }) =>
    request<{ summary?: CumulativeTradeSummary | null; error?: string }>(
      '/trades-summary', {}, { accountId: params?.accountId },
    ),
  /**
   * 引擎统计（`engine.stats` 薄代理）。带 accountId 时内核把
   * `strategies` 行切到该账户（strategy_stats_for_account，同形状）；
   * 缺席 = 全局 fold（每行带 `accounts` 数组）。
   */
  engineStatsFor: (params?: { accountId?: string }) =>
    request<EngineStats>('/strategy-stats', {}, { accountId: params?.accountId }),
  // ── 账户↔交易所绑定（account.venue.get / account.venue.set）────────────────
  // 专用代理路由（#508 的教训：JSON-RPC 信封走 /command 文本动词通道会被
  // 网关当 unknown command 拒掉 —— 绑定面必须走自己的 proxy 路由，同
  // account-journal 的纪律）。

  /** 绑定读数（只读）：accountId 缺省 = 全簿。venue 空串 = 未绑定。 */
  accountVenue: (params?: { accountId?: string }) =>
    request<AccountVenueDoc>('/account-venue', {}, {
      accountId: params?.accountId,
    }),
  /** 绑定写：venue 空串 = 解绑。内核校验插件名并持久化进 accounts.toml。 */
  accountVenueSet: (body: { accountId: string; venue: string; reason?: string }) =>
    request<AccountVenueSetDoc>('/account-venue/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  // ── 跨所套利创建（arb.config.set / arb.config.get，合同 v1）──────────────
  // 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。
  // 内核侧校验失败 = JSON-RPC APPLICATION 错误，网关包成 200 + `{error}` 文档
  // —— 调用方必须检查 `error` 并把消息原文转述给用户（消息点名非法字段）。
  // get 返回 configured=false = 老内核（不认识该动词）与「未配置」两种情况的
  // 区分在 feature-detect 层做（get 的 error 带 rpc -32601 字样 = 动词不存在）。

  /** 已配置双腿的读数（弹窗预填：configured=true 时按腿回显）。 */
  arbConfigGet: () => request<ArbConfigDoc>('/arb-config'),
  /** 双腿配置写：先于 setStrategy 调用（合同提交序：config 成功才启用）。 */
  arbConfigSet: (body: ArbConfigSetBody) =>
    request<ArbConfigDoc>('/arb-config/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  // ── 显示账户（issue 559）─────────────────────────────────────────────────
  // 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。
  // 写的是「行情页显示谁的市场」——与 `account <id>`（本面板会话默认账户）
  // 是两个独立事实：切换器成功路径两者都写，本动词 fire-and-forget，失败
  // 静默（快照下一拍带回新 displayVenue，页面回落链兜底）。
  /** 显示账户写：内核校验账户存在并持久化进 accounts.toml 顶层 display_account。 */
  displayAccountSet: (body: { accountId: string; reason?: string }) =>
    request<DisplayAccountSetDoc>('/display-account/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  // ── 多账户扇出（account.trading.set / account.strategies.set）──────────────
  // 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。

  /** 交易开关写：关 = 该账户拒开仓、平仓照放（内核 Gate 2 裁决并持久化）。 */
  accountTradingSet: (body: { accountId: string; enabled: boolean; reason?: string }) =>
    request<AccountTradingSetDoc>('/account-trading/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * 策略分配写：空列表 = 继承全部全局启用；非空 = 精确钉住。返回的
   * keptExitsOnly 是被移出但仍持仓的策略 —— 内核保活到平完。
   */
  accountStrategiesSet: (body: { accountId: string; strategies: string[]; reason?: string }) =>
    request<AccountStrategiesSetDoc>('/account-strategies/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  // ── 账户簿运行时增删（#517：添加账户弹窗 / 账户删除，只删不改）──────────────
  // 专用代理路由（body 即 params 逐字转发，同 account-venue/set 的纪律）。
  // 凭据不走这两个动词 —— 另行 market.credentials.set（值只下行、从不回显）。

  /** 建户：id 非空且不与现有重复、default 保留名被内核拒绝；venue 缺省 = 未绑定。 */
  accountCreate: (body: { id: string; name?: string; venue?: string; reason?: string }) =>
    request<AccountCreateDoc>('/account-create', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /** 删户：default 与有持仓的账户内核拒绝；删的是当前默认时内核回切。 */
  accountRemove: (body: { id: string; reason?: string }) =>
    request<AccountRemoveDoc>('/account-remove', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * 轮次周期集写（`account.rounds.set`，轮次多族 B2）：body 即 params 对象
   * （`{accountId, rounds, reason?}`），逐字转发（同 account-trading/set 的
   * 纪律）。rounds = 白名单秒数数组（空数组 = 不限制）；移除有敞口的族被
   * 内核整体拒绝 —— APPLICATION 错误原文随 `{error}` 回来，调用方必须把消
   * 息转述给用户。老内核不认识该动词 → error 带 JSON-RPC unknown-method 字
   * 样，卡面 feature-detect 降级为只读提示。
   */
  accountRoundsSet: (body: { accountId: string; rounds: number[]; reason?: string }) =>
    request<AccountRoundsSetResult>('/account-rounds/set', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /** Dispatch a gateway command verb (`status`/`start`/`stop`/…). */
  command: (cmd: string) =>
    request<CommandDoc>('/command', { method: 'POST', body: cmd }),
  /**
   * Strategy enable toggle (`strategy.enable` on the core). `accountId` 缺省
   * = default（全局开关分支，与旧面板逐字节一致）；非 default 账户走内核的
   * assignment=enablement 分支（账户级启停 —— 启停绑定在「账户+交易所」级）。
   * 回执带 `accountId` 回显（合同字段）。
   */
  setStrategy: (name: string, enabled: boolean, accountId?: string) => {
    // 文本动词通道不认 JSON 信封的 params —— accountId 走专用 JSON-RPC 代理
    // 路由（body 即 params 逐字转发，同 account-venue/set 的纪律）；缺省时
    // 走原 /api/command 文本动词（老面板路径不变）。
    if (accountId === undefined) {
      return request<CommandDoc>('/command', {
        method: 'POST',
        body: `strategy ${name} ${enabled ? 'on' : 'off'}`,
      })
    }
    return request<CommandDoc>('/strategy-enable', {
      method: 'POST',
      body: JSON.stringify({ name, enabled, accountId }),
    })
  },
  /** Manual operator flatten for one stable position id. */
  flatten: (positionId: string) =>
    request<CommandDoc>('/command', { method: 'POST', body: `flatten ${positionId}` }),
  /** E13: decide one evolution proposal (accept hot-swaps, reject/defer don't). */
  decideEvolution: (id: string, decision: 'accept' | 'reject' | 'defer') =>
    request<CommandDoc>('/command', {
      method: 'POST',
      body: `decide ${id} ${decision}`,
    }),
  /** E13: the unattended-evolution switch (`auto-evolve on|off`). */
  setAutoEvolve: (on: boolean) =>
    request<CommandDoc>('/command', {
      method: 'POST',
      body: `auto-evolve ${on ? 'on' : 'off'}`,
    }),
  /** #249: the evolution ENGINE switch (`evolve on|off`) — whether anything evolves. */
  setEvolve: (on: boolean) =>
    request<CommandDoc>('/command', {
      method: 'POST',
      body: `evolve ${on ? 'on' : 'off'}`,
    }),
  /** E13: undo the last accepted promotion of one strategy (一键回滚). */
  rollbackStrategy: (strategy: string) =>
    request<CommandDoc>('/command', { method: 'POST', body: `rollback ${strategy}` }),
  /**
   * 网络自检（`GET /api/netcheck`）：内核拨测它自己出网要走的每条路径。
   *
   * 网关侧是缓存 + 后台探测（20s TTL）：探测要拨真实端点、耗时以秒计，而网关
   * 的 HTTP 接受循环是单线程的 — 内联探测会连带冻住快照轮询。所以这里拿到的是
   * 「最近一次结果 + 是否正有一次在跑」，`probing` 为真时页面显示探测中，而不是
   * 拿旧结果冒充新结果。
   */
  netCheck: () => request<NetCheckDoc>('/netcheck'),
  /**
   * 发起一次新的网络自检（`POST /api/netcheck/probe`）。
   *
   * 返回的还是同一个文档：探测永远在网关侧后台跑（`probing: true`），调用方
   * 继续轮询 `netCheck()` 直到结果落地。如果已经有一次在跑，这次请求不会另起
   * 一次 —— 那次的result 就是答案。
   */
  probeNetCheck: () => request<NetCheckDoc>('/netcheck/probe', { method: 'POST' }),

  /**
   * 设置页「检查更新」按钮（VERSIONING.md §6.3）。网关只负责把请求转给内核并
   * 立即返回 —— 内核拨 GitHub 以秒计，网关的单线程接受循环绝不等它。轮询
   * snapshot 的 `systemVersion` 直到 `lastCheckMs` 变化；`error` 非空时是内核
   * 明确拒绝（例如 checkEnabled=false），必须原样展示，不许静默。
   */
  updateCheck: () => request<{ started: boolean; running?: boolean; error?: string }>('/version/check', { method: 'POST' }),
  /**
   * 设置页「自动更新」/「更新检查」开关（VERSIONING.md §7.4，#393）。内核是
   * 开关的唯一事实来源：它落盘 + 落审计后回显两个值；写盘失败必须报错
   * （静默回退的开关比没有更糟）。body 里缺席的键保持内核现值不动。
   */
  updateConfigure: (body: { autoUpdate?: boolean; checkEnabled?: boolean }) =>
    request<{ ok: boolean; autoUpdate?: boolean; checkEnabled?: boolean; error?: string }>(
      '/version/configure',
      {
        method: 'POST',
        body: JSON.stringify(body),
      },
    ),
  /**
   * 设置页「暂存下载」按钮（VERSIONING.md §7.5，#379）。让内核把较新的
   * 发布资产下载到 data/update/staging/ 并校验 SHA256 —— 校验过也只停在
   * 「重启可应用」，替换二进制永远是启动器的职责，内核不碰自己正在执行的
   * 文件。网关同样只转发、立即返回；轮询 snapshot 的 `stageState` 直到
   * phase 变化；`error` 非空是内核拒绝（autoUpdate=false），原样展示。
   */
  updateStage: () => request<{ started: boolean; running?: boolean; error?: string }>('/version/stage', { method: 'POST' }),

  // ── #353 回测（拉数据 → 配置 → 回测 → 看结果，全程浏览器内）────────────
  // 六条都是内核 `backtest.*` IPC 的纯代理：网关不加工任何参数（body 即
  // params，逐字转发），内核的 JSON-RPC 错误在网关侧包成 200 + `{error}`
  // 文档 — 所以每个调用方都要检查 `error` 字段，原样抛给页面展示。

  /** 数据集列表（`backtest.onchain.list`）：第一步「拉数据」的选择器。 */
  backtestOnchainList: () => request<BacktestDatasetListDoc>('/backtest/onchain-list'),
  /**
   * 拉数据（`backtest.onchain.pull`）：钱包 + 时间窗 + 可选资产过滤。
   * 空 `assets` = 整个钱包；命名资产各起一个可断点续拉的任务。
   */
  backtestOnchainPull: (params: OnchainPullParams) =>
    request<BacktestJobIdsDoc>('/backtest/onchain-pull', {
      method: 'POST',
      body: JSON.stringify(params),
    }),
  /** 发起回测（`backtest.run`）：数据集 + #351 模式档位。策略只传名字。 */
  backtestRun: (params: BacktestRunParams) =>
    request<{ jobId: string }>('/backtest/run', {
      method: 'POST',
      body: JSON.stringify(params),
    }),
  /** 轮询任务状态（`backtest.status`）：~1s 一次直到 done/failed。 */
  backtestStatus: (id: string) =>
    request<BacktestJobStatusDoc>(`/backtest/status?id=${encodeURIComponent(id)}`),
  /** 已完成任务的全量结果（`backtest.result`）。 */
  backtestResult: (id: string) =>
    request<BacktestResultDoc>(`/backtest/result?id=${encodeURIComponent(id)}`),
  /** 导出信封（`backtest.export`）：文件由浏览器自己保存（`fileName`+`content`）。 */
  backtestExport: (id: string) =>
    request<BacktestExportDoc>(`/backtest/export?id=${encodeURIComponent(id)}`),

  // ── #364 生效风控（`execution_policy.*` IPC 的纯代理；TOML 永不出内核）─────
  // 与回测六条同一纪律：网关不加工参数（body 即 params，逐字转发），内核的
  // JSON-RPC 错误在网关侧包成 200 + `{error}` 文档 — 每个调用方都要检查
  // `error` 字段，原样抛给页面展示。小数一律字符串过线（最短十进制规则）。

  /** 哪些账户可配 + 策略文件是否加载（`execution_policy.list`）：chips 的来源。 */
  executionPolicyList: () => request<ExecutionPolicyListDoc>('/execution-policy/list'),
  /**
   * 生效视图（`execution_policy.get`）：内核自己把 `[accounts.<id>]` 折叠到
   * `[defaults]` 上 — 浏览器拿到的永远是内核视角的「实际生效」，不是文件原文。
   */
  executionPolicyGet: (accountId: string) =>
    request<ExecutionPolicySectionDoc>(
      `/execution-policy?accountId=${encodeURIComponent(accountId)}`,
    ),
  /**
   * 预览（`execution_policy.preview`）：最近 ~100 笔已平仓交易按当前规则重放
   * （内核自己的 `evaluate`，UI 端没有第二套规则引擎）。
   */
  executionPolicyPreview: (accountId: string) =>
    request<ExecutionPolicyPreviewDoc>(
      `/execution-policy/preview?accountId=${encodeURIComponent(accountId)}`,
    ),
  /**
   * 写路径（`execution_policy.set`）：内核落盘→重读→落审计后才应答，应答
   * `{accountId}` — UI 保存后自己重新 `get` 刷新，不靠本地猜测。
   */
  executionPolicySet: (params: ExecutionPolicySetParams) =>
    request<{ accountId: string; error?: string }>('/execution-policy/set', {
      method: 'POST',
      body: JSON.stringify(params),
    }),
  /** 删除账户段（`execution_policy.reset`）：该账户折回 `[defaults]`。 */
  executionPolicyReset: (accountId: string) =>
    request<{ accountId: string; error?: string }>('/execution-policy/reset', {
      method: 'POST',
      body: JSON.stringify({ accountId }),
    }),
  /** 审计流水（`execution_policy.history`）：版本列表与回滚快照的来源。 */
  executionPolicyHistory: (accountId: string) =>
    request<ExecutionPolicyHistoryDoc>(
      `/execution-policy/history?accountId=${encodeURIComponent(accountId)}`,
    ),

  // ── #362 蓝图编辑器（compile 预览 + save 落盘，全程走内核 IPC）────────────
  // 两条都是内核 `blueprint.*` IPC 的纯代理（网关 body 即 params 逐字转发），
  // 内核错误包成 200 + `{error}` — 调用方必须检查 `error` 并内联渲染。

  /**
   * 编译预览（`blueprint.compile`）：把当前画布 JSON 编成 Lua 源码。纯只读；
   * 拒绝消息含节点 id（#361 三阶段编译链保证），必须原样展示，绝不静默。
   */
  blueprintCompile: (json: string) =>
    request<BlueprintCompileDoc>('/blueprint/compile', {
      method: 'POST',
      body: JSON.stringify({ json }),
    }),
  /**
   * 保存策略包（`blueprint.save`）：内核校验名字 → 现场编译 → 写入
   * blueprint.json + strategy.lua + manifest.json。UI 永不碰文件系统；
   * 同名包已存在时必须显式 `overwrite`（回执里带三个文件路径 + sha256）。
   */
  blueprintSave: (body: BlueprintSaveBody) =>
    request<BlueprintSaveDoc>('/blueprint/save', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  /**
   * 读回策略包的蓝图文档（`blueprint.load`，#393）：编辑策略入口的预载读 ——
   * `blueprint.save` 的只读镜像。只保存过蓝图编译产物的包才有 blueprint.json，
   * 手写包会被内核以明确拒绝作答（原样展示，绝不编造图）。
   */
  blueprintLoad: (name: string) =>
    request<BlueprintLoadDoc>('/blueprint/load', {
      method: 'POST',
      body: JSON.stringify({ name }),
    }),
  /**
   * 读回手写包的入口源码（`blueprint.loadSource`）：编辑策略入口对**所有**
   * Lua 包开放的那一半 —— 没有 blueprint.json 的包从这条路打开，源码模式
   * 展示 Lua 与 manifest；内核按 manifest.entry 定位文件（同名字闸门）。
   */
  blueprintLoadSource: (name: string) =>
    request<BlueprintLoadSourceDoc>('/blueprint/load-source', {
      method: 'POST',
      body: JSON.stringify({ name }),
    }),
  /**
   * 手写包源码写回（`blueprint.saveSource`）：入口文件原样落盘，sha256 现场
   * 重算进 manifest（装载器照常验证字节）。编辑不创建：包必须已存在；内容
   * 与磁盘不同时必须显式 `overwrite`。
   */
  blueprintSaveSource: (body: BlueprintSaveSourceBody) =>
    request<BlueprintSaveSourceDoc>('/blueprint/save-source', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
}

// ── #362 蓝图编辑器（mirror core/blitzkrieg_core/src/ipc/schema.rs）──────────

/** `blueprint.compile` 应答：生成的 Lua 源码。 */
export interface BlueprintCompileDoc {
  lua: string
  error?: string
}

/** `blueprint.save` 的请求体（camelCase wire 与内核 serde 对齐）。 */
export interface BlueprintSaveBody {
  name: string
  json: string
  /** 同名策略包已存在时必须显式为 true。 */
  overwrite?: boolean
}

/** `blueprint.save` 回执：写了什么、写到哪、sha256 可对账。 */
export interface BlueprintSaveDoc {
  name: string
  packageDir: string
  blueprintPath: string
  luaPath: string
  manifestPath: string
  luaSha256: string
  /** blueprint/lua/manifest 三个文件的字节数。 */
  bytes: number[]
  /** 可进化旋钮名（manifest tunables 导出）——空数组表示本蓝图没有数值锚点。 */
  tunables: string[]
  error?: string
}

/** `blueprint.load` 应答（#393）：包里的蓝图文档原文 + 相对路径。 */
export interface BlueprintLoadDoc {
  name: string
  blueprintPath: string
  json: string
  error?: string
}

/** `blueprint.loadSource` 应答：手写包的入口源码 + manifest 原文。 */
export interface BlueprintLoadSourceDoc {
  name: string
  packageDir: string
  luaPath: string
  manifestPath: string
  lua: string
  /** manifest 里当前入口文件的 sha256，hex。 */
  sha256: string
  /** manifest 原文（任意 JSON），编辑器原样展示。 */
  manifest: unknown
  error?: string
}

/** `blueprint.saveSource` 请求体（camelCase wire 与内核 serde 对齐）。 */
export interface BlueprintSaveSourceBody {
  name: string
  lua: string
  /** 磁盘上已有不同源码时必须显式为 true。 */
  overwrite?: boolean
}

/** `blueprint.saveSource` 回执：写了什么、新的 sha256、字节数。 */
export interface BlueprintSaveSourceDoc {
  name: string
  packageDir: string
  luaPath: string
  manifestPath: string
  luaSha256: string
  /** lua/manifest 两个文件的字节数。 */
  bytes: number[]
  error?: string
}

// ── #353 回测类型（mirror core/blitzkrieg_core/src/backtest_jobs.rs 的 wire 形）──

/** `backtest.onchain.pull` 参数。钱包/时间窗必填；资产过滤可空 = 全量。 */
export interface OnchainPullParams {
  wallet: string
  /** `YYYY-MM-DD`（UTC，含尾日）或 epoch 秒。 */
  start: string
  end: string
  /** 空 = 整个钱包的交易。 */
  assets: string[]
  market?: string | null
}

/** `backtest.run` 参数。strategy 只传名字 — 任意脚本/代码在内核被拒。 */
export interface BacktestRunParams {
  /** 数据集路径（来自列表/拉取结果，限数据集根内）。 */
  archive: string
  /** `mine`（默认）| `verify` | `sweep` — #351 延迟阶梯。 */
  mode?: 'mine' | 'verify' | 'sweep' | string
  verifyLatencyMs?: number
  /** 诚实摩擦下限：≥1 tick，0/负数被拒。 */
  slippageTicks?: number
  tickMs?: number
  tailMs?: number
  strategies?: string[]
}

/** 一条数据集（`backtest.onchain.list` 的元素）。 */
export interface BacktestDataset {
  dataset: string
  eventsPath: string
  manifest: {
    wallet?: string
    window?: { startMs?: number; endMs?: number; startSec?: number; endSec?: number }
    counts?: { trades?: number; events?: number; conditions?: number; skippedRows?: number }
    files?: { trades?: string; events?: string }
    sha256?: { trades?: string; events?: string }
    generatedAtMs?: number
    [extra: string]: unknown
  }
}

/** `backtest.onchain.list` 应答。 */
export interface BacktestDatasetListDoc {
  outDir: string
  datasets: BacktestDataset[]
  error?: string
}

/** `backtest.onchain.pull` 应答：每资产一个任务。 */
export interface BacktestJobIdsDoc {
  jobIds: string[]
}

/** 任务状态（`backtest.status`）。done 时附带全量 `result`。 */
export interface BacktestJobStatusDoc {
  jobId: string
  /** `pull` | `backtest` */
  kind: string
  /** `running` | `done` | `failed` */
  state: string
  /** pull 的链上阶段（trades/markets/prices/convert/done）或 replay。 */
  phase: string
  detail: string
  progress: number
  startedAtMs: number
  finishedAtMs: number | null
  error: string | null
  result?: BacktestResultDoc
}

/** 阶梯一行（`backtest.result.ladder` 的元素）。 */
export interface BacktestLadderRow {
  latencyMs: number
  netPnlUsd: number
  closed: number
  winRatePct: number
  profitFactor: number | null
}

/** `backtest.result`：报告 + 阶梯 + 判读。`report` 即 #351 的 BacktestReport。 */
export interface BacktestResultDoc {
  mode: string
  archive: string
  verdict: string
  ladder: BacktestLadderRow[]
  report: BacktestReport
  reportPath: string
}

/** `backtest.export`：浏览器保存所需的文件名 + 内容。 */
export interface BacktestExportDoc {
  fileName: string
  content: string
}

// ── #364 生效风控（mirror core/blitzkrieg_core/src/ipc/schema.rs 的 wire 形）──

/** 八个条件字段（issue 清单逐字；serde snake_case 的 wire 拼写）。 */
export type PolicyConditionField =
  | 'available_balance' | 'total_equity' | 'open_positions' | 'current_price'
  | 'time_left_sec' | 'symbol' | 'recent_pnl_1h' | 'consecutive_losses'

/** 七个比较算子。GET 应答里是 snake_case（`ge`），set 也接受符号拼写（`>=`）。 */
export type PolicyOpWire = 'lt' | 'le' | 'gt' | 'ge' | 'eq' | 'ne' | 'in' | string

/** 五个 then 动作（封闭集；止损/熔断不在词汇表里 — 结构性锁）。 */
export type PolicyThenAction = 'skip' | 'budget_ratio' | 'min_budget_usd' | 'max_budget_usd' | 'cooldown_sec'

/** `when` 的表格拼写（字符串拼写在内核侧也接受，UI 统一发表格形）。 */
export interface PolicyWhenView {
  field: PolicyConditionField | string
  op: PolicyOpWire
  /** 数字字段为 number，symbol 为字符串或字符串数组（plain-value 拼写）。 */
  value: number | string | string[]
}

/** `then`：恰一个动作（内核 `deny_unknown_fields` 保证；UI 同样只填一个）。 */
export interface PolicyThenView {
  action?: PolicyThenAction
  budget_ratio?: string
  min_budget_usd?: string
  max_budget_usd?: string
  cooldown_sec?: number
}

/** 一条规则。 */
export interface PolicyRuleView {
  name: string
  priority: number
  enabled: boolean
  when: PolicyWhenView
  then: PolicyThenView
  reason?: string
}

/**
 * 一个账户段的生效视图（`execution_policy.get` 应答本体；list 里 `section`
 * 的元素形）。字段可能缺席（内核 skip_serializing_if）— 缺席 = 继承全局。
 */
export interface ExecutionPolicySectionDoc {
  budgetRatio?: string
  minBudgetUsd?: string
  maxBudgetUsd?: string
  minEquityUsd?: string
  maxPositionsPerAsset: number
  rules?: PolicyRuleView[]
}

/** `execution_policy.list` 应答：账户 chips 的来源。 */
export interface ExecutionPolicyListDoc {
  loaded: boolean
  /** 全局段摘要（chips 里固定的 `defaults`）；未加载策略文件时为 null。 */
  defaults: ExecutionPolicySectionDoc | null
  /** 文件里已命名的账户段，每项带自己的生效视图。 */
  accounts: { accountId: string; section: ExecutionPolicySectionDoc }[]
  error?: string
}

/** 预览一行：一笔已平仓交易按当前规则的判定。 */
export interface ExecutionPolicyPreviewRow {
  tsMs: number
  symbol: string
  /** 预览时刻的活账本余额（账本是账，不是历史磁带 — 内核明示）。 */
  balance: string | number
  price: string | number
  /** `place` | `skip` | `cooldown` | `refused`。 */
  verdict: string
  /** place 的预算金额，或 skip/cooldown 的原因。 */
  detail?: string
}

/** `execution_policy.preview` 应答：最近 N 笔的重放（N≤100）。 */
export interface ExecutionPolicyPreviewDoc {
  considered: number
  skipped: number
  avgBudgetUsd?: string
  rows: ExecutionPolicyPreviewRow[]
  error?: string
}

/** 一条审计记录（`execution_policy.history` 的元素）。before/after 是段摘要。 */
export interface ExecutionPolicyAuditLine {
  tsMs: number
  actor: string
  /** `set` | `reset` | `load-refused`。 */
  action: string
  accountId: string
  before?: ExecutionPolicySectionDoc | null
  after?: ExecutionPolicySectionDoc | null
  error?: string
}

/** `execution_policy.history` 应答：裸数组，按写入顺序（升序）的审计流水。 */
export type ExecutionPolicyHistoryDoc = ExecutionPolicyAuditLine[]

/** `execution_policy.set` 参数：账户段的可写字段（小数全部字符串过线）。 */
export interface ExecutionPolicySetParams {
  accountId: string
  budgetRatio?: string
  minBudgetUsd?: string
  maxBudgetUsd?: string
  minEquityUsd?: string
  maxPositionsPerAsset?: number
  rules?: PolicyRuleView[]
}

// ── 网络自检（mirror core/market_api + ui_kit core/types.rs + web/mod.rs 缓存）─

/** 一次探测：交易链路上的一条网络路径（解析 / TCP / TLS / 一次廉价请求）。 */
export interface NetCheckItem {
  /** `venue-rest` | `venue-ws` | `discovery` | `spot-ws` — 标识符，不是文案。 */
  name: string
  target: string
  ok: boolean
  /**
   * 机器判定：`ok` | `dns_failed` | `tcp_refused` | `tcp_timeout` | `tls_cert`
   * | `tls_error` | `timeout` | `http_error` | `transport_error` | `rejected`
   * | `unsupported`。文案由 `lib/net-check.ts` 给出，未知值原样显示、绝不隐藏。
   */
  status: string
  addrs?: string[]
  /** 解析到的地址全在代理的 fake-IP 段内 — 是解析器的事实，不是路径故障。 */
  fakeIp?: boolean
  ms?: number
  detail?: string
}

export interface NetCheckReport {
  /** 全部通过才为真；空报告与 `unsupported` 都不算通过。 */
  ok: boolean
  tsMs?: number
  /** `ok` | `tls_blocked` | `dns_failed` | `proxy_env` | `fake_ip` | `partial` | `unsupported`。 */
  hintCode?: string
  /** 内核给出的一句话判读。 */
  hint?: string
  /** 探测进程看到的代理变量**名字**（永不含值）。 */
  proxyEnv?: string[]
  items?: NetCheckItem[]
}

/** `GET /api/netcheck` 的响应：缓存里的结果 + 是否有一次探测在跑。 */
export interface NetCheckDoc {
  probing: boolean
  /** 结果年龄（毫秒）；从未探测过为 null。 */
  ageMs: number | null
  report: NetCheckReport | null
  /** 探测失败的原因（内核没应答 / 未上报）；成功为 null。 */
  error: string | null
}

/**
 * Adjudicate the CURRENT token with one authenticated call: `200` proves the
 * session works, `401` proves it does not, anything else means no verdict was
 * reachable. `ping` cannot answer this question — it is deliberately
 * sessionless, so its `authRequired` describes the gateway, not the token
 * (mapping the two was what made the 设置 badge claim 会话已过期 forever on
 * gateways that require login).
 */
export async function probeSession(): Promise<'valid' | 'expired' | 'unreachable'> {
  try {
    await api.snapshot()
    return 'valid'
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) return 'expired'
    return 'unreachable'
  }
}

// ── wire types (mirror ui_kit core/types.rs) ───────────────────────────────

export interface RejectionCauses {
  [bucket: string]: number
}

export interface StrategyStatsRow {
  name: string
  enabled: boolean
  source: string
  ordersPlaced: number
  ordersRejected: number
  limitRejected: number
  blockedTiming: number
  blockedMomentum: number
  gateExemptedTiming: number
  gateExemptedMomentum: number
  /** Gate names this strategy declared itself exempt from (`[]` when none). */
  gateExemptions: string[]
  /**
   * The `time_left_sec` floor on the `timing` exemption (D-31), or `null` when
   * the strategy left it to the kernel's `min_time_left_sec`. `null` is not the
   * same as `0`: it means "this opt-out still stops at the global window".
   */
  gateExemptionTimingFloorSec?: number | null
  closedTrades: number
  wins: number
  losses: number
  netPnlUsd: string | number
  rejectionCauses: RejectionCauses | null
  /** E27 (§8.3): declared modes as §2.3 wire objects; `null` = undeclared. Older cores omit. */
  modes?: unknown
  /** §8.3: `false` = no active plugin can satisfy any declared mode. Older cores omit. */
  compatible?: boolean
  /** §8.3: when `compatible === false`, the handshake's rendered refusal reason. */
  incompatibleReason?: string | null
  /**
   * 账户↔venue 解耦（本轮合同）：该策略当前在哪些账户 scope 上有实例
   * （engine 注册表读数，排序稳定）。全局口径（engine.stats fold /
   * strategy.list）每行带出真实归属；老内核缺字段 → undefined —— 启用按钮
   * 按「缺省降级」走全局开关语义，列表不再按账户过滤（列表恒等全部已安装）。
   */
  accounts?: string[]
  /**
   * 套利收益读数（与内核 fix/gate-account-slice 分支并行交付，纯
   * feature-detect）：cross_venue_arb 行（及未来其它套利策略）由内核带出
   * 的**两腿合计**口径 —— 净 PnL / 胜 / 负 / 平仓数两腿各计一次，与行自身
   * 的单侧计数（netPnlUsd/wins/losses/closedTrades）不同源。字段缺失
   * （老内核）= undefined —— 消费方必须回落行自身值，不得假设在场。
   */
  arbNetPnlUsd?: string | number
  /** 两腿合计的胜 / 负 / 平仓计数（arbNetPnlUsd 同批；缺失回落行自身值）。 */
  arbWins?: number
  arbLosses?: number
  arbClosedTrades?: number
  /** 当前配置里激活的套利币对数（两腿合计视角；缺失 = 不渲染徽标）。 */
  arbActivePairs?: number
}

export interface EngineStats {
  books?: number
  tops?: number
  spots?: number
  rounds?: number
  evaluations?: number
  signals?: number
  placeRejected?: number
  /** Orders the live venue refused (session-scoped). Older cores omit. */
  venueRejected?: number
  /**
   * The kernel's last recorded error — STRUCTURED (#180), and the source of the
   * panel's trading-error banner.
   *
   * One slot, one writer (`Core::note_error`): whichever path went wrong last,
   * it lands here. That is why the banner must not name a source in its title —
   * this record carries a venue refusal, a safety-net failure (reconcile sweep,
   * failed self-check) AND the kernel's own refusal of a leg.
   *
   * `code` is the `CoreErrorCode` the kernel classified it with
   * (`VENUE_ERROR`, `RISK_REJECTED`, `KILL_SWITCH_ACTIVE`, …) — the same
   * vocabulary a rejected RPC carries in `data.coreCode`, so a client that
   * branches on one can branch on the other.
   *
   * `undefined`/`null` means "no error recorded" (a healthy core, or one older
   * than #180 — an old core still sends `lastVenueError`).
   */
  lastError?: { tsMs: number; code: string; message: string } | null
  /**
   * LEGACY spelling of the SAME record (`<CODE>: <message>` pre-rendered).
   *
   * Read it only as the fallback for a core that predates `lastError`; new code
   * reads `lastError`. The kernel writes both from one `LastError`, so they
   * never disagree — one record, one instant, never a second, staler copy.
   */
  lastVenueError?: { tsMs: number; message: string } | null
  /**
   * E31-b: the reconcile sweep's consecutive-failure streak against the freeze
   * threshold. `consecutiveSweepFailures / freezeThreshold` is how close the
   * kernel is to freezing trading on its own. Older cores omit.
   */
  reconcile?: { consecutiveSweepFailures: number; freezeThreshold: number } | null
  /** Newest trading-capability self-check report. Older cores omit. */
  selfCheck?: {
    ok: boolean
    tsMs: number
    items: { name: string; ok: boolean; detail: string }[]
  } | null
  /** Trading freeze (kill switch) state. Older cores omit. */
  tradingFrozen?: { active: boolean; reason?: string } | null
  /**
   * The market-data event archive's status (issue #669), mirrored from the
   * kernel's `ArchiveStatus` (data_source.rs, camelCase on the wire). `null` =
   * no archive configured, or an older core. The stopped-recording banner
   * reads `recording` + `stoppedReason`.
   */
  archive?: {
    path: string
    events: number
    bytes: number
    dropped: number
    recording: boolean
    rotateBytes: number
    segmentBytes: number
    segments: number
    freeBytes: number
    stoppedReason?: string | null
  } | null
  confirmed?: string[]
  strategies?: StrategyStatsRow[]
}

/** Market plugin classes — the kernel's seven-value enum (market_api types.rs),
 * snake_case wire spelling. A value outside this union renders its raw string
 * (`marketTypeLabel` 的 `?? t` 兜底), so a future kernel value degrades, never mis-renders. */
export type MarketType =
  | 'prediction'
  | 'spot'
  | 'margin'
  | 'futures'
  | 'options'
  | 'earn'
  | 'bot'

export const MARKET_TYPE_LABELS: Record<MarketType, string> = {
  prediction: '二元预测市场',
  spot: '现货市场',
  margin: '杠杆现货',
  futures: '合约市场',
  options: '期货实现',
  earn: '生息市场',
  bot: '托管机器人',
}

export function marketTypeLabel(t?: string | null): string {
  if (!t) return ''
  return MARKET_TYPE_LABELS[t as MarketType] ?? t
}

export interface MarketPrice {
  asset: string
  up: number
  down: number
  /**
   * 账户↔venue 解耦（本轮合同）：该行情行所属的行情插件名（小写）。老内核
   * /网关类型化重组剥掉该字段时为 undefined —— 前端按「未分所」降级显示，
   * 不过滤、不崩（feature-detect，永不假设字段在）。
   */
  venue?: string
}

/** One price level of the live book (`engine.books`, E8-c 盘口深度). */
export interface BookLevel {
  price: number
  size: number
}

/**
 * One token's live book. Metric fields stay `null` until the feed has delivered
 * a book for the token — null means "no data", never a real quote.
 */
export interface BookSide {
  /** Best-first: bids descending by price, asks ascending. */
  bids: BookLevel[]
  asks: BookLevel[]
  bestBid: number | null
  bestAsk: number | null
  midPrice: number | null
  /** Order-book imbalance (bid − ask)/(bid + ask). */
  obi: number | null
  spread: number | null
  spreadPct: number | null
}

/** Per-asset L2 depth for the 盘口深度 chart. Older cores omit the field. */
export interface AssetBook {
  asset: string
  /** E29 (§12.2): the K-line series key for the UP/DOWN token. Absent on
   *  older cores — the K-line card hides rather than queries a wrong key. */
  upTokenId?: string
  downTokenId?: string
  /**
   * 账户↔venue 解耦（本轮合同）：该盘口行所属的行情插件名（小写）。老内核
   * 缺字段 → undefined，前端按「未分所」降级（不过滤、不崩）。
   */
  venue?: string
  up: BookSide
  down: BookSide
}

export interface Position {
  id: string
  asset: string
  direction: string
  entryPrice: number
  currentPrice: number
  unrealizedPct: number
  shares?: number
  strategy?: string
  remainingSec?: number
  /** #427: the venue the position's market listed on (older cores omit). */
  venue?: string
  /**
   * E28 (§9.2): the account the position settles into (gateway forwards the
   * kernel's own field). Empty/absent = a pre-E28 core — the row is
   * unattributed and the panel shows it regardless of the active account.
   */
  accountId?: string
}

export interface TradeRow {
  id: string
  strategy?: string
  asset: string
  direction: string
  entryPrice: number
  exitPrice: number
  shares: number
  netPnlUsd: number
  netPnlPct?: number
  feesUsd?: number
  exitReason?: string
  holdTimeSec?: number
  /** Milliseconds since epoch (older cores omit). */
  entryTime?: number
  exitTime?: number
  /** #427: the venue the trade's market listed on (older cores omit). */
  venue?: string
  /**
   * E28 (§9.2): the account the trade settled in. Empty/absent = pre-E28
   * record/core — unattributed, shown regardless of the active account.
   */
  accountId?: string
}

export interface TradeSummary {
  count: number
  net: number
  winRate: number
}

/** All-time closed-trade totals from the persisted summary (trades.summary). */
export interface CumulativeTradeSummary {
  totalTrades?: number
  wins?: number
  losses?: number
  winRate?: number
  totalGrossPnl?: number
  totalFees?: number
  totalNetPnl?: number
  avgHoldTimeSec?: number
  best?: number
  worst?: number
}

/** One venue's own round clock (`round.venueRounds` row, issue 550 症状②)：
 * 与 trading 侧 per-candidate 门同一口径的 per-venue slot/age/timeLeft。
 * `durationSec`（B1a 增量字段，轮次多族）：该钟行的轮长族（秒）；老内核
 * 缺席 → undefined，消费方 feature-detect 回落全局轮长。 */
export interface VenueRound {
  /** The venue tag (empty = legacy bucket). */
  venue: string
  slot: number
  ageSec: number
  timeLeftSec: number
  durationSec?: number
}

export interface Round {
  slot: number
  ageSec: number
  timeLeftSec: number
  canTrade: boolean
  markets?: number
  prices?: MarketPrice[]
  /**
   * Per-venue 轮次钟行（issue 550 症状②）：每个已开口的 venue 一行自己的钟。
   * 老内核没有该键 → undefined，倒计时 feature-detect 降级回全局标量块
   * （现行为）。
   */
  venueRounds?: VenueRound[]
}

export interface Snapshot {
  connected: boolean
  mode?: string
  lastError?: string | null
  stats?: EngineStats
  balance?: {
    balance: number
    reserved: number
    available: number
    /**
     * Starting principal. Present in DRY (the `--seed-balance` value); absent in
     * LIVE and on older cores — treat missing as "unknown", never as zero.
     */
    seed?: number | null
  } | null
  round?: Round | null
  positions?: Position[]
  /** E8-c 盘口深度: per-asset L2 depth (older cores omit → undefined). */
  books?: AssetBook[]
  trades?: TradeSummary
  tradeRows?: TradeRow[]
  /** All-time totals (older cores omit → fall back to tradeRows sums). */
  tradeSummary?: CumulativeTradeSummary | null
  strategies?: unknown[]
  extensions?: unknown[]
  marketPlugins?: PluginRow[]
  marketActiveName?: string | null
  marketActiveType?: MarketType | null
  /**
   * 显示账户（issue 559）——行情页身份行（倒计时卡的徽章/名/slot/已过秒）
   * 的唯一事实源，与「本面板会话默认账户」解耦：账户切换器切的是交易视
   * 图，行情显示的是谁由内核的 display_account 决定（accounts.toml 顶层
   * 持久化 + 运行时生效，写动词 display.account.set）。快照缺该字段（老
   * 内核）= 回落账户绑定链派生，现行为不变。
   */
  displayAccount?: string | null
  /** displayAccount 绑定的 venue 读数；空串 = 未绑定（可交易任意交易所）。 */
  displayVenue?: string | null
  /** Venue wallet identity — null in dry mode (local seed cash, not venue funds). */
  wallet?: { signer: string | null; funder: string | null }
  /**
   * What the gateway serving this snapshot can do about the core *process*.
   *
   * Absent on a read-only adapter (`ui_kit_web` without a dispatcher) and on
   * older gateways. Absence must be read as "cannot control the lifecycle", so
   * treat a missing block as disabled rather than falling back to enabled.
   */
  gateway?: {
    /** This gateway accepts `start`/`stop` — it was started with `--manage`. */
    lifecycleEnabled: boolean
    /** This gateway spawned the core, so it can also stop it. */
    managed: boolean
    /** PID of the core this gateway spawned; null when the core was adopted. */
    corePid: number | null
    socket: string
    /**
     * How many times this gateway has replaced its own core after a crash
     * (E12-c). Absent on a gateway that predates crash reporting.
     */
    restarts?: number
    /** The restart budget is spent: the core is down and will stay down. */
    restartGivenUp?: boolean
    /**
     * Why the last core this gateway owned stopped. `kind` separates a crash
     * from a stop the operator asked for, so the panel can say which happened
     * instead of inferring it from a missing pid.
     */
    lastExit?: {
      pid: number
      kind: 'crash' | 'clean'
      code: number | null
      signal: number | null
      /** Operator-readable one-liner from the core's own classification. */
      description: string
    } | null
  } | null
  strategyStats?: StrategyStatsRow[]
  /**
   * VERSIONING.md §5 — version + build provenance + update state of the
   * serving core. `null`/absent on older gateways or cores: read as
   * "unavailable", never as a fabricated version (§5.6).
   */
  systemVersion?: SystemVersion | null
  /**
   * #379 — the kernel's download/staging state machine. `null`/absent on
   * older cores: read as "this core cannot stage", never invented.
   */
  stageState?: StageState | null
  /**
   * E13 evolution block — pending proposals + the switch/cycle clock. Absent on
   * older gateways (treat as "no proposal workflow"); never present with a
   * partial status: the gateway always writes both keys together.
   */
  evolution?: EvolutionDoc | null
}

/**
 * `system.version` (VERSIONING.md §5.3): the serving core's self-description.
 * `updateAvailable` is THREE-STATE — `null` = not checked (checks are OFF by
 * default), which must never be rendered as "up to date" (INV-3).
 */
export interface SystemVersion {
  version: string
  gitHash: string
  gitDirty: boolean
  buildDate: string
  target: string
  updateAvailable: boolean | null
  latestVersion: string | null
  autoUpdate: boolean
  checkEnabled: boolean
  lastCheckMs: number | null
  releaseUrl: string | null
  /**
   * 上次检查为什么失败，内核原话。只有检查真的失败时才非 null —— 「未检查」
   * 与「检查通过」都是 null。旧内核没有这个键 → undefined，渲染为无原因。
   */
  lastCheckError?: string | null
  /**
   * 新内核把暂存状态机附在 system.version 里返回：快照轮询由此读暂存状态，
   * 不必再碰 system.update.stage（那是下载动词）。旧内核没有这个键。
   */
  stage?: StageState | null
}

/**
 * 内核的下载/暂存状态机（VERSIONING.md §7.5，#379）。四相，最高只能到
 * `staged` —— 内核永不替换它正在执行的二进制（§7.5 铁律），安装是启动器的
 * 职责。`phase` 用内核自己的 token（idle/downloading/staged/failed）；旧内核
 * 不认识 system.update.stage → snapshot 上是 null，渲染为「不支持暂存」。
 */
export interface StageState {
  phase: 'idle' | 'downloading' | 'staged' | 'failed'
  detail?: string | null
  version?: string | null
  asset?: string | null
  sha256?: string | null
  stagedAtMs?: number | null
}

export interface PluginRow {
  name: string
  kind?: string
  /** market.list calls the identity field `type` (camelCase wire). */
  type?: string
  /**
   * Provenance, when the source carries it (`engine.stats` rows do:
   * `"dylib:<path>"`). `strategy.list` / `/api/plugins` does NOT, so a strategy
   * row from there has no `source` — report that as unknown rather than guessing
   * (see `lib/strategy-source.ts`).
   */
  source?: string
  description?: string
  enabled?: boolean
  status?: string
  /**
   * 账户↔venue 解耦合同（strategy.list 行）：该策略在哪些账户 scope 启用。
   * 老内核缺字段 → undefined —— 前端按「缺省降级」走全局开关语义，不发明。
   */
  accounts?: string[]
  /** market.list: this source is the currently selected one. */
  active?: boolean
  /** market.list capability flags — a plugin may implement only some of them. */
  hasDataFeed?: boolean
  hasDiscovery?: boolean
  hasExecutor?: boolean
  /**
   * 插件自声明的可发现轮次族（秒；supported_round_secs）。undefined =
   * 未声明 → 面板不筛（predictfun/模拟盘一类无轮次概念所）。轮次多族：
   * 账户周期集编辑器以此隐藏该所服务不了的档位。
   */
  supportedRoundSecs?: number[]
  // ── E27 (§8.3) mode declaration fields — market.list rows ────────────────
  /**
   * The concrete structure the plugin declares, when its `declare_modes()`
   * names the SAME concrete structure unanimously; null/absent = unspecified
   * (undeclared / multi-structure / structure-less plugins).
   */
  structure?: string | null
  /** Union of the declared capability bits (raw value, for cross-checking). */
  capabilitiesBits?: number
  /** Readable capability names, in bit order (`websocket_feed`, …). */
  capabilities?: string[]
  // ── E27 (§8.3) mode declaration fields — strategy.list rows ──────────────
  /** The strategy's declared modes (§2.3 wire objects); null = undeclared. */
  modes?: unknown
  /** False only when the handshake would refuse this strategy. */
  compatible?: boolean
  /** The refusal reason when incompatible, else null. */
  incompatibleReason?: string | null
  /** extension.list lifecycle state, e.g. "installed". */
  state?: string
}

/** Identity badge for a plugin row (binary prediction/spot/futures/options). */
export function pluginKindLabel(row: PluginRow): string {
  const kval = row.type ?? row.kind
  return marketTypeLabel(kval) || kval || '—'
}

export interface PluginsDoc {
  connected: boolean
  strategies: PluginRow[]
  extensions: PluginRow[]
  marketPlugins: PluginRow[]
  /** Boolean flag — the selected source's *name* is on the active market row. */
  marketActive: boolean
  lastError: string | null
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

// ── E13 evolution (mirror web/mod.rs's `evolution` block) ────────────────────

/** One side of a proposal's 对比表. Decimals cross as numbers here. */
export interface EvolutionMetricsRow {
  closed?: number
  wins?: number
  winRate?: number
  payoff?: number
  profitFactor?: number
  netPnlUsd?: number
}

export type EvolutionState =
  | 'proposed' | 'deferred' | 'accepted' | 'rejected' | 'expired' | 'superseded'

/**
 * Why a decided proposal ended the way it did (#251). Tagged by `kind`, so a
 * guard refusal carries the lock that refused it plus the guard's own words.
 * Absent on rows decided before the core wrote the field — render those as
 * "未上报" rather than guessing.
 */
export type EvolutionDecisionReason =
  | { kind: 'guardFailed'; guard: string; detail: string }
  | { kind: 'rejected' }
  | { kind: 'expired' }
  | { kind: 'superseded'; byId: string }

/** One evolution proposal row, folded by the gateway for rendering. */
export interface EvolutionProposalRow {
  id: string
  strategy: string
  state: EvolutionState | string
  /** Why the evaluator held it: higher_win_rate / better_profit_factor / combined_improvement. */
  reason: string
  confidence: number
  sampleCount: number
  dims: string[]
  /** Ordered `[name, from, to]` triples (string decimals). */
  knobMoves: [string, string, string][]
  baseline: EvolutionMetricsRow
  variant: EvolutionMetricsRow
  createdAtMs: number
  expiresAtMs: number
  decidedBy: 'user' | 'auto' | null
  decidedAtMs: number | null
  decidedReason?: EvolutionDecisionReason | null
  cycleSeq: number
}

export interface EvolutionStatusRow {
  /** The engine switch: false means nothing is evaluated at all (#249). */
  enabled: boolean
  autoEvolve: boolean
  lastCycleMs: number
  nextCycleAtMs: number | null
  pendingProposals: number
  /** Completed deep rounds; 0 = the first round has not fired yet. */
  cycleSeq: number
  /** Configured seconds between deep rounds (reported, never assumed). */
  cycleSecs: number
}

export interface EvolutionDoc {
  proposals: EvolutionProposalRow[]
  status: EvolutionStatusRow | null
}
