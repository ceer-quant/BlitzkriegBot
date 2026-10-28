/**
 * Snapshot domain — the `/api/snapshot` + `/api/plugins` wire (mirror ui_kit
 * core/types.rs) and the engine/plugin readout types the whole panel renders.
 */
import { request } from './core.ts'

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
   * The market-data event archive's status (issue 669), mirrored from the
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
  /**
   * 挂单页签的终态死单分流进历史页签时（面板合成行，从未成交、无对应
   * trades 行）由前端写入的来源标记：原内核订单的 status（REJECTED/
   * FAILED/FORCE_CLOSED/CANCELLED）。真实成交行永远没有这个键 —— 与
   * 「来源 = 订单」徽标和净利占位（—，不进盈亏统计）同源判定。
   */
  orderStatus?: string
  /** 合成行的轮次（内核 TrackedOrder.roundSlot），真实成交行缺省。 */
  orderRoundSlot?: number
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

// ── 版本/暂存读数（VERSIONING.md；快照顶层的 systemVersion/stageState）──────

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

export const snapshotApi = {
  snapshot: () => request<Snapshot>('/snapshot'),
  plugins: () => request<PluginsDoc>('/plugins'),
}
