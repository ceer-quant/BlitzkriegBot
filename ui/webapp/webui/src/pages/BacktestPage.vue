<script setup lang="ts">
/**
 * 回放复盘（E8-c）— 载入 blitzkrieg-core `--backtest` 产出的报告 JSON，
 * 用 ECharts 呈现权益曲线、单笔盈亏分布与分策略归因，并列出风控告警/错误。
 * 报告持久化在 localStorage，刷新不丢；可随时替换或清除。
 */
import { computed, onMounted, ref } from 'vue'
import type { EChartsOption } from 'echarts'
import { Upload, Trash2, FileJson, TrendingDown, TrendingUp, AlertTriangle, ShieldAlert, ShieldOff } from 'lucide-vue-next'
import { backtestLabel, loadBacktest, saveBacktest, clearBacktest } from '@/composables/backtestStore'
import type { BacktestReport } from '@/backtest'
import { useChart, areaFade, palette, tooltipStyle, axisX, axisY } from '@/lib/chart'
import { num, money, signedMoney, pct, duration, dateTime } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import StatTile from '@/components/ui/stat/StatTile.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'

const fileEl = ref<HTMLInputElement | null>(null)
const report = ref<BacktestReport | null>(null)
const loadErr = ref<string | null>(null)

// ── derived series ──────────────────────────────────────────────────────────
/** Cumulative net PnL over tradeLines (oldest → newest), with a 0 origin. */
const equity = computed<number[]>(() => {
  const r = report.value
  if (!r) return []
  let cum = 0
  return [0, ...r.tradeLines.reduce<number[]>((acc, t) => {
    cum += Number(t.netPnlUsd) || 0
    acc.push(cum)
    return acc
  }, [])]
})

/** Kernel-authoritative realized net PnL (fees already deducted). */
const netPnl = computed(() => Number(report.value?.trades?.netPnlUsd ?? 0))
const positive = computed(() => netPnl.value >= 0)

/** Trades past the `tradeLines` cap — the curve is plotted from a truncated list. */
const truncated = computed(() => Number(report.value?.tradeLinesTruncated ?? 0))

const perTrade = computed(() => (report.value?.tradeLines ?? []).map((t) => Number(t.netPnlUsd) || 0))

const strategyAgg = computed(() => {
  const r = report.value
  if (!r) return []
  return r.strategies
    .map((s) => ({
      name: s.name,
      net: Number(s.netPnlUsd ?? 0),
      fees: Number(s.feesUsd ?? 0),
      closed: Number(s.closedTrades ?? 0),
      wins: Number(s.wins ?? 0),
      losses: Number(s.losses ?? 0),
    }))
    .sort((a, b) => b.net - a.net)
})

/** Gate rejections attributed per strategy (`blocked.byStrategy`). */
const blockedRows = computed(() => {
  const map = report.value?.blocked?.byStrategy ?? {}
  return Object.entries(map)
    .map(([name, v]) => ({
      name,
      momentum: Number(v?.momentum ?? 0),
      timing: Number(v?.timing ?? 0),
    }))
    .filter((r) => r.momentum + r.timing > 0)
    .sort((a, b) => b.momentum + b.timing - (a.momentum + a.timing))
})

const blockedTotal = computed(() => blockedRows.value.reduce((a, r) => a + r.momentum + r.timing, 0))
const exemptions = computed(() => report.value?.blocked?.declaredExemptions ?? [])

const worstTrade = computed(() => {
  const rows = report.value?.tradeLines ?? []
  if (!rows.length) return null
  return rows.reduce((w, t) => (Number(t.netPnlUsd) < Number(w.netPnlUsd) ? t : w))
})
const bestTrade = computed(() => {
  const rows = report.value?.tradeLines ?? []
  if (!rows.length) return null
  return rows.reduce((b, t) => (Number(t.netPnlUsd) > Number(b.netPnlUsd) ? t : b))
})

/** Per-strategy rows sorted by trade count — the attribution table. */
const strategyTable = computed(() => [...strategyAgg.value].sort((a, b) => b.closed - a.closed))

