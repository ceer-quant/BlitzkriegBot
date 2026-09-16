<script setup lang="ts">
/** Rejection-cause attribution — horizontal gold bars, one row per cause. */
import { computed, ref } from 'vue'
import type { EChartsOption } from 'echarts'
import { useChart, palette, tooltipStyle } from '@/lib/chart'
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
    <div v-if="!causes.length" class="absolute inset-0 grid place-items-center text-[11.5px] text-faint-fg">
      暂无拒单记录
    </div>
  </div>
</template>
