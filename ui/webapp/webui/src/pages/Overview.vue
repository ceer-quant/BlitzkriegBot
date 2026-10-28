<script setup lang="ts">
/**
 * 总览 — the cockpit. KPI tiles (balance / round / trades / mode), a live
 * equity curve, engine counters, rejection attribution and open positions.
 * Every number the old hft.html surfaced is reachable from here or 行情面板.
 */
import { computed, ref } from 'vue'
import {
  CircleDot, ShieldCheck, ArrowUpRight, ArrowDownRight,
} from 'lucide-vue-next'
import { usePanelStore } from '@/stores/panel'
import {
  num, compact, money, signedMoney, winRatePct, pct, shortAddr, duration,
} from '@/lib/format'
import { api } from '@/api/client'
import { balanceView } from '@/lib/balance'
import { filterByAccount, filterByVenue } from '@/lib/account-filter'
import {
  connectionBanner, freezeBanner, lastErrorBanner, selfCheckBanner, archiveStoppedBanner,
} from '@/lib/safety'
import StatTile from '@/components/ui/stat/StatTile.vue'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import EquityCurve from '@/components/charts/EquityCurve.vue'
import RejectionChart from '@/components/charts/RejectionChart.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'

const store = usePanelStore()

const snap = computed(() => store.snapshot)
const strategyRows = computed(() => store.strategyRows)

// ── trading safety (freeze / last refusal / self-check) ─────────────────────
// The three banners below were dark because `snap.stats` could not carry the
// fields they read — the kernel sent them, the UI Kit view type dropped them
// (issue 236). The reading rules (and what each title is allowed to claim) live
// in lib/safety.ts so they are testable; this page only renders them.
const stats = computed(() => snap.value?.stats ?? null)
const connBanner = computed(() => connectionBanner(snap.value))
const freeze = computed(() => freezeBanner(stats.value))
const lastError = computed(() => lastErrorBanner(stats.value, freeze.value !== null))
const selfCheck = computed(() => selfCheckBanner(stats.value))
// issue 669: the event archive stopped recording — the kernel's stop was a
// single journal line, so an operator found days of missing backtest corpus by
// hand. The reading rule lives in lib/safety.ts like the other banners; this
// one stays up on EVERY snapshot while `stats.archive.recording` is false.
const archiveStopped = computed(() => archiveStoppedBanner(stats.value))

// ── banner severity (2026-10-28 operator call) ──────────────────────────────
// 红色只留给紧急（冻结 / 自检失败 —— 交易此刻真的停了）：persistent、无 ✕、
// 不自动消失。其余（最近交易错误 / 连接 / 归档）琥珀色 + ✕ + 40s 自动消失；
// 关掉的是「这一条」（identity 计），同一条不再重弹，新错误重新亮起。
const AUTO_DISMISS_MS = 40_000
const dismissed = ref(new Set<string>())
const dismiss = (identity: string) => {
  const next = new Set(dismissed.value)
  next.add(identity)
  dismissed.value = next
}
// The kernel's `lastError` slot keeps its value until the NEXT error overwrites
// it — a dismissed identity must stay dismissed across snapshots. A different
// message/code yields a different identity, so the banner returns for it.
const lastErrorVisible = computed(
  () => lastError.value && !(lastError.value.identity && dismissed.value.has(lastError.value.identity)),
)
const connBannerVisible = computed(
  () => connBanner.value && !(connBanner.value.identity && dismissed.value.has(connBanner.value.identity)),
)
const archiveStoppedVisible = computed(
  () => archiveStopped.value
    && !(archiveStopped.value.identity && dismissed.value.has(archiveStopped.value.identity)),
)
const bannerAutoKey = (identity?: string) => identity ?? ''

// ── #684 过期持仓清除（positions.force_close）────────────────────────────────
// 场地 updown 列表只载未来 ~20h 的未到期轮次，已过期 conditionId 的结算裁决
// 永远等不到 —— 持仓变成幽灵（positions.exit 被场地拒）。仅 remainingSec <= 0
// 的行给「清除」按钮；判定（已过期 / 审计门 / 持仓存在）全部在内核，前端只
// 做确认与反馈。成功/失败的行内文案都来自内核自己的回执/错误。
const clearingId = ref<string | null>(null)
const clearMsg = ref<string | null>(null)
const clearErr = ref<string | null>(null)

