<script setup lang="ts">
/**
 * 盘口深度 — one token's live L2 ladder (E8-c, `engine.books`).
 *
 * Classic depth shape: X = price (the market's 0..1 band), Y = cumulative
 * shares accumulated from the touch inward. Bids step down-left of the mid in
 * the up tone, asks step up-right in the down tone; a dashed hairline marks
 * the mid. Where the ladder ends is where the market stopped quoting — for a
 * prediction token the empty half-space carries real information, so the x
 * axis stays pinned to 0..1 instead of zooming to the quote.
 */
import { computed, ref } from 'vue'
import type { EChartsOption } from 'echarts'
import { useChart, areaFade, palette, tooltipStyle } from '@/lib/chart'
import { cents } from '@/lib/format'
import type { BookLevel, BookSide } from '@/api/client'

const props = withDefaults(defineProps<{ side: BookSide | null; height?: number }>(), {
  height: 150,
})

const el = ref<HTMLDivElement | null>(null)

/**
 * Cumulative-depth points, X ascending. `desc=true` walks a best-first
 * descending ladder (bids): levels are re-sorted ascending for plotting while
 * keeping each point's cumulative depth measured from the touch.
 */
function depthPoints(levels: BookLevel[], desc: boolean): [number, number][] {
  const sorted = [...levels].sort((a, b) => Number(a.price) - Number(b.price))
  let cum = 0
  const pts = (desc ? [...sorted].reverse() : sorted).map((l) => {
    cum += Number(l.size) || 0
    return [Number(l.price), cum] as [number, number]
  })
  return desc ? pts.reverse() : pts
}

const hasBook = computed(
  () => !!props.side && (props.side.bids.length > 0 || props.side.asks.length > 0),
)

const option = computed<EChartsOption>(() => {
  const p = palette()
  const side = props.side
  const bids = hasBook.value ? depthPoints(side!.bids, true) : []
  const asks = hasBook.value ? depthPoints(side!.asks, false) : []
  const mid = side?.midPrice ?? null

  const midMark = mid && mid > 0 && mid < 1
    ? {
        silent: true,
        symbol: 'none',
        label: { show: false },
        lineStyle: { color: p.axis, type: 'dashed' as const, width: 1 },
        data: [{ xAxis: mid }],
      }
    : undefined

  return {
    animationDuration: 320,
    animationEasing: 'cubicOut',
    grid: { left: 46, right: 14, top: 14, bottom: 26 },
    tooltip: {
      trigger: 'axis',
      axisPointer: { type: 'line' },
      ...tooltipStyle(),
      formatter: (params: unknown) => {
        const arr = params as { seriesName: string; value: [number, number] }[]
        const pt = arr?.[0]?.value
        if (!pt) return ''
        return `${cents(pt[0])}<br/><b>${pt[1].toFixed(1)}</b> 份`
      },
    },
    xAxis: {
      type: 'value',
      min: 0,
      max: 1,
      axisLine: { lineStyle: { color: p.axis } },
      axisTick: { show: false },
      splitLine: { show: false },
      axisLabel: { color: p.textDim, fontSize: 9.5, formatter: (v: number) => cents(v) },
    },
    yAxis: {
      type: 'value',
      axisLine: { show: false },
      axisTick: { show: false },
      splitLine: { lineStyle: { color: p.split, type: 'dashed' as const } },
      axisLabel: { color: p.textDim, fontSize: 9.5 },
    },
    series: [
      {
        name: '买盘 bids',
        type: 'line',
        step: 'end',
        data: bids,
        showSymbol: false,
        lineStyle: { width: 1.6, color: p.up },
        areaStyle: areaFade(p.up, '38'),
      },
      {
        name: '卖盘 asks',
        type: 'line',
        step: 'end',
        data: asks,
        showSymbol: false,
        lineStyle: { width: 1.6, color: p.down },
        areaStyle: areaFade(p.down, '38'),
        markLine: midMark,
      },
    ],
  }
})

useChart(el, () => option.value)
</script>

<template>
  <div class="relative w-full" :style="{ height: `${props.height}px` }">
    <div ref="el" class="absolute inset-0" />
    <div
      v-if="!hasBook"
      class="absolute inset-0 grid place-items-center text-[11.5px] text-faint-fg"
    >
      等待盘口数据
    </div>
  </div>
</template>