// ── charts ──────────────────────────────────────────────────────────────────
const eqEl = ref<HTMLDivElement | null>(null)
const distEl = ref<HTMLDivElement | null>(null)
const stratEl = ref<HTMLDivElement | null>(null)

const equityOption = computed<EChartsOption>(() => {
  const p = palette()
  const pts = equity.value
  const color = positive.value ? p.up : p.down
  return {
    animationDuration: 560,
    animationEasing: 'cubicOut',
    grid: { left: 58, right: 14, top: 14, bottom: 26 },
    tooltip: {
      trigger: 'axis',
      ...tooltipStyle(),
      valueFormatter: (v) => signedMoney(Number(v)),
    },
    xAxis: { ...axisX(pts.map((_, i) => `#${i}`)), boundaryGap: false, axisLine: { show: false } },
    yAxis: axisY(),
    series: [{
      type: 'line',
      data: pts,
      showSymbol: false,
      smooth: 0.35,
      lineStyle: { width: 2.2, color, shadowColor: color, shadowBlur: 12, shadowOffsetY: 3 },
      areaStyle: areaFade(color, positive.value ? '4d' : '40'),
      markLine: pts.length > 1
        ? {
            silent: true,
            symbol: 'none',
            label: { show: false },
            lineStyle: { color: p.axis, type: 'dashed', width: 1 },
            data: [{ yAxis: 0 }],
          }
        : undefined,
    }],
  }
})

const distOption = computed<EChartsOption>(() => {
  const p = palette()
  const vals = perTrade.value
  const bins = 21
  let min = 0
  let max = 0
  if (vals.length) {
    min = Math.min(...vals)
    max = Math.max(...vals)
  }
  const span = max - min || 1
  const w = span / bins
  const counts = new Array(bins).fill(0)
  for (const v of vals) {
    const i = Math.min(bins - 1, Math.max(0, Math.floor((v - min) / w)))
    counts[i]++
  }
  return {
    animationDuration: 520,
    grid: { left: 44, right: 14, top: 14, bottom: 40 },
    tooltip: {
      trigger: 'axis',
      ...tooltipStyle(),
      formatter: (params: unknown) => {
        const arr = params as { dataIndex: number; value: number }[]
        const i = arr?.[0]?.dataIndex ?? 0
        const lo = min + i * w
        return `${signedMoney(lo)} ~ ${signedMoney(lo + w)}<br/><b>${arr?.[0]?.value ?? 0}</b> 笔`
      },
    },
    xAxis: {
      type: 'category',
      data: counts.map((_, i) => signedMoney(min + i * w, 2)),
      axisLine: { lineStyle: { color: p.axis } },
      axisTick: { show: false },
      axisLabel: { color: p.textDim, fontSize: 9.5, rotate: 50, interval: 2 },
    },
    yAxis: { ...axisY(), minInterval: 1 },
    series: [{
      type: 'bar',
      data: counts.map((v, i) => ({
        value: v,
        itemStyle: {
          color: min + (i + 0.5) * w >= 0 ? p.up : p.down,
          borderRadius: [3, 3, 0, 0],
        },
      })),
      barCategoryGap: '18%',
    }],
  }
})

const stratOption = computed<EChartsOption>(() => {
  const p = palette()
  const rows = strategyAgg.value
  const vals = rows.map((r) => r.net)
  const hi = Math.max(0, ...vals)
  const lo = Math.min(0, ...vals)
  return {
    animationDuration: 520,
    grid: { left: 128, right: 64, top: 8, bottom: 8 },
    tooltip: { trigger: 'axis', axisPointer: { type: 'shadow' }, ...tooltipStyle() },
    xAxis: {
      type: 'value',
      show: false,
      // Pad both directions so a bar tip never reaches the edge and its label
      // always has room (an all-negative set otherwise pins the tip to the axis max).
      min: lo < 0 ? lo * 1.35 : undefined,
      max: hi > 0 ? hi * 1.35 : undefined,
    },
    yAxis: {
      type: 'category',
      inverse: true,
      data: rows.map((r) => r.name),
      axisLine: { show: false },
      axisTick: { show: false },
      axisLabel: { color: p.textDim, fontSize: 11 },
    },
    series: [{
      type: 'bar',
      data: rows.map((r) => ({
        value: r.net,
        itemStyle: { borderRadius: r.net >= 0 ? [0, 5, 5, 0] : [5, 0, 0, 5], color: r.net >= 0 ? p.up : p.down },
        label: {
          show: true,
          position: r.net >= 0 ? 'right' : 'left',
          color: p.textDim,
          fontSize: 10.5,
          formatter: () => signedMoney(r.net),
        },
      })),
      barMaxWidth: 16,
    }],
  }
})

