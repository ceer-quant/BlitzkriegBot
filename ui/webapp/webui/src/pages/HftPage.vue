<script setup lang="ts">
/**
 * 行情面板 — 二元预测市场通用模板页（源自 ui/hft.html）。倒计时条、行情卡、
 * 权益曲线、胜率分布、交易统计、持仓/历史瀑布流（全量、过滤器、分页），
 * 以及 hft.html 的提示音（盈/亏/开仓/熔断）。数据全部来自 /api/snapshot。
 */
import { computed, onUnmounted, ref, watch } from 'vue'
import { useIntervalFn, useIntersectionObserver } from '@vueuse/core'
import { Activity, Play, Square, Bell, BellOff, Search, X, TrendingUp, TrendingDown, Clock, Filter, Info } from 'lucide-vue-next'
import { api, marketTypeLabel, type MarketPrice, type Position, type TradeRow, type AssetBook, type BookSide, type BookLevel, type OrderRow } from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import { useSettingsStore } from '@/stores/settings'
import { useTheme } from '@/lib/theme'
import {
  num, money, signedMoney, winRatePct, pct, signedPct, cents, mmss, duration, dateTime,
} from '@/lib/format'
import { balanceView } from '@/lib/balance'
import { filterByAccount, filterByVenue } from '@/lib/account-filter'
import { newestFirst, tradeIdentity } from '@/lib/trades'
import { controlState, exitNotice } from '@/lib/lifecycle'
import { feedStaleness } from '@/lib/feed'
import {
  playDang, playDing, playOrder, playProfit, playWuwu,
} from '@/composables/alertSounds'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import StatRow from '@/components/ui/stat/StatRow.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import EquityCurve from '@/components/charts/EquityCurve.vue'
import BookDepth from '@/components/charts/BookDepth.vue'
import KlineChart from '@/components/charts/KlineChart.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import ConfirmWordDialog from '@/components/ui/confirm/ConfirmWordDialog.vue'

const store = usePanelStore()
const settings = useSettingsStore()
const { sound, toggleSound } = useTheme()
const snap = computed(() => store.snapshot)

// Fast tick: 2s snapshot poll (HFT pacing), faster than the shell's 15s.
// Pacing is a persisted preference (E8-d 设置); off turns this tab watch-only.
const { pause: stopFast } = useIntervalFn(() => {
  if (settings.fastPoll) void store.refresh()
}, 2_000)
onUnmounted(() => { stopFast() })

// ── alert sounds: diff each snapshot against the previous one ────────────────
let primedWins: number | null = null
let primedLosses: number | null = null
let primedPositions: number | null = null
let wasHalted = false

watch(snap, (s) => {
  if (!s) return
  // Deduped: the raw rows can carry the same `hft-N` id from an earlier run, and
  // counting those twice would fire an alert sound for a trade that never closed.
  const rows = store.tradeRows
  const wins = rows.filter((t) => (Number(t.netPnlUsd) || 0) > 0).length
  const losses = rows.filter((t) => (Number(t.netPnlUsd) || 0) <= 0).length
  const pos = s.positions?.length ?? 0
  // first frame only establishes the baseline (no replay storm on reload)
  if (primedWins === null || primedLosses === null || primedPositions === null) {
    primedWins = wins; primedLosses = losses; primedPositions = pos
    return
  }
  if (wins > primedWins) for (let i = 0; i < Math.min(wins - primedWins, 3); i++) playProfit()
  if (losses > primedLosses) for (let i = 0; i < Math.min(losses - primedLosses, 3); i++) playDang()
  if (pos > primedPositions) playOrder()
  primedWins = wins; primedLosses = losses; primedPositions = pos

  // Trading halted: the sound's source is the kernel's freeze state
  // (`stats.tradingFrozen`), not `snapshot.lastError` — that one is the
  // GATEWAY's "core not reachable" string, which never contains "breaker", so
  // this pair of sounds could never fire (issue 236 shook this loose). The
  // daily-loss breaker is a DIFFERENT signal (`dailyLoss.tripped`, still not
  // carried through the seam) and would need its own line here if the panel
  // should hear it too.
  const halted = s.stats?.tradingFrozen?.active === true
  if (halted && !wasHalted) playWuwu()
  if (!halted && wasHalted) playDing()
  wasHalted = halted
})

// ── market identity ─────────────────────────────────────────────────────────
const marketName = computed(() => snap.value?.marketActiveName ?? null)
const marketTypeText = computed(() => marketTypeLabel(snap.value?.marketActiveType ?? null))
const isDry = computed(() => (snap.value?.mode ?? 'dry') === 'dry')

// ── countdown: interpolate locally between server answers ────────────────────
const serverLeft = ref(0)
const serverAt = ref(0)
watch(snap, (s) => {
  if (s?.round && s.round.timeLeftSec >= 0) {
    serverLeft.value = s.round.timeLeftSec
    serverAt.value = Date.now()
  }
}, { immediate: true })

const { pause: stopTick } = useIntervalFn(() => {
  leftSec.value = Math.max(0, Math.round(serverLeft.value - (Date.now() - serverAt.value) / 1000))
}, 500)
onUnmounted(() => { stopTick() })
const leftSec = ref(0)

const round = computed(() => snap.value?.round ?? null)
const leftText = computed(() => mmss(leftSec.value))
/** Fraction of the round still to run — drives the progress ring/bar. */
const leftFraction = computed(() => {
  const total = (round.value?.ageSec ?? 0) + (round.value?.timeLeftSec ?? 0)
  if (!total) return 0
  return Math.max(0, Math.min(1, leftSec.value / total))
})
const urgent = computed(() => leftSec.value > 0 && leftSec.value <= 60)

const prices = computed<MarketPrice[]>(() => round.value?.prices ?? [])
function spreadCents(m: MarketPrice): number {
  return Math.max(0, m.up + m.down - 1) * 100
}

// ── 盘口深度 (E8-c, engine.books) ───────────────────────────────────────────
const books = computed<AssetBook[]>(() => snap.value?.books ?? [])
/**
 * Whether THIS core serves depth at all: the field is absent on cores older
 * than the engine.books verb, and that must read differently from "the feed
 * hasn't delivered a book yet" — otherwise an old core's panel shows an
 * eternally loading chart instead of a stated capability boundary.
 */
const hasDepthData = computed(() => snap.value?.books !== undefined)

const depthAsset = ref('')
watch(books, (b) => {
  if (!b.length) return
  if (!b.some((x) => x.asset === depthAsset.value)) depthAsset.value = b[0].asset
}, { immediate: true })

/** UP or DOWN token of the selected asset — the two halves of the round. */
const depthToken = ref<'up' | 'down'>('up')

const activeBook = computed(() => books.value.find((b) => b.asset === depthAsset.value) ?? null)
const activeSide = computed<BookSide | null>(() => {
  const b = activeBook.value
  if (!b) return null
  return depthToken.value === 'up' ? b.up : b.down
})

// ── K线 (E29, kline.history) ─────────────────────────────────────────────────
/**
 * The aggregator keys prediction-token bars by TOKEN id (what `feed_klines`
 * was fed), not by asset name — the same key the books view now exposes.
 * Empty on older cores: the card hides rather than queries a wrong key.
 *
 * K线只属于 CEX 现货/合约：二元预测市场（prediction）的价格就是 0..1 概率，
 * 没有 OHLC 概念，卡片必须藏——哪怕行情插件带了真 token id。
 */
const KLINE_MARKET_TYPES = new Set(['spot', 'futures', 'options'])
const klineReady = computed(() => {
  if (!KLINE_MARKET_TYPES.has(snap.value?.marketActiveType ?? '')) return false
  const b = activeBook.value
  if (!b) return false
  return depthToken.value === 'up' ? !!b.upTokenId : !!b.downTokenId
})
const klineSymbol = computed(() => {
  const b = activeBook.value
  if (!b) return ''
  return depthToken.value === 'up' ? b.upTokenId : b.downTokenId
})

