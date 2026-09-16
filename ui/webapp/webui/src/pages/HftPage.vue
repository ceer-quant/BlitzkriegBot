<script setup lang="ts">
/**
 * 行情面板 — 二元预测市场通用模板页（源自 ui/hft.html）。倒计时条、行情卡、
 * 权益曲线、胜率分布、交易统计、持仓/历史瀑布流（全量、过滤器、分页），
 * 以及 hft.html 的提示音（盈/亏/开仓/熔断）。数据全部来自 /api/snapshot。
 */
import { computed, onUnmounted, ref, watch } from 'vue'
import { useIntervalFn, useIntersectionObserver } from '@vueuse/core'
import { Play, Square, Bell, BellOff, Search, TrendingUp, TrendingDown, Clock, Filter } from 'lucide-vue-next'
import { api, marketTypeLabel, type MarketPrice, type TradeRow } from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import { useTheme } from '@/lib/theme'
import {
  num, money, signedMoney, winRatePct, pct, signedPct, cents, mmss, duration, dateTime,
} from '@/lib/format'
import { reconcileBalance } from '@/lib/balance'
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
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import EquityCurve from '@/components/charts/EquityCurve.vue'

const store = usePanelStore()
const { sound, toggleSound } = useTheme()
const snap = computed(() => store.snapshot)

// Fast tick: 2s snapshot poll (HFT pacing), faster than the shell's 15s.
const { pause: stopFast } = useIntervalFn(() => { void store.refresh() }, 2_000)
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

  const err = s.lastError ?? ''
  const halted = err.toLowerCase().includes('breaker')
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

