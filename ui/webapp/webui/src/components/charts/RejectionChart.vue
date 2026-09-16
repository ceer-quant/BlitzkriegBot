<script setup lang="ts">
/**
 * Rejection-cause attribution — horizontal gold bars, one row per cause.
 *
 * Refusals without a cause breakdown are a distinct state from "no refusals", and
 * this chart must not merge them. A core older than `rejectionCauses` (#65) refuses
 * orders without reporting buckets, so an empty chart here means "the reasons were
 * not reported", not "nothing was refused" — see `lib/rejections.ts`. The empty
 * state says which of the two applies, and how many refusals are unattributed.
 */
import { computed, ref } from 'vue'
import type { EChartsOption } from 'echarts'
import { useChart, palette, tooltipStyle } from '@/lib/chart'
import { num } from '@/lib/format'
import type { StrategyStatsRow } from '@/api/client'

const props = withDefaults(
  defineProps<{ rows: StrategyStatsRow[]; height?: number; topN?: number }>(),
  { height: 220, topN: 10 },
)

const el = ref<HTMLDivElement | null>(null)

const causes = computed(() => {
  const totals = new Map<string, number>()
  for (const r of props.rows) {
    for (const [bucket, n] of Object.entries(r.rejectionCauses ?? {})) {
      totals.set(bucket, (totals.get(bucket) ?? 0) + Number(n || 0))
    }
  }
  return [...totals.entries()]
    .filter(([, n]) => n > 0)
    .sort((a, b) => b[1] - a[1])
    .slice(0, props.topN)
})

/** Refusals the core counted but did not attribute to a cause. */
const unattributed = computed(() =>
  props.rows.reduce((a, r) => a + (Number(r.ordersRejected) || 0), 0),
)

const option = computed<EChartsOption>(() => {
  const p = palette()
  const rows = causes.value
  const max = Math.max(...rows.map((r) => r[1]), 1)
  return {
    animationDuration: 520,
    animationEasing: 'cubicOut',
    grid: { left: 130, right: 46, top: 6, bottom: 6, containLabel: false },
    tooltip: { trigger: 'axis', axisPointer: { type: 'shadow' }, ...tooltipStyle() },
    xAxis: { type: 'value', show: false, max: max * 1.08 },
    yAxis: {
      type: 'category',
      inverse: true,
      data: rows.map(([k]) => k),
      axisLine: { show: false },
      axisTick: { show: false },
      axisLabel: { color: p.textDim, fontSize: 11 },
    },
    series: [
      {
        type: 'bar',
        // each bar is a gradient of the brand gold, dimmed by rank
        data: rows.map(([, v], i) => {
          const ratio = 1 - (i / Math.max(rows.length, 1)) * 0.55
          return {
            value: v,
            itemStyle: {
              borderRadius: [0, 5, 5, 0],
              color: {
                type: 'linear' as const,
                x: 0, y: 0, x2: 1, y2: 0,
                colorStops: [
                  { offset: 0, color: `rgba(240,160,42,${(0.55 * ratio).toFixed(3)})` },
                  { offset: 1, color: `rgba(240,160,42,${(0.95 * ratio).toFixed(3)})` },
                ],
              },
            },
          }
        }),
        barMaxWidth: 15,
        label: {
          show: true,
          position: 'right',
          color: p.textDim,
          fontSize: 10.5,
          formatter: '{c}',
        },
      },
    ],
  }
})

useChart(el, () => option.value)
</script>

<template>
  <div class="relative w-full" :style="{ height: `${props.height}px` }">
    <div ref="el" class="absolute inset-0" />
    <div v-if="!causes.length" class="absolute inset-0 grid place-items-center px-4 text-center">
      <!--
        Two very different situations, kept apart: nothing was refused, versus
        refusals happened and the core did not say why. Saying "暂无拒单记录" for
        both would read as an all-clear on a core that has refused tens of
        thousands of orders.
      -->
      <p v-if="unattributed > 0" class="text-[11.5px] leading-snug text-faint-fg">
        <span class="num font-semibold text-down">{{ num(unattributed) }}</span> 笔拒单未归因：
        内核未上报拒单原因（通常是内核版本早于拒单归因功能）。重启内核后即可按原因分类。
      </p>
      <p v-else class="text-[11.5px] text-faint-fg">暂无拒单记录</p>
    </div>
  </div>
</template>