const depthMetrics = computed(() => {
  const s = activeSide.value
  if (!s) return null
  const sum = (xs: BookLevel[]) => xs.reduce((a, l) => a + (Number(l.size) || 0), 0)
  return {
    bestBid: s.bestBid,
    bestAsk: s.bestAsk,
    spread: s.spread,
    obi: s.obi,
    bidDepth: sum(s.bids),
    askDepth: sum(s.asks),
  }
})

/**
 * Feed liveness, so a stalled market-data feed cannot masquerade as a quiet market.
 *
 * The prices below are whatever the core last received; it keeps serving them
 * indefinitely. `feedStaleness` watches the orderbook counters (the only signal
 * that moves *with* the feed — see `lib/feed.ts`) and this page states the outage
 * instead of quoting stale prices as current.
 *
 * `nowMs` is ticked locally rather than read from the snapshot, so the banner
 * appears on its own a few seconds after the feed dies instead of waiting for the
 * poll that would have revealed it.
 */
const nowMs = ref(Date.now())
useIntervalFn(() => { nowMs.value = Date.now() }, 2000)
const feed = computed(() => feedStaleness(store.feedAt, nowMs.value))

// ── cumulative all-time totals (core summary preferred, rows as fallback) ────
// E28 (§9.5): the history is scoped to the session's default account —
// `accountFilter.rows` is the account's slice of the log (unattributed rows
// from a pre-E28 core stay visible, see lib/account-filter). Every
// history-derived view (cumulative header, equity curve, filters, stats)
// reads the filtered list so the page cannot mix two accounts in one number.
const accountFilter = computed(() => filterByAccount(store.tradeRows, store.activeAccount))
// venue 层（账户绑定交易所的下游过滤）：账户切换器就是交易所切换器 ——
// active 账户绑定了 venue X 时，页面只显示 X 的行；未绑定 = 可交易任意
// 交易所，页面也就显示全部。这层过滤按行数据的 venue 字段（issue 427）走，
// 与 accountId 过滤互补：pre-E28 无归属的旧行同样能被 venue 层过滤到。
const activeVenue = computed(() => store.activeAccountVenue)
const historyRows = computed<TradeRow[]>(() =>
  filterByVenue(accountFilter.value.rows, activeVenue.value).rows,
)
const cumStats = computed(() => {
  const s = snap.value?.tradeSummary
  const rows = historyRows.value
  if (s && (s.totalTrades ?? 0) > 0) {
    return {
      total: s.totalTrades ?? rows.length,
      gross: s.totalGrossPnl ?? 0,
      fees: s.totalFees ?? 0,
      net: s.totalNetPnl ?? 0,
      wins: s.wins ?? 0,
      losses: s.losses ?? 0,
      winRate: winRatePct(s.winRate),
      avgHold: s.avgHoldTimeSec,
      best: s.best,
      worst: s.worst,
      fromSummary: true,
    }
  }
  const net = rows.reduce((x, t) => x + (Number(t.netPnlUsd) || 0), 0)
  const wins = rows.filter((t) => (Number(t.netPnlUsd) || 0) > 0).length
  const pcts = rows.map((t) => Number(t.netPnlPct) || 0)
  const fees = rows.reduce((x, t) => x + (Number(t.feesUsd) || 0), 0)
  return {
    total: rows.length,
    // `netPnlUsd` is already after fees, so gross is net plus the fees back —
    // NOT net itself, which would print the post-cost figure under a pre-cost
    // label and make the cost invisible.
    gross: net + fees,
    fees,
    net,
    wins,
    losses: rows.length - wins,
    winRate: rows.length ? (wins / rows.length) * 100 : 0,
    avgHold: undefined as number | undefined,
    best: pcts.length ? Math.max(...pcts) : undefined,
    worst: pcts.length ? Math.min(...pcts) : undefined,
    fromSummary: false,
  }
})

// ── balance (本金 + 净利 is the real balance; cash is the ledger it commits against)
const recon = computed(() => balanceView(snap.value?.balance, cumStats.value.net, cumStats.value.fees))
// E28 多账户的诚实标注：瓦片的现金/本金读数来自内核进程级活动账本（§9.5
// gateway 走 active_ledger），不是会话切换账户的自有账本——多账户下两者可以
// 不同，标签必须说出来而不是让两套数字冒充一套。单账户（<=1 行）不加后缀，
// 与 0.2 部署逐字一致。账户级余额瓦片（读 account.list 行）等内核暴露
// 会话级账本读数后再换源（wallet 重构批次）。
const balanceAccountNote = computed(() => {
  const id = store.activeAccount
  if (!id || store.accounts.length <= 1) return ''
  return ` 当前读数属于进程默认账户「${id}」（本面板连接的 §9.5 会话级默认）；切换账户不会改写这个账本的读数。`
})
const balanceAccountSuffix = (label: string): string => {
  const id = store.activeAccount
  if (!id || store.accounts.length <= 1) return label
  return `${label} · ${id}`
}

// ── history filters ─────────────────────────────────────────────────────────
const fTime = ref<'all' | 'today' | '7d'>('all')
const fAsset = ref('all')
const fOutcome = ref<'all' | 'win' | 'loss'>('all')
const fStrategy = ref('all')

const assetOptions = computed(() => [...new Set(historyRows.value.map((t) => t.asset).filter(Boolean))].sort())
const strategyOptions = computed(() =>
  [...new Set(historyRows.value.map((t) => t.strategy ?? '').filter(Boolean))].sort(),
)
const hasFilters = computed(
  () => fTime.value !== 'all' || fAsset.value !== 'all' || fOutcome.value !== 'all' || fStrategy.value !== 'all',
)
function resetFilters(): void {
  fTime.value = 'all'; fAsset.value = 'all'; fOutcome.value = 'all'; fStrategy.value = 'all'
}

const filteredRows = computed<TradeRow[]>(() => {
  const now = Date.now()
  const dayStart = new Date(new Date().setHours(0, 0, 0, 0)).getTime()
  const week = now - 7 * 86_400_000
  const kept = historyRows.value.filter((t) => {
    const ts = t.exitTime ?? t.entryTime ?? 0
    if (fTime.value === 'today' && ts < dayStart) return false
    if (fTime.value === '7d' && ts < week) return false
    if (fAsset.value !== 'all' && t.asset !== fAsset.value) return false
    if (fStrategy.value !== 'all' && (t.strategy ?? '') !== fStrategy.value) return false
    const pnl = Number(t.netPnlUsd) || 0
    if (fOutcome.value === 'win' && pnl <= 0) return false
    if (fOutcome.value === 'loss' && pnl > 0) return false
    return true
  })
  // Newest first for display; the totals below are order-independent.
  return newestFirst(kept)
})

const filteredStats = computed(() => {
  const rows = filteredRows.value
  const net = rows.reduce((a, t) => a + (Number(t.netPnlUsd) || 0), 0)
  const wins = rows.filter((t) => (Number(t.netPnlUsd) || 0) > 0).length
  return { count: rows.length, net, wins, losses: rows.length - wins }
})

// ── waterfall pagination (30 per page, sentinel auto-loads) ─────────────────
const PAGE = 30
const visibleCount = ref(PAGE)
const sentinel = ref<HTMLElement | null>(null)
const hasMore = computed(() => visibleCount.value < filteredRows.value.length)
const visibleRows = computed(() => filteredRows.value.slice(0, visibleCount.value))

// Rewind to the first page when the FILTER changes — never when the data does.
// Watching `filteredRows` looked equivalent but is not: it is derived from the
// snapshot, and the 2s poll hands it a fresh array identity every tick, so the
// watcher fired on every poll and sent the operator's page count back to 30
// seconds after they clicked 加载更多.
watch([fTime, fAsset, fOutcome, fStrategy], () => { visibleCount.value = PAGE })

useIntersectionObserver(sentinel, ([entry]) => {
  if (entry?.isIntersecting && hasMore.value) visibleCount.value += PAGE
}, { rootMargin: '200px' })

