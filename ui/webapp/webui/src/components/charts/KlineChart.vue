<script setup lang="ts">
/**
 * K线图 — one symbol's OHLCV bars (E29 §12.2, `kline.history` via the
 * gateway's thin proxy `GET /api/kline-history`).
 *
 * The panel has no WebSocket: the whole data channel is poll-redraw-poll on
 * the same 2s tick the HFT tab uses for snapshots, honouring the same
 * persisted fastPoll preference (off = watch-only, chart freezes on its last
 * bars rather than lying about liveness). The LAST bar is the GROWING one
 * (`isClosed: false`) — it is drawn semi-transparent so "still forming" reads
 * at a glance; closed bars never change under it (§10.3: data-driven close,
 * one bar one event, never throttled).
 *
 * Bar colours follow the CN convention — 红涨绿跌 — the same convention the
 * panel's PnL columns already use. The palette's `up`/`down` are BUY/SELL
 * semantics, not rise/fall, so the mapping is deliberately crossed here.
 */
import { computed, onUnmounted, ref, watch } from 'vue'
import { useIntervalFn } from '@vueuse/core'
import type { EChartsOption } from 'echarts'
import { useChart, palette, tooltipStyle } from '@/lib/chart'
import { clockTime, dateTime, compact } from '@/lib/format'
import { api, KLINE_INTERVALS, type KlineBar, type KlineIntervalWire } from '@/api/client'
import { useSettingsStore } from '@/stores/settings'

const props = withDefaults(defineProps<{
  symbol: string
  height?: number
  defaultInterval?: KlineIntervalWire
}>(), { height: 260, defaultInterval: 'min1' })

const settings = useSettingsStore()
const el = ref<HTMLDivElement | null>(null)
const interval = ref<KlineIntervalWire>(props.defaultInterval)
const bars = ref<KlineBar[]>([])
const loaded = ref(false)
const failed = ref<string | null>(null)

async function refresh(): Promise<void> {
  if (!props.symbol) return
  try {
    const doc = await api.klineHistory({
      symbol: props.symbol,
      interval: interval.value,
      limit: 120,
    })
    if (doc.error) {
      failed.value = doc.error
      return
    }
    failed.value = null
    bars.value = doc.klines
    loaded.value = true
  } catch (e) {
    failed.value = e instanceof Error ? e.message : String(e)
  }
}

watch(
  () => [props.symbol, interval.value] as const,
  () => { void refresh() },
  { immediate: true },
)

// 2s poll, same pacing as the HFT tab's snapshot tick (see HftPage) — and the
// same persisted preference gates it: fastPoll off means the operator asked
// for watch-only, so the chart holds its last bars instead of polling.
const { pause: stopPoll } = useIntervalFn(() => {
  if (settings.fastPoll) void refresh()
}, 2_000)
onUnmounted(() => { stopPoll() })

/** Sub-minute and 1m bars read as clock time; wider spans need the date. */
function fmtBarTime(ms: number, iv: string): string {
  return ['sec1', 'sec5', 'sec15', 'min1'].includes(iv) ? clockTime(ms) : dateTime(ms)
}

