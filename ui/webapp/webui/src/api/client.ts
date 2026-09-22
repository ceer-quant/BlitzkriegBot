/**
 * Gateway API client for the Blitzkrieg panel.
 *
 * Auth: user/password login (`POST /api/login`, set via
 * `BLITZKRIEG_PANEL_USER`/`BLITZKRIEG_PANEL_PASSWORD` on the server) issues a
 * session token kept in localStorage; every /api call carries it as
 * `X-Auth-Token`. A `?token=…` link is also accepted (session hand-off).
 */

const LS_KEY = 'blitzkrieg-panel-token'
/** When the current session token was issued (login click), for the 设置 page. */
export const LOGIN_AT_KEY = 'blitzkrieg-panel-login-at'

/** When the current session was issued, or null when unknown/pre-restart. */
export function loginAt(): number | null {
  const raw = localStorage.getItem(LOGIN_AT_KEY)
  const n = raw ? Number(raw) : 0
  return Number.isFinite(n) && n > 0 ? n : null
}

let token = (() => {
  const q = new URLSearchParams(window.location.search).get('token')
  if (q) {
    localStorage.setItem(LS_KEY, q)
    history.replaceState(null, '', window.location.pathname)
    return q
  }
  return localStorage.getItem(LS_KEY) ?? ''
})()

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

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
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
  [extra: string]: unknown
}

export const api = {
  snapshot: () => request<Snapshot>('/snapshot'),
  plugins: () => request<PluginsDoc>('/plugins'),
  /** Dispatch a gateway command verb (`status`/`start`/`stop`/…). */
  command: (cmd: string) =>
    request<CommandDoc>('/command', { method: 'POST', body: cmd }),
  /** Strategy enable toggle (`strategy <name> on|off` on the core). */
  setStrategy: (name: string, enabled: boolean) =>
    request<CommandDoc>('/command', {
      method: 'POST',
      body: `strategy ${name} ${enabled ? 'on' : 'off'}`,
    }),
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
  confirmed?: string[]
  strategies?: StrategyStatsRow[]
}

/** Market plugin classes — binary prediction market / spot / futures / options. */
export type MarketType = 'prediction' | 'spot' | 'futures' | 'options'

export const MARKET_TYPE_LABELS: Record<MarketType, string> = {
  prediction: '二元预测市场',
  spot: '现货市场',
  futures: '合约市场',
  options: '期货实现',
}

export function marketTypeLabel(t?: string | null): string {
  if (!t) return ''
  return MARKET_TYPE_LABELS[t as MarketType] ?? t
}

export interface MarketPrice {
  asset: string
  up: number
  down: number
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

export interface Round {
  slot: number
  ageSec: number
  timeLeftSec: number
  canTrade: boolean
  markets?: number
  prices?: MarketPrice[]
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
   * E13 evolution block — pending proposals + the switch/cycle clock. Absent on
   * older gateways (treat as "no proposal workflow"); never present with a
   * partial status: the gateway always writes both keys together.
   */
  evolution?: EvolutionDoc | null
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
  /** market.list: this source is the currently selected one. */
  active?: boolean
  /** market.list capability flags — a plugin may implement only some of them. */
  hasDataFeed?: boolean
  hasDiscovery?: boolean
  hasExecutor?: boolean
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
  /**
   * Switches where the shipped file and the persisted runtime state disagree
   * (#269). The persisted value wins, so editing the file is not a kill switch;
   * empty is the normal case.
   */
  switchConflicts: EvolutionSwitchConflictRow[]
}

export interface EvolutionSwitchConflictRow {
  /** `'engine'` (is the evaluator running) or `'autoEvolve'` (who applies it). */
  switch: string
  /** What `user_layer/configs/shadow_evolution.toml` says. */
  fileValue: boolean
  /** What `data/evolution/state.json` says — the value actually in force. */
  runtimeValue: boolean
}

export interface EvolutionDoc {
  proposals: EvolutionProposalRow[]
  status: EvolutionStatusRow | null
}