async function clearExpiredPosition(positionId: string): Promise<void> {
  if (clearingId.value) return
  if (!confirm(`清除持仓 ${positionId}？仅限已过期且裁决永远等不到的市场；按兜底价平账（不建赎回申索），并写 manual-ops 审计。`)) {
    return
  }
  clearingId.value = positionId
  clearMsg.value = null
  clearErr.value = null
  try {
    const r = await api.forceClosePosition({ positionId })
    clearMsg.value = `已清除 ${r.positionId}（平账价 ${Number(r.price).toFixed(3)}，审计已写）`
    await store.refresh()
  } catch (e) {
    clearErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    clearingId.value = null
    setTimeout(() => { clearMsg.value = null; clearErr.value = null }, 8000)
  }
}

// ── balance (principal + net profit is the real balance in both modes) ──────
// issue 561（裁决 1）：真实余额瓦片跟随显示账户 —— 读 account.list 行
// （内核 account_views 已按 ledger_of(id) 读该账户自有账本：balance/
// available/reserved），不再读快照顶层的进程级 ledger.balance。切到 B 账户
// 时显示 B 的账本，不能扣 B 的名字却算 A 的钱。
const isDry = computed(() => (snap.value?.mode ?? 'dry') === 'dry')
const balance = computed(() => {
  const row = store.activeAccountRow
  if (row) {
    return {
      // account.list 行的十进制跨线是 string|number，归一成 number 再进
      // balanceView（该函数与内核快照的 number 口径对齐）。
      balance: Number(row.balance) || 0,
      reserved: Number(row.reserved) || 0,
      available: Number(row.available) || 0,
      // account.list 行不带 seed —— 本金从快照顶层（dry 配置值）取，
      // equity = 本金 + 该账户净利的口径不变。
      seed: snap.value?.balance?.seed ?? null,
    }
  }
  // 行未到（面板刚启动的一拍）：回落快照顶层，标称口径由 balanceTitle
  // 的 row 兜底分支如实标注 —— 不渲染一个猜出来的 0。
  return snap.value?.balance ?? null
})
const walletAddr = computed(() => snap.value?.wallet?.funder ?? snap.value?.wallet?.signer ?? null)

// ── trades ─────────────────────────────────────────────────────────────────
// issue 561 同型补漏（真实余额错位，2026-10-23 操作者报告）：issue 571 把瓦片的
// 现金账切到了显示账户的 account.list 行，但这里的净利还在读快照顶层的
// 全局 tradeSummary —— seed+全局净利会把别的账户的亏算进本账户的余额
// （真线实锤：default 现金账 999.33、自账仅 1 笔，瓦片却显示 990.48，
// 因为全局 17 笔里含 kalshi 的 −8.85）。读数源改为与 HftPage cumStats
// 同型：内核账户切片 summary（/api/trades-summary?accountId=display，
// store.accountTradeSummary）优先；切片缺失（老内核/未到一拍）回落到
// 账户过滤后的行求和 —— 回落也必须过滤，不能用全局行。
const tradeRows = computed(() => store.tradeRows)
const accountFilteredRows = computed(() => filterByAccount(tradeRows.value, store.activeAccount).rows)
// 权益曲线跟随显示账户（像真实余额一样，2026-10-20 操作者指令）：曲线此前
// 读快照顶层的全局 tradeRows —— 余额瓦片已切到账户自有读数，曲线还画着
// 全进程的成交（跨所跑多账户时 B 账户的曲线里混着 A 的亏），口径劈叉。
// 读数源 = 已平仓交易瓦片同一过滤链（账户切片）再叠 venue 域
// （filterByVenue：显示账户绑定的所；未绑定 = 全域）。无显示账户时全量行
// 即进程级一拍（老行为）。
const equityCurveRows = computed(() => {
  if (!store.activeAccount) return tradeRows.value
  return filterByVenue(accountFilteredRows.value, store.activeAccountVenue).rows
})
const trades = computed(() => {
  // 切片在场且报了成交数即用（内核已按账户过滤，与 HftPage cumStats 同型）；
  // 切片报 0 笔时也回落行兜底——老内核切片字段可能缺席，行求和仍按账户过滤。
  const scoped = store.accountTradeSummary
  if (scoped && (scoped.totalTrades ?? 0) > 0) {
    return {
      count: scoped.totalTrades ?? 0,
      net: scoped.totalNetPnl ?? 0,
      fees: scoped.totalFees ?? 0,
      winRate: winRatePct(scoped.winRate),
      win: scoped.wins ?? 0,
      loss: scoped.losses ?? 0,
    }
  }
  // 切片读数缺席时的兜底：账户过滤后的行求和。不能用全局行——有显示账户
  // 而切片未到的一拍，宁可显示过滤行求和，也不能拿全局净利冒充本账户余额
  // （真线实锤：default 自账 1 笔 −0.67，全局 17 笔含 kalshi −8.85，读全局
  // 块瓦片显示 990.48 而非 999.33）。无显示账户时全量行即进程级一拍。
  const rows = store.activeAccount ? accountFilteredRows.value : tradeRows.value
  const win = rows.filter((t) => (Number(t.netPnlUsd) || 0) > 0).length
  return {
    count: rows.length,
    net: rows.reduce((a, t) => a + (Number(t.netPnlUsd) || 0), 0),
    fees: rows.reduce((a, t) => a + (Number(t.feesUsd) || 0), 0),
    winRate: rows.length ? (win / rows.length) * 100 : 0,
    win,
    loss: rows.length - win,
  }
})

