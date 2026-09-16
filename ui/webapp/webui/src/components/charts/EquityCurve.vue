<script setup lang="ts">
/**
 * Equity curve — cumulative net PnL across closed trades. Shared by the
 * overview and the HFT template page.
 */
import { computed, ref } from 'vue'
import type { EChartsOption } from 'echarts'
import { useChart, areaFade, palette, tooltipStyle, axisX, axisY } from '@/lib/chart'
import { signedMoney } from '@/lib/format'
import type { TradeRow } from '@/api/client'

const props = withDefaults(
  defineProps<{ rows: TradeRow[]; height?: number; showAxis?: boolean; live?: boolean }>(),
  { height: 132, showAxis: true, live: false },
)

const el = ref<HTMLDivElement | null>(null)

/** Cumulative series (oldest → newest) with a 0 origin point. */
const series = computed<number[]>(() => {
  const ordered = [...props.rows].reverse()
  let cum = 0
  const pts = ordered.map((t) => (cum += Number(t.netPnlUsd) || 0))
  return [0, ...pts]
})

const total = computed(() => series.value.at(-1) ?? 0)
const positive = computed(() => total.value >= 0)

const option = computed<EChartsOption>(() => {
  const p = palette()
  const color = positive.value ? p.up : p.down
  const pts = series.value
  return {
    animationDuration: 480,
    animationEasing: 'cubicOut',
    grid: {
      left: props.showAxis ? 46 : 2,
      right: 2,
      top: 8,
      bottom: props.showAxis ? 20 : 2,
      containLabel: false,
    },
    tooltip: {
      trigger: 'axis',
      ...tooltipStyle(),
      valueFormatter: (v) => signedMoney(Number(v)),
    },
    xAxis: props.showAxis
      ? { ...axisX(pts.map((_, i) => `#${i}`)), boundaryGap: false, axisLine: { show: false } }
      : { type: 'category', data: pts.map((_, i) => i), show: false, boundaryGap: false },
    yAxis: props.showAxis
      ? axisY()
      : { type: 'value', show: false, scale: true },
    series: [
      {
        type: 'line',
        data: pts,
        showSymbol: false,
        smooth: 0.35,
        lineStyle: { width: 2, color, shadowColor: color, shadowBlur: 12, shadowOffsetY: 3 },
        areaStyle: areaFade(color, positive.value ? '4d' : '40'),
        markLine: props.showAxis && pts.length > 1
          ? {
              silent: true,
              symbol: 'none',
              label: { show: false },
              lineStyle: { color: p.axis, type: 'dashed', width: 1 },
              data: [{ yAxis: 0 }],
            }
          : undefined,
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
      v-if="props.rows.length < 2"
      class="absolute inset-0 grid place-items-center text-[11.5px] text-faint-fg"
    >
      暂无平仓交易
    </div>
  </div>
</template>