// ── win-rate band distribution ──────────────────────────────────────────────
const winBands = computed(() => {
  const rows = historyRows.value
  const bands = [
    { label: '≥ +$1.00', tone: 'up' as const, test: (v: number) => v >= 1 },
    { label: '$0 – $1', tone: 'up' as const, test: (v: number) => v >= 0.01 && v < 1 },
    { label: '–$1 – $0', tone: 'down' as const, test: (v: number) => v <= -0.01 && v > -1 },
    { label: '≤ –$1.00', tone: 'down' as const, test: (v: number) => v <= -1 },
  ]
  const counts = bands.map((b) => rows.filter((t) => b.test(Number(t.netPnlUsd) || 0)).length)
  const max = Math.max(...counts, 1)
  return bands.map((b, i) => ({ ...b, count: counts[i], frac: counts[i] / max }))
})

// ── engine control ──────────────────────────────────────────────────────────
const busy = ref(false)
const cmdMsg = ref<string | null>(null)

/**
 * Whether the engine is up.
 *
 * `connected` is the gateway's "core is reachable on the socket" flag, and the
 * core *is* the engine — 启动 spawns it, 停止 kills it — so reachability is the
 * running state. It stays accurate on a failed poll too: the web adapter answers
 * 200 with `connected: false` rather than erroring, so the store never keeps a
 * stale `true`.
 */
const engineUp = computed(() => snap.value?.connected ?? false)

/**
 * What the two controls can actually do here.
 *
 * Reachability alone is not enough to offer a live 停止: a gateway started
 * without `--manage` refuses both verbs, and even with `--manage` it only stops
 * a core it spawned itself (`Supervisor::stop` leaves an adopted core running).
 * `controlState` folds those in so the buttons are disabled with a stated
 * reason instead of being clickable and failing.
 */
const control = computed(() => controlState(snap.value?.gateway, engineUp.value))

/**
 * The last core exit, when the operator needs to know about it (E12-c).
 *
 * A crashed core used to be invisible: the gateway cleared its handle and the
 * panel went on showing whatever the socket said, so a core that died and was
 * replaced looked identical to one that never died. The sentence below is the
 * gateway's own classification (`kind`), not something this page infers.
 */
const exit = computed(() => exitNotice(snap.value?.gateway, engineUp.value))
const exitText = computed(() => {
  const e = exit.value
  if (!e) return ''
  if (e.kind === 'clean') return `内核已停止：${e.description}`
  if (e.givenUp) {
    return `内核崩溃且已放弃重启（${e.description}）。请检查日志后手动启动。`
  }
  if (engineUp.value && e.restarts > 0) {
    return `内核曾崩溃（${e.description}），已自动重启 ${e.restarts} 次，当前运行中。`
  }
  return `内核崩溃：${e.description}`
})

/** Tooltip/title explaining why a control is unavailable (empty when usable). */
const startHint = computed(() => {
  if (busy.value) return '正在下发命令…'
  if (control.value.canStart) return '启动引擎'
  if (engineUp.value) return '引擎运行中，无需重复启动'
  return control.value.blockedReason ?? '无法启动引擎'
})
const stopHint = computed(() => {
  if (busy.value) return '正在下发命令…'
  if (control.value.canStop) return '停止引擎'
  if (!engineUp.value) return '引擎未运行'
  return control.value.blockedReason ?? '无法停止引擎'
})

async function send(cmd: 'start' | 'stop'): Promise<void> {
  busy.value = true
  cmdMsg.value = null
  try {
    const res = await api.command(cmd)
    cmdMsg.value = res.message ?? `${cmd} 已下发`
    await store.refresh()
  } catch (e) {
    cmdMsg.value = e instanceof Error ? e.message : String(e)
  } finally {
    busy.value = false
    setTimeout(() => { cmdMsg.value = null }, 4000)
  }
}

async function forceClose(position: Position): Promise<void> {
  if (!control.value.canStop || busy.value) return
  const confirmed = window.confirm(
    `确认强行平仓？\n\n${position.asset} ${position.direction.toUpperCase()} · ${Number(position.shares).toFixed(3)} 份\n当前价 ${Number(position.currentPrice).toFixed(3)}\n\n该操作会以人工退出原因记录，实盘将等待成交回报。`,
  )
  if (!confirmed) return
  busy.value = true
  cmdMsg.value = null
  try {
    const res = await api.flatten(position.id)
    cmdMsg.value = res.message ?? `已请求平仓 ${position.asset}`
    await store.refresh()
  } catch (e) {
    cmdMsg.value = e instanceof Error ? e.message : String(e)
  } finally {
    busy.value = false
    setTimeout(() => { cmdMsg.value = null }, 5000)
  }
}

// ── trade volume / averages ─────────────────────────────────────────────────
const tradeStats = computed(() => {
  const rows = historyRows.value
  const dayStart = new Date(new Date().setHours(0, 0, 0, 0)).getTime()
  const todays = rows.filter((t) => (t.exitTime ?? 0) >= dayStart)
  const totalVol = rows.reduce((x, t) => x + (Number(t.entryPrice) * Number(t.shares) || 0), 0)
  return {
    count: rows.length,
    today: todays.length,
    todayNet: todays.reduce((x, t) => x + (Number(t.netPnlUsd) || 0), 0),
    volume: totalVol,
    avg: rows.length ? totalVol / rows.length : 0,
  }
})

const tab = ref<'positions' | 'orders' | 'history'>('positions')
// E28 (§9.5): positions are the session account's too — same filter discipline
// as the history rows (unattributed rows from a pre-E28 core stay visible).
const positions = computed(() =>
  filterByVenue(
    filterByAccount(snap.value?.positions ?? [], store.activeAccount).rows,
    activeVenue.value,
  ).rows,
)

// ── issue 456: 挂单簿 + 强平 ────────────────────────────────────────────────
// 轮次是否已过期：round_slot N 覆盖 [N·dur, (N+1)·dur)，与内核过期扫描同一
// 公式；slot 0（无轮次）不参与（内核同样豁免它）。过期与否只决定按钮
// 可用性 —— 真判定在内核，按钮点了也过不了活轮拒绝。
const roundDurSec = 900
function orderExpired(o: OrderRow): boolean {
  return o.roundSlot > 0 && Date.now() / 1000 >= (o.roundSlot + 1) * roundDurSec
}

/**
 * E28 (§9.5): the account these 挂单/历史 rows belong to. The gateway serves
 * this panel through ONE long-lived IPC connection, so every read on this page
 * reflects the SESSION's default account — the switcher's `account <id>` moves
 * that context, the next poll re-pulls everything through it, and the rows are
 * filtered locally (lib/account-filter) to the active account. `filtered`
 * is true only when the kernel actually attributes rows (E28): on a pre-E28
 * core every row is unattributed and nothing is filtered — the label below
 * says 范围, not 已过滤, accordingly. Null id = book never fetched.
 */
const accountBadge = computed(() => {
  const id = store.activeAccount
  if (!id) return ''
  const row = store.accounts.find((a) => a.id === id)
  return row?.name ? `${id} · ${row.name}` : id
})
const accountFiltered = computed(() =>
  accountBadge.value !== '' &&
  (filterByAccount(snap.value?.positions ?? [], store.activeAccount).attributed ||
    filterByAccount(store.tradeRows, store.activeAccount).attributed),
)
// venue 层标注：active 账户绑定了 venue 且行数据带 venue 标签时，过滤真的
// 在生效 —— 标注要说清「过滤器在哪、为什么在」（账户绑定，不是独立控件）。
const venueFiltered = computed(() =>
  activeVenue.value !== '' &&
  (filterByVenue(snap.value?.positions ?? [], activeVenue.value).attributed ||
    filterByVenue(store.tradeRows, activeVenue.value).attributed),
)
const orderRows = computed<OrderRow[]>(() => {
  // snapshot 里已带 orders（网关从 orders.list 拉的同一份）；不在时回落
  // 到专用读路径，两边都是内核的 TrackedOrder wire 形状。
  // E28 (§9.5): rows are scoped to the session's default account — same
  // discipline as positions/history (pre-E28 unattributed rows stay visible).
  const fromSnap = (snap.value as unknown as { orders?: OrderRow[] } | null)?.orders
  return filterByVenue(
    filterByAccount(fromSnap ?? [], store.activeAccount).rows,
    activeVenue.value,
  ).rows
})
const expiredOrderRows = computed(() => orderRows.value.filter((o) => o.status === 'LIVE' || o.status === 'PARTIALLY_FILLED' || o.status === 'PENDING').filter(orderExpired))