/**
 * 真实余额 = 本金 ＋ 净利润. The core's cash ledger is deliberately not shown
 * beside it in DRY (see `lib/balance.ts` for why it is a subset of what the
 * trade log already records).
 *
 * Fees are an expense, so they are shown as a cost charged against gross profit
 * — never folded into the balance, which would make the fee look like it moved
 * the principal.
 */
const recon = computed(() => balanceView(balance.value, trades.value.net, trades.value.fees))

/**
 * The tile label tracks what the figure above it actually is. When the equity is
 * computable the headline is 本金 ＋ 净利润; when the core reports no principal
 * the panel falls back to the cash ledger and the label must say so, or the
 * number would be read as a balance it is not.
 * issue 561：读数源已是显示账户自有账本（account.list 行）；行未到的兜底
 * 一拍标注「进程级」—— 那是快照顶层字段的口径，如实说出它属于谁。
 */
const balanceTitle = computed(() => {
  if (store.activeAccountRow === null) return '进程级读数'
  if (recon.value.equity !== null) return isDry.value ? '真实余额' : '账户权益'
  return isDry.value ? '内核现金账' : '交易所余额'
})

/**
 * E28 (§9.5) + issue 561: WHICH account this tile's figures belong to — the
 * DISPLAY account (面板唯一可信源；store.activeAccount 是它的兼容读数).
 * The switcher's `accounts` fetch mirrors the book into the store; the name
 * comes along so the tile can read 「default · 实盘主账户」 instead of a bare
 * id. Unfetched (or a pre-switcher mount race) renders nothing — never a
 * guessed `default`.
 */
const accountBadge = computed(() => {
  const id = store.activeAccount
  if (!id) return ''
  const row = store.accounts.find((a) => a.id === id)
  return row?.name ? `${id} · ${row.name}` : id
})

const round = computed(() => snap.value?.round ?? null)

// ── 当前轮次跟随显示账户（像真实余额一样，2026-10-20 操作者指令）────────────
// 全局标量块是各 venue 最早到期的保守钟，多插件（或多族）下通常是先接入
// 那个所/族的钟 —— 显示账户绑着 kalshi 时瓦片却标 polymarket 的 slot，
// 与 HftPage 修过的 issue 550 同一症状。跟随链照抄行情页（issue 550/559
// 建立的口径）：identityVenue（displayVenue 优先，老内核回落账户绑定）
// 有自己的 venueRounds 行时读该行（slot/已过秒随该所的钟），行缺席
// （老内核没有 venueRounds / 该所钟未开口）feature-detect 回落全局块。
const identityVenue = computed<string>(() => {
  const s = snap.value
  if (s?.displayAccount !== undefined || s?.displayVenue !== undefined) {
    return s?.displayVenue ?? ''
  }
  return store.activeAccountVenue
})
const venueRound = computed(() => {
  const v = identityVenue.value
  if (!v) return null
  const rows = snap.value?.round?.venueRounds
  if (!Array.isArray(rows) || rows.length === 0) return null
  return rows.find((r) => r.venue === v) ?? null
})
const roundDisplay = computed(() => {
  const vr = venueRound.value
  if (vr) return { slot: vr.slot, ageSec: vr.ageSec }
  return round.value
})
/**
 * 混钟尾（审计 2026-10-20）：瓦片副行的「剩余」此前永远读全局块的
 * timeLeftSec —— 多族（或多插件）下全局块是各 venue 最早到期的保守钟，
 * 4h 族的瓦片会倒计 15m 族的剩余，同一瓦片里 slot 随身份所、剩余随别家。
 * 修法与 roundDisplay 同一回落链：身份所有 venueRounds 行读行的
 * timeLeftSec（B1a 增量字段同一行），行缺席（老内核无 venueRounds / 该所
 * 钟未开口）feature-detect 回落全局块 —— 单族/老内核逐字节不变。
 */