useChart(eqEl, () => equityOption.value)
useChart(distEl, () => distOption.value)
useChart(stratEl, () => stratOption.value)

// ── file IO ─────────────────────────────────────────────────────────────────
function pickFile(): void {
  fileEl.value?.click()
}

async function onFile(e: Event): Promise<void> {
  loadErr.value = null
  const inp = e.target as HTMLInputElement
  const f = inp.files?.[0]
  if (!f) return
  try {
    const parsed = JSON.parse(await f.text()) as BacktestReport
    if (!parsed || typeof parsed.source !== 'string' || !parsed.trades) {
      loadErr.value = '不是有效的回测报告（缺少 source / trades 字段）'
      return
    }
    if (parsed.tradeLines && !Array.isArray(parsed.tradeLines)) parsed.tradeLines = []
    if (!Array.isArray(parsed.tradeLines)) parsed.tradeLines = []
    if (!Array.isArray(parsed.strategies)) parsed.strategies = []
    if (!Array.isArray(parsed.riskAlerts)) parsed.riskAlerts = []
    if (!Array.isArray(parsed.errors)) parsed.errors = []
    if (!parsed.blocked || typeof parsed.blocked !== 'object') parsed.blocked = {}
    if (!parsed.feed || typeof parsed.feed !== 'object') parsed.feed = {}
    if (!parsed.sourceStats || typeof parsed.sourceStats !== 'object') {
      parsed.sourceStats = { events: 0, malformedLines: 0, outOfOrderEvents: 0 }
    }
    report.value = parsed
    saveBacktest(parsed)
  } catch (err) {
    loadErr.value = `解析失败：${err instanceof Error ? err.message : String(err)}`
  } finally {
    inp.value = ''
  }
}

function onClear(): void {
  clearBacktest()
  report.value = null
}

onMounted(() => {
  const saved = loadBacktest()
  if (saved) report.value = saved
})

const windowSec = computed(() => Math.round((report.value?.virtualMs ?? 0) / 1000))
</script>

