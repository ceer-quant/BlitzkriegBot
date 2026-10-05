/**
 * Gateway API client for the Blitzkrieg panel.
 *
 * Auth: user/password login (`POST /api/login`, set via
 * `BLITZKRIEG_PANEL_USER`/`BLITZKRIEG_PANEL_PASSWORD` on the server) issues a
 * session token kept in localStorage; every /api call carries it as
 * `X-Auth-Token`. A `?token=…` link is also accepted (session hand-off).
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
}

/** The whole `risk.limits` readout. */
export interface RiskLimitsDoc {
  version: string
  account: { id: string; limits: AccountRiskLimitsView }
  global: { limits: GlobalRiskLimitsView }
  exit: RiskExitView
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
   * a boot-time snapshot without the Core lock).
   */
  riskLimits: () => request<RiskLimitsDoc>('/risk-limits'),
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

  /**
   * 设置页「检查更新」按钮（VERSIONING.md §6.3）。网关只负责把请求转给内核并
   * 立即返回 —— 内核拨 GitHub 以秒计，网关的单线程接受循环绝不等它。轮询
   * snapshot 的 `systemVersion` 直到 `lastCheckMs` 变化；`error` 非空时是内核
   * 明确拒绝（例如 checkEnabled=false），必须原样展示，不许静默。
   */
  updateCheck: () => request<{ started: boolean; running?: boolean; error?: string }>('/version/check', { method: 'POST' }),
  /**
   * 设置页「自动更新」开关（VERSIONING.md §7.4）。内核是开关的唯一事实来源：
   * 它落盘 + 落审计后回显新值；写盘失败必须报错（静默回退的开关比没有更糟）。
   */
  updateConfigure: (autoUpdate: boolean) =>
    request<{ ok: boolean; autoUpdate?: boolean; error?: string }>('/version/configure', {
      method: 'POST',
      body: JSON.stringify({ autoUpdate }),
    }),
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
  /** E29 (§12.2): the K-line series key for the UP/DOWN token. Absent on
   *  older cores — the K-line card hides rather than queries a wrong key. */
  upTokenId?: string
  downTokenId?: string
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
  /** market.list: this source is the currently selected one. */
  active?: boolean
  /** market.list capability flags — a plugin may implement only some of them. */
  hasDataFeed?: boolean
  hasDiscovery?: boolean
  hasExecutor?: boolean
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