const option = computed<EChartsOption>(() => {
  const p = palette()
  const rows = bars.value
  const iv = interval.value
  // CN convention: 红涨绿跌. The palette's `up` is the BUY tone (green) and
  // `down` the SELL tone (red), so the rise/fall mapping crosses the names.
  const RISE = p.down
  const FALL = p.up
  const n = rows.length

  // ECharts candlestick item order: [open, close, low, high].
  const ohlc = rows.map((b) => [
    Number(b.open), Number(b.close), Number(b.low), Number(b.high),
  ])
  const volData = rows.map((b, i) => ({
    value: Number(b.volume) || 0,
    itemStyle: {
      color: Number(b.close) >= Number(b.open) ? RISE : FALL,
      opacity: i === n - 1 && !b.isClosed ? 0.4 : 0.85,
    },
  }))
  const labels = rows.map((b) => fmtBarTime(b.openTimeMs, iv))
  const last = n > 0 ? rows[n - 1] : null
  const lastPx = last ? Number(last.close) : null
  const lastMark = lastPx && lastPx > 0
    ? {
        silent: true,
        symbol: 'none',
        label: { show: false },
        lineStyle: { color: p.textDim, type: 'dashed' as const, width: 1 },
        data: [{ yAxis: lastPx }],
      }
    : undefined

  return {
    animationDuration: 240,
    grid: [
      { left: 50, right: 14, top: 12, height: '60%' },
      { left: 50, right: 14, top: '78%', height: '15%' },
    ],
    tooltip: {
      trigger: 'axis',
      axisPointer: { type: 'cross', label: { backgroundColor: p.surface, color: p.text } },
      ...tooltipStyle(),
      formatter: (ps: unknown) => {
        const arr = ps as { dataIndex: number }[]
        const idx = arr?.[0]?.dataIndex
        if (idx == null || !rows[idx]) return ''
        const b = rows[idx]
        const chg = Number(b.open) > 0
          ? ((Number(b.close) - Number(b.open)) / Number(b.open)) * 100
          : 0
        return [
          `<b>${fmtBarTime(b.openTimeMs, iv)}</b>${b.isClosed ? '' : ' · 形成中'}`,
          `开 ${Number(b.open).toFixed(3)}  收 <b>${Number(b.close).toFixed(3)}</b> <span style="color:${chg >= 0 ? RISE : FALL}">${chg >= 0 ? '+' : ''}${chg.toFixed(2)}%</span>`,
          `高 ${Number(b.high).toFixed(3)}  低 ${Number(b.low).toFixed(3)}`,
          `量 ${compact(b.volume)} · ${b.tradeCount} 笔`,
        ].join('<br/>')
      },
    },
    axisPointer: { link: [{ xAxisIndex: 'all' }] },
    xAxis: [
      {
        type: 'category',
        gridIndex: 0,
        data: labels,
        boundaryGap: true,
        axisLine: { lineStyle: { color: p.axis } },
        axisTick: { show: false },
        axisLabel: { show: false },
      },
      {
        type: 'category',
        gridIndex: 1,
        data: labels,
        boundaryGap: true,
        axisLine: { lineStyle: { color: p.axis } },
        axisTick: { show: false },
        axisLabel: { color: p.textDim, fontSize: 9.5 },
      },
    ],
    yAxis: [
      {
        type: 'value',
        gridIndex: 0,
        scale: true,
        axisLine: { show: false },
        axisTick: { show: false },
        splitLine: { lineStyle: { color: p.split, type: 'dashed' as const } },
        axisLabel: { color: p.textDim, fontSize: 9.5, formatter: (v: number) => v.toFixed(3) },
      },
      {
        type: 'value',
        gridIndex: 1,
        axisLine: { show: false },
        axisTick: { show: false },
        splitLine: { show: false },
        axisLabel: { color: p.textDim, fontSize: 9.5, formatter: (v: number) => compact(v) },
      },
    ],
    series: [
      {
        name: 'K线',
        type: 'candlestick',
        xAxisIndex: 0,
        yAxisIndex: 0,
        data: ohlc,
        itemStyle: {
          color: RISE,
          color0: FALL,
          borderColor: RISE,
          borderColor0: FALL,
        },
        markLine: lastMark,
      },
      {
        name: '成交量',
        type: 'bar',
        xAxisIndex: 1,
        yAxisIndex: 1,
        data: volData,
        barMaxWidth: 6,
      },
    ],
  }
})

useChart(el, () => option.value)
</script>

<template>
  <div class="relative w-full" :style="{ height: `${props.height}px` }">
    <div ref="el" class="absolute inset-0" />
    <!-- interval picker: the nine §10.2 spans, ascending -->
    <div class="absolute top-1 right-2 z-10 flex gap-0.5">
      <button
        v-for="iv in KLINE_INTERVALS"
        :key="iv.value"
        class="px-1.5 py-0.5 rounded text-[10px] transition-colors"
        :class="interval === iv.value
          ? 'bg-primary/15 text-primary font-semibold'
          : 'text-faint-fg hover:text-fg'"
        @click="interval = iv.value"
      >
        {{ iv.label }}
      </button>
    </div>
    <div
      v-if="!loaded || bars.length === 0"
      class="absolute inset-0 grid place-items-center text-[11.5px] text-faint-fg pointer-events-none"
    >
      <template v-if="failed">K线读取失败：{{ failed }}</template>
      <template v-else-if="loaded">暂无K线数据（等待行情喂入）</template>
      <template v-else>等待K线数据</template>
    </div>
  </div>
</template>