const roundTimeLeft = computed(() => {
  const vr = venueRound.value
  if (vr) return vr.timeLeftSec
  return round.value?.timeLeftSec ?? 0
})
/**
 * 「可交易」徽章同病：canTrade 是全局「任何一族窗口开着」的或值，多族下
 * 不能标到身份所头上。身份所有自己的钟行时，窗口判定按该行的剩余秒
 * （行在场即该所钟已开口；timeLeftSec>0 = 该所窗口开着）——与 trading 侧
 * per-candidate 门同一口径。行缺席回落全局值（老行为，老内核逐字节不变）。
 */
const roundCanTrade = computed(() => {
  const vr = venueRound.value
  if (vr) return vr.timeLeftSec > 0
  return round.value?.canTrade ?? false
})
/**
 * 轮次瓦片的身份副标（真线报告 2026-10-24：切户后瓦片「看不出跟随」）。
 * 数据层跟随链本身在工作（切户后快照 displayVenue 正确翻转、瓦片读身份
 * 所的 venueRounds 行），但当前生产只有 900s 单族——两所行数字逐字节相同
 * （同一 (venue,duration) 纪元取整必然同值），加上两账户均零成交，切户在
 * 数字上不可见；瓦片标签又写死「当前轮次」四字，没有任何标识说明这口钟
 * 属于谁。补一个与 balanceTitle/accountBadge 同口径的身份标注：身份所在
 * 场（displayVenue 或账户绑定）时标出所名（+ 族时长，行带 durationSec
 * 时），行缺席/老内核回落全局块时如实标「全局」——不猜。
 */
const roundTileTitle = computed(() => {
  const v = identityVenue.value
  const dur = venueRound.value?.durationSec
  if (v) return dur ? `当前轮次 · ${v} · ${Math.round(dur / 60)}m` : `当前轮次 · ${v}`
  return '当前轮次 · 全局'
})
// E28 (§9.5) + issue 561: the 当前持仓 table is scoped to the display account
// (same filter as 行情页; unattributed rows from a pre-E28 core stay visible).
// issue 561（裁决 1）后余额瓦片同样是账户自有读数（account.list 行）——
// 上方「余额瓦片故意不按账户过滤」的旧口径随进程级换源一并废除。
const positions = computed(() =>
  filterByAccount(snap.value?.positions ?? [], store.activeAccount).rows,
)
const engine = computed(() => snap.value?.stats ?? {})

/**
 * 引擎计数卡的口径标注（真线报告 2026-10-24：切户后「引擎计数没跟随账户」）。
 * 数据链定谳（为什么切不动）：
 *   快照 stats 链 = 内核 CoreStats（service.rs，进程内单份、自 boot 饱和累计，
 *   books/rounds 在 on_book/on_rollover 事件路径上自增、evaluations/signals
 *   在评估循环上自增，均不带 accountId 维度）→ IpcClient::stats（空参
 *   engine.stats）→ UiSnapshot.stats（EngineStatsView）→ 网关 render_json_full
 *   原样透传 → /api/snapshot → Overview 的 counters。内核唯一带账户维的
 *   engine.stats 面是 `strategies` 行（engine.stats?accountId= 的
 *   strategy_stats_for_account 切片），顶层的 books/rounds/evaluations/
 *   signals/placeRejected 是进程级事件计数，切片在内核侧就不成立 —— 强行
 *   按 venue 摊分是把一个事实掰成五个错数。所以修的是归属可见性而不是
 *   数字：issue 592 的死字「进程级」升级为身份标注（与 roundTileTitle/
 *   balanceTitle 同口径）—— 显示账户在场时标出「进程级 · {account}」，
 *   副行点明数字属于内核进程而非该账户；账户未定（快照/账户簿未到的一
 *   拍）回落「进程级」——不猜。未平仓浮动合计仍是例外：读上方已按账户
 *   过滤的持仓集，是账户切片读数。
 */
const engineCounterScope = computed(() => {
  const id = store.activeAccount
  return id ? `进程级 · ${id}` : '进程级'
})
/** 卡标题：静态四字 + 口径尾巴，与 roundTileTitle 的「当前轮次 · {venue}」
 * 同一形态 —— 标题自己说出口径，不依赖角落里的灰字被看见。 */
const engineCounterLabel = computed(() => `引擎计数 · ${engineCounterScope.value}`)