<template>
  <div v-if="report" class="rise-in">
    <!-- headline -->
    <Card>
      <div class="flex flex-wrap items-center gap-x-6 gap-y-4">
        <div class="min-w-0">
          <div class="flex items-center gap-2">
            <Badge variant="gold" dot>{{ backtestLabel }}</Badge>
            <Badge :variant="report.forcedDry ? 'info' : 'up'">
              {{ report.forcedDry ? '强制 dry' : 'dry' }}
            </Badge>
          </div>
          <p class="mt-2 truncate text-[13px] font-semibold">{{ report.source }}</p>
          <p class="mt-1 text-[11.5px] text-faint-fg num">
            {{ dateTime(report.startAtMs) }} → {{ dateTime(report.endAtMs) }}
            · 窗口 {{ duration(windowSec) }} · tick {{ report.tickMs }}ms
            <template v-if="report.tailMs"> · tail {{ report.tailMs }}ms</template>
          </p>
          <p class="mt-1 text-[11px] text-faint-fg num">
            事件 {{ num(report.sourceStats?.events) }}
            · 坏行 <span :class="report.sourceStats?.malformedLines ? 'text-down' : ''">{{ num(report.sourceStats?.malformedLines) }}</span>
            · 乱序 {{ num(report.sourceStats?.outOfOrderEvents) }}
            · 匹配盘口 {{ num(report.feed?.books) }} / 轮次 {{ num(report.feed?.rounds) }}
          </p>
        </div>

        <div class="text-center">
          <div class="stat-num text-[34px] leading-none" :class="positive ? 'text-up' : 'text-down'">
            {{ signedMoney(netPnl) }}
          </div>
          <div class="label-micro mt-1.5">回放净 PnL（扣费）</div>
        </div>

        <div class="ml-auto flex items-center gap-2">
          <Button variant="default" @click="pickFile"><Upload class="size-3.5" />替换报告</Button>
          <Button variant="ghost" @click="onClear"><Trash2 class="size-3.5" />清除</Button>
        </div>
      </div>
    </Card>

    <!-- KPIs -->
    <div class="mt-3.5 grid gap-3.5 sm:grid-cols-2 xl:grid-cols-4">
      <StatTile
        label="胜 / 负 / 平"
        :value="String(report.trades.wins)"
        tone="up"
      >
        <template #sub>
          <span class="text-down font-semibold">{{ report.trades.losses }}</span> 亏 ·
          {{ report.trades.flat }} 平 · 共 {{ report.trades.closed }} 笔平仓
        </template>
      </StatTile>
      <StatTile
        label="胜率"
        :value="pct(report.trades.winRatePct)"
        :tone="report.trades.winRatePct >= 50 ? 'up' : 'down'"
      >
        <template #sub>
          盈亏比 {{ report.trades.profitFactor != null ? num(report.trades.profitFactor) : '—' }} ·
          均笔 {{ signedMoney(report.trades.avgPnlUsd) }}
        </template>
      </StatTile>
      <StatTile label="手续费" :value="money(report.trades.feesUsd)">
        <template #sub>
          毛利 +{{ money(report.trades.grossProfitUsd) }} ·
          毛亏 {{ money(report.trades.grossLossUsd) }}
        </template>
      </StatTile>
      <StatTile
        label="最大回撤"
        :value="money(report.trades.maxDrawdownUsd)"
        :tone="report.trades.maxDrawdownUsd ? 'down' : 'default'"
      >
        <template #sub>
          <Tooltip content="回撤金额 ÷ 回撤前的权益峰值。净值基数很小时百分比会被放大，属正常现象。">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              占权益峰 {{ pct(report.trades.maxDrawdownPct) }}
            </span>
          </Tooltip>
        </template>
      </StatTile>
    </div>

    <div class="mt-3.5 grid gap-3.5 sm:grid-cols-2 xl:grid-cols-4">
      <StatTile label="订单流水" :value="num(report.orders.orders)">
        <template #sub>
          成交 {{ report.fills }} · 撤 {{ report.orders.cancelled }} ·
          拒 {{ report.orders.rejected }} · 失败 {{ report.orders.failed }}
        </template>
      </StatTile>
      <StatTile
        label="收盘未完成单"
        :value="num(report.orders.liveAtEnd)"
        :tone="report.orders.liveAtEnd ? 'gold' : 'default'"
      >
        <template #sub>回放结束时仍未终态的订单（挂单/部分成交）</template>
      </StatTile>
      <StatTile
        label="未平仓"
        :value="num(report.openPositions)"
        :tone="report.openPositions ? 'gold' : 'default'"
      >
        <template #sub>占用名义 {{ money(report.openNotionalUsd) }}</template>
      </StatTile>
      <StatTile
        label="门禁拦截"
        :value="num(blockedTotal)"
        :tone="blockedTotal ? 'gold' : 'default'"
      >
        <template #sub>
          动量 {{ num(report.blocked?.momentum) }} · 时点 {{ num(report.blocked?.timing) }}
        </template>
      </StatTile>
    </div>

    <!-- equity -->
    <Card class="mt-3.5">
      <CardHeader label="权益曲线">
        <template #action>
          <Tooltip v-if="truncated" :content="`报告只保留前 ${report.tradeLines.length} 笔明细，另有 ${truncated} 笔未列出；曲线按已列出的明细绘制。`">
            <Badge variant="gold">{{ report.tradeLines.length }} / {{ report.tradeLines.length + truncated }} 笔</Badge>
          </Tooltip>
          <Badge v-else variant="default">{{ report.tradeLines.length }} 笔</Badge>
        </template>
      </CardHeader>
      <div ref="eqEl" class="h-[240px] w-full" />
    </Card>

    <div class="mt-3.5 grid gap-3.5 xl:grid-cols-2">
      <Card>
        <CardHeader label="单笔盈亏分布" />
        <div ref="distEl" class="h-[220px] w-full" />
      </Card>
      <Card>
        <CardHeader label="分策略归因" />
        <div v-if="strategyAgg.length" ref="stratEl" class="h-[220px] w-full" />
        <EmptyState v-else text="报告未携带分策略数据（--engine 未挂载策略）" compact />
      </Card>
    </div>

    <!-- strategy table -->
    <Card v-if="strategyTable.length" class="mt-3.5" dense>
      <CardHeader label="策略明细" />
      <div class="-mx-2 overflow-x-auto">
        <table class="w-full min-w-[680px] text-[13px]">
          <thead>
            <tr class="text-left">
              <th class="label-micro px-2 pb-2.5">策略</th>
              <th class="label-micro px-2 pb-2.5 text-right">平仓</th>
              <th class="label-micro px-2 pb-2.5 text-right">胜 / 负</th>
              <th class="label-micro px-2 pb-2.5 text-right">胜率</th>
              <th class="label-micro px-2 pb-2.5 text-right">手续费</th>
              <th class="label-micro px-2 pb-2.5 text-right">净 PnL</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="s in strategyTable" :key="s.name" class="border-t border-line">
              <td class="px-2 py-2.5 font-semibold">{{ s.name }}</td>
              <td class="px-2 py-2.5 text-right num">{{ num(s.closed) }}</td>
              <td class="px-2 py-2.5 text-right num">
                <span class="text-up">{{ num(s.wins) }}</span>
                <span class="text-faint-fg"> / </span>
                <span class="text-down">{{ num(s.losses) }}</span>
              </td>
              <td class="px-2 py-2.5 text-right num text-muted-fg">
                {{ s.wins + s.losses ? pct((s.wins / (s.wins + s.losses)) * 100) : '—' }}
              </td>
              <td class="px-2 py-2.5 text-right num text-muted-fg">{{ money(s.fees) }}</td>
              <td class="px-2 py-2.5 text-right num font-semibold" :class="s.net >= 0 ? 'text-up' : 'text-down'">
                {{ signedMoney(s.net) }}
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </Card>

    <!-- gate rejections (blocked.byStrategy) -->
    <Card v-if="blockedRows.length || exemptions.length" class="mt-3.5" dense>
      <CardHeader label="门禁拦截归因">
        <template #action>
          <Badge :variant="blockedTotal ? 'gold' : 'default'">{{ blockedTotal }} 次</Badge>
        </template>
      </CardHeader>
      <div v-if="blockedRows.length" class="-mx-2 overflow-x-auto">
        <table class="w-full min-w-[480px] text-[13px]">
          <thead>
            <tr class="text-left">
              <th class="label-micro px-2 pb-2.5">策略</th>
              <th class="label-micro px-2 pb-2.5 text-right">动量门禁</th>
              <th class="label-micro px-2 pb-2.5 text-right">时点门禁</th>
              <th class="label-micro px-2 pb-2.5 text-right">合计</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="b in blockedRows" :key="b.name" class="border-t border-line">
              <td class="px-2 py-2.5 font-semibold">{{ b.name }}</td>
              <td class="px-2 py-2.5 text-right num" :class="b.momentum ? 'text-primary' : 'text-faint-fg'">{{ num(b.momentum) }}</td>
              <td class="px-2 py-2.5 text-right num" :class="b.timing ? 'text-primary' : 'text-faint-fg'">{{ num(b.timing) }}</td>
              <td class="px-2 py-2.5 text-right num font-semibold">{{ num(b.momentum + b.timing) }}</td>
            </tr>
          </tbody>
        </table>
      </div>
      <div v-if="exemptions.length" class="mt-3 flex flex-wrap gap-2">
        <Tooltip v-for="e in exemptions" :key="e.strategy" :content="`${e.strategy} 声明豁免：${(e.gates ?? []).join(' / ')}`">
          <span class="inline-flex items-center gap-1.5 rounded-full border border-line bg-panel-2 px-2.5 py-1 text-[11.5px] text-info">
            <ShieldOff class="size-3" />{{ e.strategy }}
          </span>
        </Tooltip>
      </div>
    </Card>

    <!-- extremes + risk -->
    <div class="mt-3.5 grid gap-3.5 xl:grid-cols-2">
      <Card>
        <CardHeader label="最佳 / 最差单笔" />
        <div v-if="bestTrade && worstTrade" class="space-y-2.5">
          <div class="flex items-center justify-between rounded-md border border-up/25 bg-up/8 px-3 py-2">
            <span class="inline-flex items-center gap-2 text-[12.5px] text-up">
              <TrendingUp class="size-3.5" />{{ bestTrade.asset }} {{ bestTrade.direction.toUpperCase() }}
            </span>
            <span class="stat-num text-[15px] text-up">{{ signedMoney(bestTrade.netPnlUsd) }}</span>
          </div>
          <div class="flex items-center justify-between rounded-md border border-down/25 bg-down/8 px-3 py-2">
            <span class="inline-flex items-center gap-2 text-[12.5px] text-down">
              <TrendingDown class="size-3.5" />{{ worstTrade.asset }} {{ worstTrade.direction.toUpperCase() }}
            </span>
            <span class="stat-num text-[15px] text-down">{{ signedMoney(worstTrade.netPnlUsd) }}</span>
          </div>
        </div>
        <EmptyState v-else text="本次回放没有平仓交易" compact />
      </Card>

      <Card>
        <CardHeader label="风控告警 / 错误">
          <template #action>
            <Badge :variant="report.riskAlerts.length ? 'gold' : 'default'">{{ report.riskAlerts.length }}</Badge>
            <Badge :variant="report.errors.length ? 'down' : 'default'">{{ report.errors.length }}</Badge>
          </template>
        </CardHeader>
        <div v-if="report.riskAlerts.length || report.errors.length" class="space-y-2">
          <AlertBanner v-for="(a, i) in report.riskAlerts" :key="`ra-${i}`" tone="warn">
            <span class="inline-flex items-start gap-1.5"><ShieldAlert class="mt-px size-3.5" />{{ a }}</span>
          </AlertBanner>
          <AlertBanner v-for="(er, i) in report.errors" :key="`er-${i}`" tone="error">
            <span class="inline-flex items-start gap-1.5"><AlertTriangle class="mt-px size-3.5" />{{ er }}</span>
          </AlertBanner>
        </div>
        <EmptyState v-else text="回放全程未触发风控告警或错误" compact />
      </Card>
    </div>
  </div>

  <!-- empty / load prompt -->
  <Card v-else class="rise-in">
    <div class="flex flex-col items-center gap-3 py-10 text-center">
      <span class="grid size-12 place-items-center rounded-xl border border-line bg-panel-2 text-primary">
        <FileJson class="size-6" />
      </span>
      <div>
        <p class="text-[15px] font-semibold">还没有回放报告</p>
        <p class="mt-1 text-[12.5px] text-faint-fg">
          先用内核跑一次回放，再把生成的 JSON 载入这里。
        </p>
      </div>
      <code class="rounded-md border border-line bg-panel-2 px-3 py-1.5 text-[11.5px] text-muted-fg">
        blitzkrieg-core --backtest &lt;archive.jsonl&gt; --engine --backtest-report report.json
      </code>
      <Button variant="gold" class="mt-1" @click="pickFile">
        <Upload class="size-3.5" />加载回测报告 JSON
      </Button>
      <div v-if="loadErr" class="w-full max-w-[560px]">
        <AlertBanner tone="error" title="加载失败" dismissible @dismiss="loadErr = null">{{ loadErr }}</AlertBanner>
      </div>
    </div>
  </Card>

  <input ref="fileEl" type="file" accept="application/json,.json" class="hidden" @change="onFile">
</template>