const orderBusy = ref(false)
const orderErr = ref<string | null>(null)
const orderMsg = ref<string | null>(null)
const forceOpen = ref(false)
const forceTarget = ref<OrderRow | null>(null)
const sweepOpen = ref(false)

function orderStatusLabel(s: string): string {
  const map: Record<string, string> = {
    PENDING: '待确认', LIVE: '挂单中', PARTIALLY_FILLED: '部分成交',
    FILLED: '已成交', CANCELLED: '已撤销', REJECTED: '已拒绝',
    FAILED: '失败', FORCE_CLOSED: '已强平',
  }
  return map[s] ?? s
}
function orderStatusTone(s: string): 'up' | 'down' | 'default' | 'gold' {
  if (s === 'LIVE' || s === 'PENDING' || s === 'PARTIALLY_FILLED') return 'gold'
  if (s === 'FILLED') return 'up'
  if (s === 'FORCE_CLOSED' || s === 'CANCELLED') return 'down'
  return 'default'
}

function askForceClose(o: OrderRow): void {
  orderErr.value = null
  forceTarget.value = o
  forceOpen.value = true
}

async function doForceClose(): Promise<void> {
  const target = forceTarget.value
  if (!target || orderBusy.value) return
  orderBusy.value = true
  orderErr.value = null
  try {
    await api.forceCloseOrder({ orderId: target.orderId })
    orderMsg.value = `已强平 ${target.orderId}（审计已写）`
    forceOpen.value = false
    await store.refresh()
  } catch (e) {
    orderErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    orderBusy.value = false
    setTimeout(() => { orderMsg.value = null }, 5000)
  }
}

/** 批量清理过期挂单：逐单调内核，一单失败不挡下一单（各自审计）。 */
async function doSweepExpired(): Promise<void> {
  if (orderBusy.value) return
  orderBusy.value = true
  orderErr.value = null
  let closed = 0
  const failed: string[] = []
  try {
    for (const o of expiredOrderRows.value) {
      try {
        await api.forceCloseOrder({ orderId: o.orderId })
        closed += 1
      } catch {
        failed.push(o.orderId)
      }
    }
    orderMsg.value = failed.length
      ? `清理完成：${closed} 笔强平，${failed.length} 笔被内核拒绝（${failed.slice(0, 3).join('、')}${failed.length > 3 ? '…' : ''}）`
      : `清理完成：${closed} 笔挂单已强平（各写一条审计）`
    sweepOpen.value = false
    await store.refresh()
  } finally {
    orderBusy.value = false
    setTimeout(() => { orderMsg.value = null }, 6000)
  }
}
const positionUnrealized = computed(() =>
  positions.value.reduce((a, p) => a + (Number(p.unrealizedPct) || 0), 0),
)

function exitReasonTone(reason?: string): 'up' | 'down' | 'default' | 'gold' {
  if (!reason) return 'default'
  const r = reason.toLowerCase()
  if (r.includes('take') || r.includes('profit')) return 'up'
  if (r.includes('stop') || r.includes('loss')) return 'down'
  return 'gold'
}
</script>