// ── cumulative all-time totals (core summary preferred, rows as fallback) ────
const historyRows = computed<TradeRow[]>(() => store.tradeRows)
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
  return {
    total: rows.length,
    gross: net,
    fees: rows.reduce((x, t) => x + (Number(t.feesUsd) || 0), 0),
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

// ── balance reconciliation (本金 + 净利 vs the core's cash ledger) ────────────
const recon = computed(() => reconcileBalance(snap.value?.balance, cumStats.value.net))

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
  return historyRows.value.filter((t) => {
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

watch([filteredRows], () => { visibleCount.value = PAGE })

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

const tab = ref<'positions' | 'history'>('positions')
const positions = computed(() => snap.value?.positions ?? [])
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
            <span>{{ round?.ageSec ?? '—' }}s 已过</span>
            <span class="opacity-40">·</span>
            <span :class="round?.canTrade ? 'text-up font-semibold' : 'text-primary'">
              {{ round?.canTrade ? 'TRADING' : 'WAITING' }}
            </span>
          </div>
        </div>

        <div class="mx-auto text-center">
          <div
            class="stat-num text-[42px] leading-none tracking-[-0.03em]"
            :class="urgent ? 'text-down' : 'grad-gold'"
          >{{ leftText }}</div>
          <div class="label-micro mt-1">剩余时间</div>
        </div>

        <div class="ml-auto flex items-center gap-2">
          <Tooltip :content="sound ? '关闭提示音' : '开启提示音'">
            <Button variant="ghost" size="icon" @click="toggleSound">
              <Bell v-if="sound" /><BellOff v-else class="opacity-60" />
            </Button>
          </Tooltip>
          <Button variant="up" :disabled="busy" @click="send('start')">
            <Play class="size-3.5" />启动
          </Button>
          <Button variant="danger" :disabled="busy" @click="send('stop')">
            <Square class="size-3.5" />停止
          </Button>
        </div>
      </div>

      <Transition name="fade">
        <div v-if="cmdMsg" class="mt-3 rounded-md border border-line bg-panel-2 px-3 py-2 text-[12px] text-muted-fg">
          {{ cmdMsg }}
        </div>
      </Transition>
    </Card>

    <!-- ── market price cards ────────────────────────────────────────────── -->
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
            <div class="stat-num mt-1 text-[21px] leading-none text-up">{{ Number(m.up).toFixed(3) }}</div>
            <div class="mt-0.5 text-[10px] text-faint-fg num">{{ cents(m.up) }}</div>
          </div>
          <div class="rounded-md border border-down/25 bg-down/8 px-2.5 py-2">
            <div class="flex items-center gap-1 text-[10.5px] font-semibold text-down">
              <TrendingDown class="size-3" />DOWN
            </div>
            <div class="stat-num mt-1 text-[21px] leading-none text-down">{{ Number(m.down).toFixed(3) }}</div>
            <div class="mt-0.5 text-[10px] text-faint-fg num">{{ cents(m.down) }}</div>
          </div>
        </div>
      </div>
    </div>
    <Card v-else class="mt-3.5">
      <EmptyState text="暂无行情数据（等待报价插件）" compact />
    </Card>

    <!-- ── stats row ─────────────────────────────────────────────────────── -->
    <div class="mt-3.5 grid gap-3.5 xl:grid-cols-[1.6fr_1fr_1fr_1fr]">
      <Card>
        <CardHeader label="累计净 PnL">
          <template #title>
            <span class="stat-num text-[16px]" :class="cumStats.net >= 0 ? 'text-up' : 'text-down'">
              {{ signedMoney(cumStats.net) }}
            </span>
          </template>
          <template #action>
            <Badge variant="default">扣费口径</Badge>
          </template>
        </CardHeader>
        <EquityCurve :rows="historyRows" :height="128" :show-axis="false" />
        <div class="mt-2 flex items-center gap-4 text-[11px] text-faint-fg">
          <span>毛利 <span class="num text-fg">{{ signedMoney(cumStats.gross) }}</span></span>
          <span>费用 <span class="num text-down">{{ money(cumStats.fees) }}</span></span>
        </div>
      </Card>

      <Card>
        <CardHeader label="胜率分布" />
        <div class="stat-num text-[30px] leading-none">{{ pct(cumStats.winRate) }}</div>
        <div class="mt-1 flex items-center gap-3 text-[11.5px]">
          <span class="font-semibold text-up">{{ cumStats.wins }} 盈</span>
          <span class="font-semibold text-down">{{ cumStats.losses }} 亏</span>
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
            <span class="w-7 shrink-0 text-right text-[10.5px] text-muted-fg num">{{ b.count }}</span>
          </div>
        </div>
      </Card>

      <Card>
        <CardHeader label="交易统计" />
        <div class="grid grid-cols-2 gap-x-4 gap-y-3">
          <div><div class="label-micro">笔数</div><div class="stat-num mt-1 text-[19px] leading-none">{{ tradeStats.count }}</div></div>
          <div><div class="label-micro">今日</div><div class="stat-num mt-1 text-[19px] leading-none">{{ tradeStats.today }}</div></div>
          <div><div class="label-micro">成交额</div><div class="stat-num mt-1 text-[15px] leading-none">{{ money(tradeStats.volume) }}</div></div>
          <div><div class="label-micro">均笔</div><div class="stat-num mt-1 text-[15px] leading-none">{{ money(tradeStats.avg) }}</div></div>
        </div>
        <div class="mt-3.5 flex items-center justify-between border-t border-line pt-2.5 text-[11.5px]">
          <span class="text-faint-fg">今日净利</span>
          <span class="stat-num" :class="tradeStats.todayNet >= 0 ? 'text-up' : 'text-down'">
            {{ signedMoney(tradeStats.todayNet) }}
          </span>
        </div>
      </Card>

      <Card>
        <CardHeader label="今日表现" />
        <StatRow label="最佳单笔" :value="signedPct(cumStats.best)" tone="up" />
        <StatRow label="最差单笔" :value="signedPct(cumStats.worst)" tone="down" />
        <StatRow label="持仓浮动" :value="signedPct(positionUnrealized)" :tone="positionUnrealized >= 0 ? 'up' : 'down'" />
        <StatRow label="平均持仓" :value="cumStats.avgHold ? duration(cumStats.avgHold) : '—'" tone="dim" />
        <StatRow label="运行模式" :value="(snap.mode ?? '—').toUpperCase()" :tone="isDry ? 'gold' : 'up'" />
        <StatRow label="行情轮次" :value="round?.markets ?? '—'" tone="dim" />
        <div class="mt-2.5 border-t border-line pt-2.5">
          <StatRow
            :label="isDry ? '模拟余额' : '交易所余额'"
            :value="money(snap.balance?.balance)"
            :tone="isDry ? 'gold' : 'default'"
          />
          <p v-if="recon.seed !== null" class="mt-1 text-[10px] leading-snug text-faint-fg">
            本金 {{ money(recon.seed) }} ＋ 已实现净利 {{ signedMoney(recon.net) }} − 未平仓占用 {{ money(recon.reserved) }}
            <Tooltip :content="`余额是内核的现金账：本金随每笔成交与手续费增减，再减去挂单占用。净利来自已平仓记录。两者口径一致时应当吻合；出现差额说明内核的现金流水与盈亏记录不同源（例如进程早于费用入账修复启动，或记录跨越多个进程）。差额 ${signedMoney(recon.residual)}。`">
              <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2" :class="recon.drifted ? 'text-down' : 'text-up'">
                {{ recon.drifted ? `对账差 ${signedMoney(recon.residual)}` : '已对账' }}
              </span>
            </Tooltip>
          </p>
          <!--
            No principal on the wire: either LIVE, or a core older than the field
            that reports it. Say why the two numbers need not add up instead of
            printing an equation the panel cannot verify.
          -->
          <p v-else-if="isDry" class="mt-1 text-[10px] leading-snug text-faint-fg">
            <Tooltip content="余额是内核的现金账：本金随每笔成交与手续费增减，再减去挂单占用。要显示对账结果，内核必须上报本金；当前运行中的内核未上报本金，因此这里只标注口径、不虚构等式。重启内核后即可显示完整对账。">
              <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
                本进程现金账 · 本金未上报
              </span>
            </Tooltip>
          </p>
        </div>
      </Card>
    </div>

    <!-- ── positions / history ──────────────────────────────────────────── -->
    <Card class="mt-3.5" dense>
      <div class="mb-3.5 flex flex-wrap items-center gap-3">
        <SegmentedControl
          v-model="tab"
          :segments="[
            { id: 'positions', label: '当前持仓', badge: positions.length },
            { id: 'history', label: '历史订单', badge: cumStats.total },
          ]"
          size="sm"
        />
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
            <Search class="size-3.5" />重置
          </Button>
        </div>
      </div>

      <!-- cumulative header + filtered subtotal -->
      <template v-if="tab === 'history'">
        <div class="mb-3.5 grid grid-cols-2 gap-3 rounded-lg border border-line bg-panel-2 p-3 sm:grid-cols-4">
          <div>
            <div class="label-micro">累计订单</div>
            <div class="stat-num mt-1 text-[22px] leading-none">{{ cumStats.total }}</div>
          </div>
          <div>
            <div class="label-micro">累计利润（扣费）</div>
            <div class="stat-num mt-1 text-[22px] leading-none" :class="cumStats.net >= 0 ? 'text-up' : 'text-down'">
              {{ signedMoney(cumStats.net) }}
            </div>
          </div>
          <div>
            <div class="label-micro">累计胜率</div>
            <div class="stat-num mt-1 text-[22px] leading-none">{{ pct(cumStats.winRate) }}</div>
          </div>
          <div>
            <div class="label-micro">盈利 / 亏损</div>
            <div class="stat-num mt-1 text-[22px] leading-none">
              <span class="text-up">{{ cumStats.wins }}</span>
              <span class="text-faint-fg"> / </span>
              <span class="text-down">{{ cumStats.losses }}</span>
            </div>
          </div>
        </div>
        <p v-if="!cumStats.fromSummary" class="mb-3 text-[10.5px] text-faint-fg">
          累计值由已加载的行汇总（旧内核未提供全史 summary）。
        </p>
        <div v-if="hasFilters" class="mb-3 flex items-center gap-2 text-[11.5px] text-faint-fg">
          <Filter class="size-3.5" />
          已筛选 <span class="num text-fg">{{ filteredStats.count }}</span> 笔 ·
          净利 <span class="stat-num" :class="filteredStats.net >= 0 ? 'text-up' : 'text-down'">{{ signedMoney(filteredStats.net) }}</span>
          · 盈 <span class="text-up num">{{ filteredStats.wins }}</span>
          / 亏 <span class="text-down num">{{ filteredStats.losses }}</span>
        </div>
      </template>

      <!-- positions -->
      <div v-if="tab === 'positions'">
        <div v-if="positions.length" class="overflow-x-auto">
          <table class="w-full text-[13px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">资产</th>
                <th class="label-micro px-2 pb-2">方向</th>
                <th class="label-micro px-2 pb-2">策略</th>
                <th class="label-micro px-2 pb-2 text-right">入场</th>
                <th class="label-micro px-2 pb-2 text-right">现价</th>
                <th class="label-micro px-2 pb-2 text-right">份额</th>
                <th class="label-micro px-2 pb-2 text-right">浮动</th>
                <th class="label-micro px-2 pb-2 text-right">剩余</th>
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
                  <Badge :variant="p.direction === 'up' ? 'up' : 'down'">{{ p.direction.toUpperCase() }}</Badge>
                </td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ p.strategy ?? '—' }}</td>
                <td class="px-2 py-2.5 text-right num text-muted-fg">{{ Number(p.entryPrice).toFixed(3) }}</td>
                <td class="px-2 py-2.5 text-right num">{{ Number(p.currentPrice).toFixed(3) }}</td>
                <td class="px-2 py-2.5 text-right num text-muted-fg">{{ p.shares ?? '—' }}</td>
                <td class="px-2 py-2.5 text-right num font-semibold" :class="p.unrealizedPct >= 0 ? 'text-up' : 'text-down'">
                  {{ Number(p.unrealizedPct) >= 0 ? '+' : '' }}{{ Number(p.unrealizedPct).toFixed(2) }}%
                </td>
                <td class="px-2 py-2.5 text-right num text-faint-fg">
                  {{ p.remainingSec !== undefined ? duration(p.remainingSec) : '—' }}
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <EmptyState v-else text="无持仓" compact />
      </div>

      <!-- history waterfall -->
      <div v-else>
        <div v-if="visibleRows.length" class="overflow-x-auto">
          <table class="w-full text-[13px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">资产</th>
                <th class="label-micro px-2 pb-2">方向</th>
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
              <tr
                v-for="t in visibleRows"
                :key="t.id"
                class="border-t border-line transition-colors hover:bg-panel-2"
              >
                <td class="px-2 py-2.5 font-semibold">{{ t.asset }}</td>
                <td class="px-2 py-2.5">
                  <Badge :variant="t.direction === 'up' ? 'up' : 'down'">{{ t.direction.toUpperCase() }}</Badge>
                </td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ t.strategy ?? '—' }}</td>
                <td class="px-2 py-2.5 text-right num text-muted-fg">{{ Number(t.entryPrice).toFixed(3) }}</td>
                <td class="px-2 py-2.5 text-right num text-muted-fg">{{ Number(t.exitPrice).toFixed(3) }}</td>
                <td class="px-2 py-2.5 text-right num text-muted-fg">{{ t.shares }}</td>
                <td class="px-2 py-2.5 text-right num text-down">
                  {{ t.feesUsd ? money(t.feesUsd, 3) : '—' }}
                </td>
                <td class="px-2 py-2.5 text-right num font-semibold" :class="Number(t.netPnlUsd) >= 0 ? 'text-up' : 'text-down'">
                  {{ signedMoney(t.netPnlUsd, 2) }}
                </td>
                <td class="px-2 py-2.5 text-right num" :class="Number(t.netPnlPct ?? 0) >= 0 ? 'text-up' : 'text-down'">
                  {{ signedPct(t.netPnlPct, 2) }}
                </td>
                <td class="px-2 py-2.5 text-right num text-faint-fg">
                  {{ t.holdTimeSec !== undefined ? duration(t.holdTimeSec) : '—' }}
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
        <EmptyState v-else text="无匹配订单" compact />

        <!-- waterfall sentinel + fallback button -->
        <div ref="sentinel" class="h-px" />
        <div v-if="hasMore" class="mt-3 flex flex-col items-center gap-2">
          <Button variant="outline" size="sm" @click="visibleCount += PAGE">加载更多</Button>
          <span class="text-[10.5px] text-faint-fg num">
            已显示 {{ visibleRows.length }} / {{ filteredRows.length }}
          </span>
        </div>
        <p v-else-if="filteredRows.length > PAGE" class="mt-3 text-center text-[10.5px] text-faint-fg">
          已加载全部 {{ filteredRows.length }} 笔
        </p>
      </div>
    </Card>
  </template>

  <Card v-else class="rise-in">
    <EmptyState :loading="store.loading" text="暂无快照数据" />
  </Card>
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