/**
 * Feed counters, all cumulative since core boot.
 *
 * `tops` is deliberately absent. The wire format and the `market_api` ABI both
 * offer a `TopOfBook` event, but the only shipping market plugin (Polymarket,
 * `extensions/polymarket/src/feed.rs`) calls `on_book` / `on_spot` and never
 * `on_top_of_book` — so the counter is structurally 0, and a tile that always
 * reads 0 invites the question it cannot answer. It stays in the snapshot for
 * plugins that do emit it.
 */
const counters = computed(() => [
  { label: '订单簿', value: compact(engine.value.books), tone: 'default' as const },
  { label: '轮次', value: num(engine.value.rounds), tone: 'default' as const },
  { label: '评估', value: compact(engine.value.evaluations), tone: 'default' as const },
  { label: '信号', value: compact(engine.value.signals), tone: 'gold' as const },
  {
    // `placeRejected`: refusals inside the order path, not the sum of the gate
    // counters beside it — see `lib/rejections.ts`. Labelled by where it happens
    // so it is not read as "risk control blocked everything else".
    label: '下单被拒',
    value: compact(engine.value.placeRejected),
    tone: (engine.value.placeRejected ?? 0) > 0 ? ('down' as const) : ('default' as const),
  },
])

// 浮动合计读的是上面已按账户过滤后的持仓集合（数字与当前持仓表同口径）。
const unrealized = computed(() =>
  positions.value.reduce((a, p) => a + (Number(p.unrealizedPct) || 0) * (Number(p.shares) || 0), 0),
)

/**
 * 新手线索（issue 381 产品化）：面板在跑但还没有任何启用的策略 —— 数据已经能看，
 * 机器人还不能动。一条安静的指路行，与首次引导卡同一规则：指向下一步该去
 * 的页（主导航「策略」项），不自作主张改成别的话术。
 *
 * 「启用」的口径 = (账户,策略) 复合键（issue 552 审计）：非 default 账户
 * 下读 `accounts` 字段（该策略有 live scope = 启用集点名了它），行不带
 * `accounts`（老内核/降级面）才回落 master-power 位 `enabled` —— 全局
 * 开关关着、账户启用集点名了的策略是真在跑的，只读 `enabled` 会误报。
 */
const needsStrategyHint = computed(() => {
  if (snap.value === null) return false
  const rows = strategyRows.value
  const anyOn = rows.some((r) =>
    Array.isArray(r.accounts) ? r.accounts.length > 0 : Boolean(r.enabled),
  )
  return !anyOn
})
</script>