<template>
  <template v-if="snap">
    <!-- ── round bar ─────────────────────────────────────────────────────── -->
    <Card class="rise-in relative overflow-hidden">
      <!-- time-remaining hairline across the card top -->
      <div
        class="absolute inset-x-0 top-0 h-[2px] origin-left transition-[width,background] duration-500"
        :style="{
          width: `${leftFraction * 100}%`,
          background: urgent
            ? 'linear-gradient(90deg, var(--down), oklch(0.72 0.18 40))'
            : 'linear-gradient(90deg, var(--primary), var(--primary-hi))',
        }"
      />
      <div class="flex flex-wrap items-center gap-x-5 gap-y-3">
        <div class="min-w-0">
          <div class="flex items-center gap-2">
            <Badge variant="gold">{{ marketTypeText || '市场' }}</Badge>
            <span class="truncate text-[13px] font-semibold">{{ marketName ?? '未激活插件' }}</span>
            <span class="label-micro">#{{ round?.slot ?? '—' }}</span>
          </div>
          <div class="mt-1.5 flex items-center gap-2 text-[11.5px] text-faint-fg">
            <Clock class="size-3.5" />
            <!--
              The age lives in a fixed four-digit slot so the status to its right
              cannot move. Two separate motions used to happen here: proportional
              figures gave `1111s 已过` and `1000s 已过` widths 7.95px apart at the
              same character count, and each extra digit added ~14px more, so
              TRADING/WAITING slid back and forth on every tick. The slot is
              `tabular-nums` so its `ch` matches the digits it reserves, and
              overflow grows leftward (text-right) past four digits — up to
              9999s ≈ 2.8h, beyond any round this market runs.
            -->
            <span class="inline-flex items-baseline">
              <!--
                `tabular-nums` on the slot itself, not just inside: the `ch` unit
                is resolved against THIS element's font, so without it the
                reserved width is the proportional "0" and drifts ~0.04px per
                place from the tabular cells it is reserving for.
              -->
              <span class="inline-block w-[4ch] text-right tabular-nums">
                <RollingNumber :value="round?.ageSec ?? '—'" :duration="320" />
              </span>
              <span class="ml-0.5">s 已过</span>
            </span>
            <span class="opacity-40">·</span>
            <span :class="round?.canTrade ? 'text-up font-semibold' : 'text-primary'">
              {{ round?.canTrade ? 'TRADING' : 'WAITING' }}
            </span>
          </div>
        </div>

        <div class="mx-auto text-center">
          <!--
            320ms roll on a 1s countdown, so the digits settle well before the
            next tick. `stat-num`'s tracking is dropped deliberately: the digit
            cells are fixed-width, so tracking cannot apply (see RollingNumber).
          -->
          <RollingNumber
            :value="leftText"
            :duration="320"
            class="stat-num text-[42px] leading-none"
            :class="urgent ? 'text-down' : 'grad-gold'"
          />
          <div class="label-micro mt-1">剩余时间</div>
        </div>

        <div class="ml-auto flex items-center gap-2">
          <Tooltip :content="sound ? '关闭提示音' : '开启提示音'">
            <Button variant="ghost" size="icon" @click="toggleSound">
              <Bell v-if="sound" /><BellOff v-else class="opacity-60" />
            </Button>
          </Tooltip>
          <!--
            Exactly one of these carries the next move. While the engine runs
            that is 停止, so it takes the solid deep fill and 启动 goes pale and
            inert; when the engine is down the pair swaps.

            Enabled state comes from `control`, not from reachability alone: the
            gateway may refuse the verbs outright (`--manage` absent) or be
            unable to stop an adopted core. A control that cannot act is disabled
            with the reason in its `title`, so hovering explains the refusal
            rather than leaving the user to discover it by clicking.
          -->
          <Button
            :variant="control.canStart ? 'up' : 'idle'"
            :disabled="busy || !control.canStart"
            :title="startHint"
            @click="send('start')"
          >
            <Play class="size-3.5" />启动
          </Button>
          <Button
            :variant="control.canStop ? 'danger-solid' : 'idle'"
            :disabled="busy || !control.canStop"
            :title="stopHint"
            @click="send('stop')"
          >
            <Square class="size-3.5" />停止
          </Button>
        </div>
      </div>

      <!--
        An inert pair with no explanation is the bug being fixed here, so when
        neither control can act, say which of the two reasons applies.
      -->
      <div
        v-if="!control.usable && control.blockedReason"
        class="mt-3 flex items-start gap-2 rounded-md border border-line bg-panel-2 px-3 py-2 text-[11.5px] leading-snug text-muted-fg"
      >
        <Info class="mt-px size-3.5 shrink-0 text-faint-fg" />
        <span>{{ control.blockedReason }}</span>
      </div>

      <!--
        A crash is stated in the panel rather than left to be inferred from a
        启动 button that suddenly looks clickable. It outlives the restart that
        fixed it (the wording changes), because a core that silently came back
        is a core that crashed, and the operator is the one who decides whether
        that is a fluke or a pattern.
      -->
      <AlertBanner
        v-if="exit"
        class="mt-3"
        :tone="exit.kind === 'crash' ? 'error' : 'info'"
      >{{ exitText }}</AlertBanner>

      <Transition name="fade">
        <div v-if="cmdMsg" class="mt-3 rounded-md border border-line bg-panel-2 px-3 py-2 text-[12px] text-muted-fg">
          {{ cmdMsg }}
        </div>
      </Transition>
    </Card>

    <!-- ── market price cards ────────────────────────────────────────────── -->
    <!--
      Says the feed stopped. Without this the cards below are indistinguishable
      from a live market: the core republishes its last book forever and the round
      countdown keeps running off the local clock. Stated above the prices rather
      than inside them so it is read before the numbers are.
    -->
    <!--
      A stale feed is an alert, so it is the kit's `AlertBanner` and not a
      page-local tint (issue 260): the hand-rolled `border-down/35 bg-down/8` was the
      same semantics at a second concentration.
    -->
    <AlertBanner
      v-if="feed.stale"
      class="mt-3.5"
      tone="error"
      :title="feed.label"
    >下面的报价是内核最后收到的行情，并非当前市场。轮次与倒计时按本地时钟推进，所以看起来仍在跳动；
      引擎也会因为行情过期而拒绝开仓。请检查行情插件与网络连接。</AlertBanner>

    <div v-if="prices.length" class="mt-3.5 flex items-center gap-2">
      <Activity class="size-3.5" :class="feed.stale ? 'text-down' : 'text-faint-fg'" />
      <span
        class="text-[11px] num"
        :class="feed.stale ? 'font-semibold text-down' : 'text-faint-fg'"
      >{{ feed.ageLabel }}</span>
      <Tooltip content="行情数据的新鲜度：内核里订单簿计数最后一次增长到现在的时间。轮次与倒计时按本地时钟走，所以它们会继续跳动，不能用来判断行情是否还在到达。">
        <span class="cursor-help text-[11px] text-faint-fg underline decoration-dotted decoration-line underline-offset-2">
          数据新鲜度
        </span>
      </Tooltip>
    </div>

    <div v-if="prices.length" class="mt-3.5 grid gap-3.5 sm:grid-cols-2 xl:grid-cols-4">
      <div
        v-for="m in prices"
        :key="m.asset"
        class="glass card-pad transition-transform duration-200 hover:-translate-y-0.5"
      >
        <div class="flex items-center justify-between">
          <span class="text-[13px] font-bold tracking-wide">{{ m.asset }}</span>
          <Badge :variant="spreadCents(m) <= 1 ? 'up' : 'gold'">
            {{ spreadCents(m).toFixed(1) }}¢
          </Badge>
        </div>
        <div class="mt-3 grid grid-cols-2 gap-3">
          <div class="rounded-md border border-up/25 bg-up/8 px-2.5 py-2">
            <div class="flex items-center gap-1 text-[10.5px] font-semibold text-up">
              <TrendingUp class="size-3" />UP
            </div>
            <div class="stat-num mt-1 text-[21px] leading-none text-up">
              <RollingNumber :value="Number(m.up).toFixed(3)" />
            </div>
            <div class="mt-0.5 text-[10px] text-faint-fg num">
              <RollingNumber :value="cents(m.up)" />
            </div>
          </div>
          <div class="rounded-md border border-down/25 bg-down/8 px-2.5 py-2">
            <div class="flex items-center gap-1 text-[10.5px] font-semibold text-down">
              <TrendingDown class="size-3" />DOWN
            </div>
            <div class="stat-num mt-1 text-[21px] leading-none text-down">
              <RollingNumber :value="Number(m.down).toFixed(3)" />
            </div>
            <div class="mt-0.5 text-[10px] text-faint-fg num">
              <RollingNumber :value="cents(m.down)" />
            </div>
          </div>
        </div>
      </div>
    </div>
    <Card v-else class="mt-3.5">
      <!--
        issue 488: 连不上 ≠ 未启用。插件活跃（marketName 有值）但行情卡空 =
        数据源拉不到（网络/代理/对端），指向网络自检而不是叫操作员去启用
        一个已经启用的插件 —— 那条提示是误导。
      -->
      <EmptyState
        :text="marketName ? `行情插件 ${marketName} 活跃，但暂无行情到达` : '暂无行情数据（等待报价插件）'"
        :hint="marketName
          ? '通常是网络/代理拦住了数据源域名。运行网关命令 netcheck 做网络自检，或检查本机代理是否放行该插件的 API 域名。'
          : '到「插件」页选定一个行情源（重启内核后生效）。'"
        compact
      />
    </Card>

    <!-- ── 盘口深度 (E8-c) ──────────────────────────────────────────────── -->
    <Card v-if="books.length" class="mt-3.5">
      <CardHeader label="盘口深度">
        <template #action>
          <div class="flex items-center gap-2">
            <select v-model="depthAsset" class="filter-select">
              <option v-for="b in books" :key="b.asset" :value="b.asset">{{ b.asset }}</option>
            </select>
            <SegmentedControl
              v-model="depthToken"
              :segments="[
                { id: 'up', label: 'UP' },
                { id: 'down', label: 'DOWN' },
              ]"
              size="sm"
            />
          </div>
        </template>
      </CardHeader>
      <!--
        The touch metrics first: a depth ladder is read from the spread and the
        imbalance before any level is looked at. Null means the feed has not
        delivered this token's book yet — never a zero quote.
      -->
      <div
        v-if="depthMetrics && activeSide"
        class="flex flex-wrap items-center gap-x-5 gap-y-1.5 text-[11.5px]"
      >
        <span>买一 <span class="num text-up"><RollingNumber :value="depthMetrics.bestBid != null ? cents(depthMetrics.bestBid) : '—'" /></span></span>
        <span>卖一 <span class="num text-down"><RollingNumber :value="depthMetrics.bestAsk != null ? cents(depthMetrics.bestAsk) : '—'" /></span></span>
        <span>点差 <span class="num"><RollingNumber :value="depthMetrics.spread != null ? cents(depthMetrics.spread) : '—'" /></span></span>
        <span>盘口失衡 OBI <span class="num" :class="(depthMetrics.obi ?? 0) >= 0 ? 'text-up' : 'text-down'"><RollingNumber :value="depthMetrics.obi != null ? signedPct(depthMetrics.obi * 100, 1) : '—'" /></span></span>
        <span class="text-faint-fg">累计买深 <span class="num text-fg"><RollingNumber :value="depthMetrics.bidDepth.toFixed(1)" /></span> · 卖深 <span class="num text-fg"><RollingNumber :value="depthMetrics.askDepth.toFixed(1)" /></span> 份</span>
      </div>
      <div class="mt-2">
        <BookDepth :side="activeSide" :height="168" />
      </div>
    </Card>
    <Card v-else-if="!hasDepthData" class="mt-3.5">
      <EmptyState text="盘口深度暂不可用" hint="当前内核未提供 engine.books——重启到新内核后自动出现。" compact />
    </Card>

    <!-- ── K线 (E29, kline.history) ─────────────────────────────────────────
      The aggregator keys bars by TOKEN id, so the card needs the books view's
      E29 token-id fields. Older cores omit them (empty string): the card hides
      rather than queries a wrong key. Shares the depth card's asset/token
      pickers so one selection drives both charts.
    -->
    <Card v-if="klineReady && klineSymbol" class="mt-3.5">
      <CardHeader label="K线">
        <template #action>
          <div class="flex items-center gap-2">
            <select v-model="depthAsset" class="filter-select">
              <option v-for="b in books" :key="b.asset" :value="b.asset">{{ b.asset }}</option>
            </select>
            <SegmentedControl
              v-model="depthToken"
              :segments="[
                { id: 'up', label: 'UP' },
                { id: 'down', label: 'DOWN' },
              ]"
              size="sm"
            />
          </div>
        </template>
      </CardHeader>
      <div class="mt-2">
        <KlineChart :symbol="klineSymbol" :height="260" />
      </div>
    </Card>

    <!-- ── stats row ─────────────────────────────────────────────────────── -->
    <!--
      Four stat cards. The 1.6/1/1/1 split only applies once there is room for it
      (`xl`), but the fallback used to be a single column all the way down — at
      tablet and small-laptop widths that stacked four full-width cards into a
      long scroll. `sm:grid-cols-2` puts them 2×2 across that whole range instead,
      and 累计净 PnL keeps the wider column whenever the split is active.
    -->
    <div class="mt-3.5 grid gap-3.5 sm:grid-cols-2 xl:grid-cols-[1.6fr_1fr_1fr_1fr]">
      <Card>
        <CardHeader label="累计净 PnL">
          <template #title>
            <span class="stat-num text-[16px]" :class="cumStats.net >= 0 ? 'text-up' : 'text-down'">
              <RollingNumber :value="signedMoney(cumStats.net)" />
            </span>
          </template>
          <template #action>
            <Badge variant="default">扣费口径</Badge>
          </template>
        </CardHeader>
        <EquityCurve :rows="historyRows" :height="128" :show-axis="false" />
        <div class="mt-2 flex items-center gap-4 text-[11px] text-faint-fg">
          <span>毛利 <span class="num text-fg"><RollingNumber :value="signedMoney(cumStats.gross)" /></span></span>
          <span>费用 <span class="num text-down"><RollingNumber :value="money(cumStats.fees)" /></span></span>
        </div>
      </Card>

      <Card>
        <CardHeader label="胜率分布" />
        <div class="stat-num text-[30px] leading-none">
          <RollingNumber :value="pct(cumStats.winRate)" />
        </div>
        <div class="mt-1 flex items-center gap-3 text-[11.5px]">
          <span class="font-semibold text-up"><RollingNumber :value="cumStats.wins" /> 盈</span>
          <span class="font-semibold text-down"><RollingNumber :value="cumStats.losses" /> 亏</span>
        </div>
        <div class="mt-3.5 space-y-1.5">
          <div v-for="b in winBands" :key="b.label" class="flex items-center gap-2">
            <span class="w-[52px] shrink-0 text-[10.5px] text-faint-fg">{{ b.label }}</span>
            <span class="h-1.5 flex-1 overflow-hidden rounded-full bg-panel-2">
              <span
                class="block h-full rounded-full transition-[width] duration-500"
                :style="{
                  width: `${b.frac * 100}%`,
                  background: b.tone === 'up' ? 'var(--up)' : 'var(--down)',
                }"
              />
            </span>
            <span class="w-7 shrink-0 text-right text-[10.5px] text-muted-fg num"><RollingNumber :value="b.count" /></span>
          </div>
        </div>
      </Card>

      <Card>
        <CardHeader label="交易统计" />
        <div class="grid grid-cols-2 gap-x-4 gap-y-3">
          <div>
            <div class="label-micro">笔数</div>
            <div class="stat-num mt-1 text-[19px] leading-none"><RollingNumber :value="tradeStats.count" /></div>
          </div>
          <div>
            <div class="label-micro">今日</div>
            <div class="stat-num mt-1 text-[19px] leading-none"><RollingNumber :value="tradeStats.today" /></div>
          </div>
          <div>
            <div class="label-micro">成交额</div>
            <div class="stat-num mt-1 text-[15px] leading-none"><RollingNumber :value="money(tradeStats.volume)" /></div>
          </div>
          <div>
            <div class="label-micro">均笔</div>
            <div class="stat-num mt-1 text-[15px] leading-none"><RollingNumber :value="money(tradeStats.avg)" /></div>
          </div>
        </div>
        <div class="mt-3.5 flex items-center justify-between border-t border-line pt-2.5 text-[11.5px]">
          <span class="text-faint-fg">今日净利</span>
          <span class="stat-num" :class="tradeStats.todayNet >= 0 ? 'text-up' : 'text-down'">
            <RollingNumber :value="signedMoney(tradeStats.todayNet)" />
          </span>
        </div>
      </Card>

      <Card>
        <CardHeader label="今日表现" />
        <StatRow label="最佳单笔" :value="signedPct(cumStats.best)" tone="up" />
        <StatRow label="最差单笔" :value="signedPct(cumStats.worst)" tone="down" />
        <StatRow label="持仓浮动" :value="signedPct(positionUnrealized)" :tone="positionUnrealized >= 0 ? 'up' : 'down'" />
        <StatRow label="平均持仓" :value="cumStats.avgHold ? duration(cumStats.avgHold) : '—'" tone="dim" />
        <!-- Words, not figures: DRY / LIVE never rolls, so it stays plain text. -->
        <StatRow label="运行模式" :value="(snap.mode ?? '—').toUpperCase()" :tone="isDry ? 'gold' : 'up'" :roll="false" />
        <StatRow label="行情轮次" :value="round?.markets ?? '—'" tone="dim" />
        <div class="mt-2.5 border-t border-line pt-2.5">
          <!--
            Headline is 本金 ＋ 净利润 (the real balance). No cash-ledger row
            underneath in DRY: the core's ledger is 本金 ＋ 本次会话净利, a strict
            subset of what the history tab below already shows.
          -->
          <StatRow
            :label="balanceAccountSuffix(isDry ? (recon.equity !== null ? '真实余额' : '内核现金账') : (recon.equity !== null ? '账户权益' : '交易所余额'))"
            :value="recon.equity !== null ? money(recon.equity) : money(recon.cash)"
            :tone="isDry ? 'gold' : 'default'"
          />
          <p v-if="recon.equity !== null" class="mt-1 text-[10px] leading-snug text-faint-fg">
            本金 {{ money(recon.seed) }} ＋ 已实现净利 {{ signedMoney(recon.net) }}
            <Tooltip :content="`真实余额 = 本金 ${money(recon.seed)} ＋ 扣费后净利 ${signedMoney(recon.net)}。手续费是成本：毛利 ${money(recon.gross)} − 手续费 ${money(recon.fees)} = 净利。手续费不改变本金，因此不并入余额。${balanceAccountNote}`">
              <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
                手续费支出 {{ money(recon.fees) }}
              </span>
            </Tooltip>
          </p>
          <!--
            No principal on the wire: either LIVE, or a core older than the field
            that reports it. Show the cash ledger and say the principal is
            missing instead of printing an equation the panel cannot verify.
          -->
          <p v-else-if="isDry" class="mt-1 text-[10px] leading-snug text-faint-fg">
            <Tooltip content="内核未上报本金，无法计算「本金 ＋ 净利」的真实余额，此处显示内核现金账。本金由内核按 --seed-balance 持有；当前运行中的内核早于该字段，重启内核后即可显示真实余额。">
              <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
                内核现金账 · 本金未上报
              </span>
            </Tooltip>
          </p>
        </div>
      </Card>
    </div>

    <!-- ── positions / orders / history ─────────────────────────────────── -->
    <Card class="mt-3.5" dense>
      <div class="mb-3.5 flex flex-wrap items-center gap-3">
        <SegmentedControl
          v-model="tab"
          :segments="[
            { id: 'positions', label: '当前持仓', badge: positions.length },
            { id: 'orders', label: '当前挂单', badge: orderRows.length },
            { id: 'history', label: '历史订单', badge: cumStats.total },
          ]"
          size="sm"
        />
        <!-- E28：账户范围标注。真过滤生效（内核带 accountId）时说「已过滤」；
             老内核/旧记录（accountId 空）只说「范围」，行全显不过滤。 -->
        <span
          v-if="accountBadge"
          class="text-[11.5px] text-faint-fg"
          :title="accountFiltered
            ? '本面板连接的默认账户（顶栏可切换）；持仓、挂单与历史已按该账户过滤，无归属的旧行仍然显示'
            : '本面板连接的默认账户（顶栏可切换）；当前内核未标注行归属，以下为全部行'"
        >
          <Info class="mr-1 inline size-3 align-[-1px]" />{{ accountFiltered ? '已按账户过滤' : '账户' }}
          <span class="text-fg">{{ accountBadge }}</span>
        </span>
        <span
          v-if="venueFiltered"
          class="text-[11.5px] text-faint-fg"
          :title="'当前账户绑定了交易所（顶栏账户卡可改绑定）；以下只显示该交易所的行情与交易'"
        >
          <Info class="mr-1 inline size-3 align-[-1px]" />已按账户绑定的交易所过滤：<span class="text-fg">{{ activeVenue }}</span>
        </span>
        <!-- issue 456: 批量清理过期挂单 —— 逐单调 orders.force_close，内核
             逐单拒绝仍活着的轮次，逐单写审计。只清理「轮次已过期」的行，
             活跃挂单原样保留（强平只兜幽灵，不做无差别撤单）。 -->
        <Button
          v-if="tab === 'orders' && expiredOrderRows.length > 0"
          class="ml-auto"
          variant="danger"
          size="sm"
          :disabled="orderBusy"
          :title="`对 ${expiredOrderRows.length} 笔轮次已过期的挂单逐笔强平（活跃挂单不动）`"
          @click="sweepOpen = true"
        >
          清理过期挂单（{{ expiredOrderRows.length }}）
        </Button>
        <div v-if="tab === 'history'" class="ml-auto flex flex-wrap items-center gap-2">
          <select v-model="fTime" class="filter-select">
            <option value="all">全部时间</option>
            <option value="today">今日</option>
            <option value="7d">近 7 天</option>
          </select>
          <select v-model="fAsset" class="filter-select">
            <option value="all">全部币种</option>
            <option v-for="a in assetOptions" :key="a" :value="a">{{ a }}</option>
          </select>
          <select v-model="fOutcome" class="filter-select">
            <option value="all">全部盈亏</option>
            <option value="win">仅盈利</option>
            <option value="loss">仅亏损</option>
          </select>
          <select v-model="fStrategy" class="filter-select">
            <option value="all">全部策略</option>
            <option v-for="s in strategyOptions" :key="s" :value="s">{{ s }}</option>
          </select>
          <Button v-if="hasFilters" variant="ghost" size="sm" @click="resetFilters">
            <X class="size-3.5" />重置
          </Button>
        </div>
      </div>

      <!-- cumulative header + filtered subtotal -->
      <template v-if="tab === 'history'">
        <div class="mb-3.5 grid grid-cols-2 gap-3 rounded-lg border border-line bg-panel-2 p-3 sm:grid-cols-4">
          <div>
            <div class="label-micro">累计订单</div>
            <div class="stat-num mt-1 text-[22px] leading-none"><RollingNumber :value="cumStats.total" /></div>
          </div>
          <div>
            <div class="label-micro">累计利润（扣费）</div>
            <div class="stat-num mt-1 text-[22px] leading-none" :class="cumStats.net >= 0 ? 'text-up' : 'text-down'">
              <RollingNumber :value="signedMoney(cumStats.net)" />
            </div>
          </div>
          <div>
            <div class="label-micro">累计胜率</div>
            <div class="stat-num mt-1 text-[22px] leading-none"><RollingNumber :value="pct(cumStats.winRate)" /></div>
          </div>
          <div>
            <div class="label-micro">盈利 / 亏损</div>
            <div class="stat-num mt-1 text-[22px] leading-none">
              <span class="text-up"><RollingNumber :value="cumStats.wins" /></span>
              <span class="text-faint-fg"> / </span>
              <span class="text-down"><RollingNumber :value="cumStats.losses" /></span>
            </div>
          </div>
        </div>
        <p v-if="!cumStats.fromSummary" class="mb-3 text-[10.5px] text-faint-fg">
          累计值由已加载的行汇总（旧内核未提供全史 summary）。
        </p>
        <div v-if="hasFilters" class="mb-3 flex items-center gap-2 text-[11.5px] text-faint-fg">
          <Filter class="size-3.5" />
          已筛选 <span class="num text-fg"><RollingNumber :value="filteredStats.count" /></span> 笔 ·
          净利 <span class="stat-num" :class="filteredStats.net >= 0 ? 'text-up' : 'text-down'"><RollingNumber
            :value="signedMoney(filteredStats.net)"
          /></span>
          · 盈 <span class="text-up num"><RollingNumber :value="filteredStats.wins" /></span>
          / 亏 <span class="text-down num"><RollingNumber :value="filteredStats.losses" /></span>
        </div>
      </template>

      <!-- positions -->
      <div v-if="tab === 'positions'">
        <div v-if="positions.length" class="overflow-x-auto">
          <table class="w-full min-w-[640px] text-[13px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">资产</th>
                <th class="label-micro px-2 pb-2">方向</th>
                <th class="label-micro px-2 pb-2">场地</th>
                <th class="label-micro px-2 pb-2">策略</th>
                <th class="label-micro px-2 pb-2 text-right">入场</th>
                <th class="label-micro px-2 pb-2 text-right">现价</th>
                <th class="label-micro px-2 pb-2 text-right">份额</th>
                <th class="label-micro px-2 pb-2 text-right">浮动</th>
                <th class="label-micro px-2 pb-2 text-right">剩余</th>
                <th class="label-micro px-2 pb-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="p in positions"
                :key="p.id"
                class="border-t border-line transition-colors hover:bg-panel-2"
              >
                <td class="px-2 py-2.5 font-semibold">{{ p.asset }}</td>
                <td class="px-2 py-2.5">
                  <Badge :variant="p.direction === 'up' ? 'up' : 'down'">{{ p.direction.toUpperCase() }}</Badge>
                </td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ p.venue || '—' }}</td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ p.strategy ?? '—' }}</td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(p.entryPrice).toFixed(3)" /></td>
                <td class="px-2 py-2.5 text-right num"><RollingNumber :value="Number(p.currentPrice).toFixed(3)" /></td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="p.shares ?? '—'" /></td>
                <td class="px-2 py-2.5 text-right num font-semibold" :class="p.unrealizedPct >= 0 ? 'text-up' : 'text-down'">
                  <RollingNumber :value="`${Number(p.unrealizedPct) >= 0 ? '+' : ''}${Number(p.unrealizedPct).toFixed(2)}%`" />
                </td>
                <td class="px-2 py-2.5 text-right num text-faint-fg">
                  <RollingNumber :value="p.remainingSec !== undefined ? duration(p.remainingSec) : '—'" />
                </td>
                <td class="px-2 py-2.5 text-right">
                  <Button
                    variant="danger"
                    size="sm"
                    :disabled="busy || !control.canStop"
                    :title="busy ? '正在下发命令…' : control.canStop ? '人工强行平仓' : (control.blockedReason ?? '当前网关不允许人工操作')"
                    @click="forceClose(p)"
                  >
                    强平
                  </Button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <EmptyState v-else :text="venueFiltered ? `暂无 ${activeVenue} 的持仓` : '无持仓'" :hint="venueFiltered
          ? `当前账户绑定了 ${activeVenue}，只显示该交易所的持仓。若该所行情未喂入，请到插件页确认插件已启用（插件启用将随账户派生：绑定即启用）。`
          : '启用策略后，成交平仓即出现在这里。'" compact />
      </div>

      <!-- issue 456: 挂单簿 —— 内核记账的订单，逐行强平（确认词门） -->
      <div v-else-if="tab === 'orders'">
        <div v-if="orderRows.length" class="overflow-x-auto">
          <table class="w-full min-w-[900px] text-[13px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">资产</th>
                <th class="label-micro px-2 pb-2">方向</th>
                <th class="label-micro px-2 pb-2">侧</th>
                <th class="label-micro px-2 pb-2">方式</th>
                <th class="label-micro px-2 pb-2">策略</th>
                <th class="label-micro px-2 pb-2 text-right">价格</th>
                <th class="label-micro px-2 pb-2 text-right">数量</th>
                <th class="label-micro px-2 pb-2 text-right">已成交</th>
                <th class="label-micro px-2 pb-2">轮次</th>
                <th class="label-micro px-2 pb-2">状态</th>
                <th class="label-micro px-2 pb-2 text-right">操作</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="o in orderRows"
                :key="o.orderId"
                class="border-t border-line transition-colors hover:bg-panel-2"
              >
                <td class="px-2 py-2.5 font-semibold">{{ o.asset }}</td>
                <td class="px-2 py-2.5">
                  <Badge :variant="o.direction === 'up' ? 'up' : 'down'">{{ o.direction.toUpperCase() }}</Badge>
                </td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ o.side }}</td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ o.mode }}</td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ o.strategy || '—' }}</td>
                <td class="px-2 py-2.5 text-right num"><RollingNumber :value="Number(o.price).toFixed(3)" /></td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(o.size).toFixed(1)" /></td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(o.filledSize).toFixed(1)" /></td>
                <td class="px-2 py-2.5 text-[12px] num text-faint-fg">
                  <RollingNumber :value="`#${o.roundSlot}`" />
                </td>
                <td class="px-2 py-2.5">
                  <Badge :variant="orderStatusTone(o.status)">{{ orderStatusLabel(o.status) }}</Badge>
                </td>
                <td class="px-2 py-2.5 text-right">
                  <Button
                    variant="danger"
                    size="sm"
                    :disabled="orderBusy || !orderExpired(o)"
                    :title="orderExpired(o)
                      ? `强平 ${o.orderId}（轮次已过期，写 force_close 审计）`
                      : '轮次仍活着，强平被内核拒绝 —— 活单等成交或自然过期'"
                    @click="askForceClose(o)"
                  >
                    强平
                  </Button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <EmptyState v-else text="无挂单" hint="策略报出的订单会出现在这里；过期残留可用强平清理。" compact />
      </div>

      <!-- history waterfall -->
      <div v-else>
        <div v-if="visibleRows.length" class="overflow-x-auto">
          <!-- 13 列在手机上不可压成一列一个字符：min-width 让表格保持
               可读列宽、在容器内横向滚动（与 Plugins 页表格同一模式），
               否则 min-content 按"每列一个折行点"计算，数字会竖排。 -->
          <table class="w-full min-w-[1160px] text-[13px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">资产</th>
                <th class="label-micro px-2 pb-2">方向</th>
                <th class="label-micro px-2 pb-2">场地</th>
                <th class="label-micro px-2 pb-2">策略</th>
                <th class="label-micro px-2 pb-2 text-right">入场</th>
                <th class="label-micro px-2 pb-2 text-right">出场</th>
                <th class="label-micro px-2 pb-2 text-right">份额</th>
                <th class="label-micro px-2 pb-2 text-right">费用</th>
                <th class="label-micro px-2 pb-2 text-right">净利</th>
                <th class="label-micro px-2 pb-2 text-right">收益率</th>
                <th class="label-micro px-2 pb-2 text-right">持仓时长</th>
                <th class="label-micro px-2 pb-2">买入时间</th>
                <th class="label-micro px-2 pb-2">卖出时间</th>
                <th class="label-micro px-2 pb-2">原因</th>
              </tr>
            </thead>
            <tbody>
              <!--
                Keyed on the trade's identity, never on `t.id`: the id is a
                per-boot counter (`hft-1` …) and the log is append-only, so the
                first page of 30 rows carried only 14 distinct ids. Duplicate
                keys break Vue's patch algorithm — it reuses the wrong nodes, so
                the table showed rows from the pre-filter list and left stale
                rows behind on 重置.
              -->
              <tr
                v-for="t in visibleRows"
                :key="tradeIdentity(t)"
                class="border-t border-line transition-colors hover:bg-panel-2"
              >
                <td class="px-2 py-2.5 font-semibold">{{ t.asset }}</td>
                <td class="px-2 py-2.5">
                  <Badge :variant="t.direction === 'up' ? 'up' : 'down'">{{ t.direction.toUpperCase() }}</Badge>
                </td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ t.venue || '—' }}</td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ t.strategy ?? '—' }}</td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(t.entryPrice).toFixed(3)" /></td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(t.exitPrice).toFixed(3)" /></td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="t.shares" /></td>
                <td class="px-2 py-2.5 text-right num text-down">
                  <RollingNumber :value="t.feesUsd ? money(t.feesUsd, 3) : '—'" />
                </td>
                <td class="px-2 py-2.5 text-right num font-semibold" :class="Number(t.netPnlUsd) >= 0 ? 'text-up' : 'text-down'">
                  <RollingNumber :value="signedMoney(t.netPnlUsd, 2)" />
                </td>
                <td class="px-2 py-2.5 text-right num" :class="Number(t.netPnlPct ?? 0) >= 0 ? 'text-up' : 'text-down'">
                  <RollingNumber :value="signedPct(t.netPnlPct, 2)" />
                </td>
                <td class="px-2 py-2.5 text-right num text-faint-fg">
                  <RollingNumber :value="t.holdTimeSec !== undefined ? duration(t.holdTimeSec) : '—'" />
                </td>
                <td class="px-2 py-2.5 text-[11.5px] text-faint-fg num">{{ dateTime(t.entryTime) }}</td>
                <td class="px-2 py-2.5 text-[11.5px] text-faint-fg num">{{ dateTime(t.exitTime) }}</td>
                <td class="px-2 py-2.5">
                  <Badge v-if="t.exitReason" :variant="exitReasonTone(t.exitReason)">{{ t.exitReason }}</Badge>
                  <span v-else class="text-faint-fg">—</span>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <EmptyState v-else text="无匹配订单" hint="清空搜索或筛选后仍为空，说明该窗口尚无成交。" compact />

        <!-- waterfall sentinel + fallback button -->
        <div ref="sentinel" class="h-px" />
        <div v-if="hasMore" class="mt-3 flex flex-col items-center gap-2">
          <Button variant="outline" size="sm" @click="visibleCount += PAGE">加载更多</Button>
          <span class="text-[10.5px] text-faint-fg num">
            已显示 <RollingNumber :value="visibleRows.length" /> / <RollingNumber :value="filteredRows.length" />
          </span>
        </div>
        <p v-else-if="filteredRows.length > PAGE" class="mt-3 text-center text-[10.5px] text-faint-fg">
          已加载全部 <RollingNumber :value="filteredRows.length" /> 笔
        </p>
      </div>
    </Card>
  </template>

  <Card v-else class="rise-in">
    <EmptyState
      :loading="store.loading"
      text="暂无快照数据"
      hint="网关在线时会自动拉取；持续为空请确认 blitzkrieg-core 已启动。"
    />
  </Card>

  <!-- issue 456: 单笔强平确认词门 —— 逐字输入 FORCE 才解锁执行。 -->
  <ConfirmWordDialog
    v-model:open="forceOpen"
    word="FORCE"
    title="强平挂单"
    :lines="forceTarget ? [
      `订单 ${forceTarget.orderId}（${forceTarget.asset} ${forceTarget.direction.toUpperCase()} · ${Number(forceTarget.price).toFixed(3)} × ${Number(forceTarget.size).toFixed(1)}）`,
      '动作 = 撤场地侧残留 + 终态 ForceClosed（不可恢复）',
      '每次强平内核写一条 force_close 审计（订单/策略/轮次/原因）',
      '轮次仍活着的挂单会被内核拒绝 —— 强平只兜幽灵，不做无差别撤单',
    ] : []"
    action-label="确认强平"
    :busy="orderBusy"
    :error="orderErr"
    @confirm="doForceClose"
  />

  <!-- issue 456: 批量清理过期挂单确认词门 —— 同一门，逐单审计。 -->
  <ConfirmWordDialog
    v-model:open="sweepOpen"
    word="FORCE"
    title="清理过期挂单"
    :lines="[
      `对 ${expiredOrderRows.length} 笔轮次已过期的挂单逐笔强平`,
      '活跃挂单不在清理范围（按钮只统计已过期的行）',
      '逐单调用 orders.force_close，逐单写审计；个别被拒不挡其余',
      '被内核拒绝的订单会在结果里点名',
    ]"
    action-label="确认清理"
    :busy="orderBusy"
    :error="orderErr"
    @confirm="doSweepExpired"
  />
</template>

<style scoped>
.filter-select {
  height: 28px;
  border-radius: 8px;
  border: 1px solid var(--line);
  background: var(--panel-2);
  color: var(--fg);
  font-size: 11.5px;
  font-family: inherit;
  padding: 0 8px;
  outline: none;
  transition: border-color 0.15s;
}
.filter-select:hover { border-color: var(--line-strong); }
.filter-select:focus { border-color: oklch(0.78 0.16 68 / 0.55); }
.filter-select option { background: var(--panel-solid); color: var(--fg); }

.fade-enter-active, .fade-leave-active { transition: opacity 0.25s; }
.fade-enter-from, .fade-leave-to { opacity: 0; }
</style>
