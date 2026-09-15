<script setup lang="ts">
import { onMounted, onUnmounted, ref, watch } from 'vue'
import * as echarts from 'echarts'
import type { StrategyStatsRow } from '../api/client'

const props = defineProps<{ rows: StrategyStatsRow[] }>()

const el = ref<HTMLDivElement | null>(null)
let chart: echarts.ECharts | null = null

// Aggregate rejection causes across all strategies into one bar chart.
function option(): echarts.EChartsOption {
  const totals = new Map<string, number>()
  for (const r of props.rows) {
    for (const [bucket, n] of Object.entries(r.rejectionCauses ?? {})) {
      totals.set(bucket, (totals.get(bucket) ?? 0) + n)
    }
  }
  const entries = [...totals.entries()].sort((a, b) => b[1] - a[1])
  return {
    grid: { left: 140, right: 24, top: 8, bottom: 24, containLabel: false },
    tooltip: { trigger: 'axis', axisPointer: { type: 'shadow' } },
    xAxis: { type: 'value', minInterval: 1 },
    yAxis: {
      type: 'category',
      data: entries.map(([k]) => k),
      axisLabel: { fontSize: 11 },
    },
    series: [
      {
        type: 'bar',
        data: entries.map(([, v]) => v),
        itemStyle: { color: '#f08c00', borderRadius: [0, 6, 6, 0] },
        barMaxWidth: 18,
      },
    ],
  }
}

function render(): void {
  if (!el.value) return
  if (!chart) chart = echarts.init(el.value)
  chart.setOption(option(), true)
  window.addEventListener('resize', resize)
}

function resize(): void {
  chart?.resize()
}

onMounted(render)
watch(() => props.rows, render, { deep: true })
onUnmounted(() => {
  window.removeEventListener('resize', resize)
  chart?.dispose()
  chart = null
})
</script>

<template>
  <template v-if="rows.some((r) => r.rejectionCauses && Object.keys(r.rejectionCauses).length)">
    <div ref="el" class="chart"></div>
  </template>
  <div v-else class="empty">暂无拒单记录</div>
</template>