<template>
  <template v-if="snap">
    <!--
      警示横幅在驾驶舱最顶上（2026-10-28 运营者裁决：挪到 page 最上方、低于
      App header）——交易安全面先于一切数据瓦片进视线；KPI 行从「第一眼」退
      为「横幅之下」。横幅自身的分级/钉住/自动消失语义见 lib/safety.ts 与上
      方脚本块，此处只管摆位。
    -->
    <div v-if="connBanner && connBannerVisible" class="mb-3.5">
      <AlertBanner
        tone="warn"
        :title="connBanner.title"
        :hint="connBanner.hint"
        dismissible
        :auto-dismiss-ms="AUTO_DISMISS_MS"
        :auto-key="bannerAutoKey(connBanner.identity)"
        @dismiss="connBanner.identity && dismiss(connBanner.identity)"
      >{{ connBanner.body }}</AlertBanner>
    </div>
    <div v-if="freeze" class="mb-3.5">
      <!-- 紧急：交易此刻停了 —— 红色、钉住、无 ✕（藏掉唯一的红 = 藏掉事故） -->
      <AlertBanner tone="error" :title="freeze.title" :hint="freeze.hint">{{ freeze.body }}</AlertBanner>
    </div>
    <div v-else-if="lastError && lastErrorVisible" class="mb-3.5">
      <AlertBanner
        tone="warn"
        :title="lastError.title"
        :hint="lastError.hint"
        dismissible
        :auto-dismiss-ms="AUTO_DISMISS_MS"
        :auto-key="bannerAutoKey(lastError.identity)"
        @dismiss="lastError.identity && dismiss(lastError.identity)"
      >
        <code v-if="lastError.code" class="mr-1.5 font-semibold">{{ lastError.code }}</code>{{ lastError.body }}
      </AlertBanner>
    </div>
    <div v-if="selfCheck" class="mb-3.5">
      <!-- 紧急：自检失败伴随冻结面 —— 同冻结一样钉住 -->
      <AlertBanner tone="error" :title="selfCheck.title">
        {{ selfCheck.body }}
        <span v-for="item in selfCheck.items" :key="item.name" class="block">
          {{ item.name }}：{{ item.detail }}
        </span>
      </AlertBanner>
    </div>
    <div v-if="archiveStopped && archiveStoppedVisible" class="mb-3.5">
      <AlertBanner
        tone="warn"
        :title="archiveStopped.title"
        :hint="archiveStopped.hint"
        dismissible
        :auto-dismiss-ms="AUTO_DISMISS_MS"
        :auto-key="bannerAutoKey(archiveStopped.identity)"
        @dismiss="archiveStopped.identity && dismiss(archiveStopped.identity)"
      >
        {{ archiveStopped.body }}
      </AlertBanner>
    </div>

    <!-- beginner hint: the panel is live but the robot is not yet allowed to act -->
    <div v-if="needsStrategyHint" class="mb-3.5 flex items-center gap-2 rounded-lg border border-dashed border-line bg-panel-2 px-3 py-2 text-[12px] text-muted-fg">
      <CircleDot class="size-3.5 shrink-0 text-primary" />
      <span>
        机器人还没有启用任何策略 —— 到
        <span class="font-semibold text-fg">策略</span>
        页内打开开关后，成交与裁决会出现在这里。
      </span>
    </div>

    <!-- KPI row -->
    <div class="grid gap-3.5 sm:grid-cols-2 xl:grid-cols-4">
      <StatTile
        :label="balanceTitle"
        :value="recon.equity !== null ? money(recon.equity) : money(recon.cash)"
        :tone="isDry ? 'gold' : 'default'"
      >
        <template #sub>
          <!--
            The row above is 本金 ＋ 净利润. State that arithmetic, because
            a reader who expects the core's cash number here needs to know which
            figure they are looking at and why the two differ.
          -->
          <Tooltip
            v-if="recon.equity !== null"
            :content="`真实余额 = 本金 ${money(recon.seed)} ＋ 已实现净利 ${signedMoney(recon.net)}。净利是扣费后的数字：毛利 ${money(recon.gross)} 减去手续费 ${money(recon.fees)}。手续费是成本，不改变本金，因此不并入余额。`"
          >
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              本金 {{ money(recon.seed) }} ＋ 净利 {{ signedMoney(recon.net) }}
            </span>
          </Tooltip>
          <!--
            No principal on the wire (LIVE, or a core older than the field): the
            equity is not computable, so show the cash ledger and say plainly that
            the principal is missing rather than inventing an equation.
            issue 561: the cash number is the DISPLAY account's own book
            (account.list row) once the rows have arrived.
          -->
          <Tooltip v-else-if="isDry && store.activeAccountRow" content="账户未上报本金，无法计算「本金 ＋ 净利」的真实余额，这里显示该账户自有现金账。本金由内核按 --seed-balance 持有；当前运行中的内核早于该字段，重启内核后即可显示真实余额。">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              内核现金账 · 本金未上报
            </span>
          </Tooltip>
          <template v-else>
            可用 <RollingNumber :value="money(recon.available)" /> · 预留 <RollingNumber :value="money(recon.reserved)" />
          </template>
        </template>

        <!-- Fees are a cost: shown against gross profit, never inside the balance. -->
        <div
          v-if="recon.equity !== null && recon.fees > 0"
          class="mt-2 flex items-center gap-1.5 text-[10.5px] leading-snug text-faint-fg"
        >
          <Tooltip :content="`手续费是本次运行的开支：毛利 ${money(recon.gross)} − 手续费 ${money(recon.fees)} = 净利 ${signedMoney(recon.net)}。它从利润里扣，不影响本金。`">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              手续费支出 {{ money(recon.fees) }} · 毛利 {{ money(recon.gross) }}
            </span>
          </Tooltip>
        </div>

        <!--
          No cash-ledger footnote in DRY: 本金 ＋ 净利润 above is already the real
          balance, and the core's ledger is only 本金 ＋ 本次会话净利, so showing
          both forced a per-page explanation of a difference that carries nothing
          the trade log does not already record.
        -->
        <div v-if="isDry" class="mt-2 flex items-center gap-1.5 text-[10.5px] text-faint-fg">
          <CircleDot class="size-3" /> 模拟资金，非真实资产
          <template v-if="accountBadge">
            <span class="opacity-50">·</span>
            <!-- 账户标识符不是读数（不滚动），故不进 num 槽位（round-header 门禁）。 -->
            <span>{{ accountBadge }}</span>
          </template>
        </div>
        <div v-else-if="accountBadge || walletAddr" class="mt-2 text-[11px] text-faint-fg">
          <!-- 账户标识符不是读数（不滚动），不进 num 槽位（round-header 门禁）；
               num 只包钱包地址这一标识符（TEXT_MARKERS 豁免 shortAddr）。 -->
          <span v-if="accountBadge">{{ accountBadge }}</span>
          <span v-if="accountBadge && walletAddr" class="opacity-50"> · </span>
          <span v-if="walletAddr" class="num">{{ shortAddr(walletAddr) }}</span>
          <!-- 钱包地址目前是快照顶层单值（进程级身份）：显示账户带 venue
               绑定时如实标注，免得读成「每个账户各有一个地址」。 -->
          <span v-if="walletAddr && store.activeAccountVenue" class="opacity-50">
            · {{ store.activeAccountVenue }} 钱包（进程级单值）
          </span>
        </div>
      </StatTile>

      <StatTile :label="roundTileTitle" :value="roundDisplay ? `#${roundDisplay.slot}` : '—'">
        <template #sub>
          <span v-if="roundDisplay">{{ duration(roundDisplay.ageSec) }} 已过 · {{ duration(roundTimeLeft) }} 剩余</span>
          <!-- issue 488: 行情插件活跃但轮次空 = 数据源没到达（网络/代理），不是「等待插件」。 -->
          <span v-else-if="snap?.marketActiveName">行情插件 {{ snap?.marketActiveName }} 活跃，等待轮次数据</span>
          <span v-else>等待行情插件</span>
        </template>
        <div v-if="roundDisplay" class="mt-2">
          <Badge :variant="roundCanTrade ? 'up' : 'gold'" dot>{{ roundCanTrade ? '可交易' : '等待窗口' }}</Badge>
        </div>
      </StatTile>

      <StatTile
        label="已平仓交易"
        :value="num(trades.count)"
        :tone="trades.net >= 0 ? 'up' : 'down'"
      >
        <template #sub>
          扣费净利 <RollingNumber :value="signedMoney(trades.net)" /> · 费用 <RollingNumber :value="money(trades.fees)" />
        </template>
        <div class="mt-2 flex items-center gap-2">
          <Badge variant="up"><ArrowUpRight class="size-3" /><RollingNumber :value="trades.win" /></Badge>
          <Badge variant="down"><ArrowDownRight class="size-3" /><RollingNumber :value="trades.loss" /></Badge>
          <span class="text-[11px] text-faint-fg num">胜率 <RollingNumber :value="pct(trades.winRate)" /></span>
        </div>
      </StatTile>

      <!-- Words, not figures: DRY / LIVE never rolls, so it stays plain text. -->
      <StatTile
        label="运行模式"
        :value="(snap.mode ?? '—').toUpperCase()"
        :roll="false"
        :tone="snap.mode === 'dry' ? 'gold' : 'up'"
      >
        <template #sub>
          {{ snap.connected ? '引擎已连接' : '引擎未连接' }}
          <template v-if="positions.length"> · <RollingNumber :value="positions.length" /> 持仓</template>
        </template>
        <div class="mt-2 flex items-center gap-1.5">
          <ShieldCheck class="size-3.5" :style="{ color: snap.mode === 'dry' ? 'var(--primary)' : 'var(--up)' }" />
          <span class="text-[10.5px] text-faint-fg">
            {{ snap.mode === 'live' ? '实盘已连接' : 'Live 交易未启用' }}
          </span>
        </div>
      </StatTile>
    </div>

    <!-- equity + counters -->
    <div class="mt-3.5 grid gap-3.5 xl:grid-cols-[1.55fr_1fr]">
      <Card>
        <CardHeader label="权益曲线">
          <template #title>
            <span class="stat-num text-[15px]" :class="trades.net >= 0 ? 'text-up' : 'text-down'">
              <RollingNumber :value="signedMoney(trades.net)" />
            </span>
          </template>
          <template #action>
            <Badge variant="default" dot><RollingNumber :value="trades.count" /> 笔</Badge>
          </template>
        </CardHeader>
        <EquityCurve :rows="equityCurveRows" :height="196" />
      </Card>

      <Card>
        <CardHeader :label="engineCounterLabel">
          <!-- 引擎计数是内核进程级事实（feed/评估/信号自 boot 起累计），无
               账户切片 —— 与余额/持仓的账户自有读数不同源，如实标注归属
               （engineCounterScope：显示账户在场时点名账户，免得读成该账
               户自己的计数）。未平仓浮动合计是例外：它读上方已按账户过滤
               的持仓集，是账户切片读数。 -->
          <template #action>
            <span class="text-[10px] text-faint-fg">{{ engineCounterScope }}</span>
          </template>
        </CardHeader>
        <div class="grid grid-cols-2 gap-x-5 gap-y-3.5 sm:grid-cols-3 xl:grid-cols-2">
          <div v-for="c in counters" :key="c.label" class="min-w-0">
            <div class="label-micro">{{ c.label }}</div>
            <div
              class="stat-num mt-1 text-[19px] leading-none"
              :class="{
                'text-gold': c.tone === 'gold',
                'text-down': c.tone === 'down',
              }"
            ><RollingNumber :value="c.value" /></div>
          </div>
        </div>
        <div
          v-if="unrealized"
          class="mt-4 flex items-center justify-between border-t border-line pt-3 text-[11.5px]"
        >
          <span class="text-faint-fg">未平仓浮动合计</span>
          <span class="stat-num" :class="unrealized >= 0 ? 'text-up' : 'text-down'">
            <RollingNumber :value="signedMoney(unrealized)" />
          </span>
        </div>
      </Card>
    </div>

    <!-- rejection attribution + positions -->
    <div class="mt-3.5 grid gap-3.5 xl:grid-cols-2">
      <Card>
        <CardHeader label="策略拒单原因分布">
          <template #action>
            <Badge variant="default"><RollingNumber :value="strategyRows.length" /> 策略</Badge>
          </template>
        </CardHeader>
        <RejectionChart :rows="strategyRows" :height="210" />
      </Card>

      <Card dense>
        <CardHeader label="当前持仓">
          <template #action>
            <Badge :variant="positions.length ? 'gold' : 'default'"><RollingNumber :value="positions.length" /></Badge>
          </template>
        </CardHeader>
        <div v-if="positions.length" class="-mx-1 overflow-x-auto">
          <table class="w-full text-[13px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">资产</th>
                <th class="label-micro px-2 pb-2">方向</th>
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
                v-for="(p, i) in positions"
                :key="`${p.asset}-${i}`"
                class="border-t border-line transition-colors hover:bg-panel-2"
              >
                <td class="px-2 py-2.5 font-semibold">{{ p.asset }}</td>
                <td class="px-2 py-2.5">
                  <Badge :variant="p.direction === 'up' ? 'up' : 'down'">
                    {{ p.direction.toUpperCase() }}
                  </Badge>
                </td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="Number(p.entryPrice).toFixed(3)" /></td>
                <td class="px-2 py-2.5 text-right num"><RollingNumber :value="Number(p.currentPrice).toFixed(3)" /></td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="p.shares ?? '—'" /></td>
                <td
                  class="px-2 py-2.5 text-right num font-semibold"
                  :class="p.unrealizedPct >= 0 ? 'text-up' : 'text-down'"
                ><RollingNumber :value="`${Number(p.unrealizedPct) >= 0 ? '+' : ''}${Number(p.unrealizedPct).toFixed(1)}%`" /></td>
                <td class="px-2 py-2.5 text-right num text-faint-fg">
                  <RollingNumber :value="p.remainingSec !== undefined ? duration(p.remainingSec) : '—'" />
                </td>
                <td class="px-2 py-2.5 text-right">
                  <!-- #684：仅已过期（remainingSec <= 0）的行给清除 —— 活轮
                       持仓内核拒绝（那是 positions.exit 的活）。判定在内核，
                       按钮只做确认。 -->
                  <button
                    v-if="(p.remainingSec ?? 1) <= 0"
                    class="text-[11.5px] font-medium text-down underline decoration-dotted decoration-line underline-offset-2 transition-colors hover:opacity-80 disabled:opacity-40"
                    :disabled="clearingId !== null"
                    :title="`清除 ${p.id}（已过期且裁决未到，按兜底价平账并写审计）`"
                    @click="clearExpiredPosition(p.id)"
                  >
                    {{ clearingId === p.id ? '清除中…' : '清除' }}
                  </button>
                  <span v-else class="text-faint-fg">—</span>
                </td>
              </tr>
            </tbody>
          </table>
          <!-- #684 清除反馈：成功/失败都落在表格正下方，行内文案 = 内核回执。 -->
          <p v-if="clearMsg" class="px-2 pt-2 text-[11.5px] text-up">{{ clearMsg }}</p>
          <p v-if="clearErr" class="px-2 pt-2 text-[11.5px] text-down">{{ clearErr }}</p>
        </div>
        <EmptyState v-else text="无持仓" hint="启用策略后，成交平仓即出现在这里。" compact />
      </Card>
    </div>

  </template>

  <Card v-else class="mt-3.5">
    <EmptyState
      :loading="store.loading"
      text="暂无快照数据"
      hint="网关在线时会自动拉取；持续为空请确认 blitzkrieg-core 已启动。"
    />
  </Card>
</template>
