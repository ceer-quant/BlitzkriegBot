<script setup lang="ts">
import { computed } from 'vue'
import { usePanelStore } from '../stores/panel'
import RejectionChart from '../components/RejectionChart.vue'

const store = usePanelStore()

const stats = computed(() => store.snapshot?.stats ?? null)

const cards = computed(() => {
  const s = stats.value
  return [
    { name: '数据 ticks', value: fmt(s?.dataTicks) },
    { name: '引擎 ticks', value: fmt(s?.engineTicks) },
    { name: '信号', value: fmt(s?.signals) },
    { name: '下单', value: fmt(s?.ordersPlaced), cls: s?.ordersPlaced ? 'gold' : '' },
    { name: '拒单', value: fmt(s?.ordersRejected), cls: s?.ordersRejected ? 'down' : '' },
  ]
})

function fmt(v: unknown): string {
  if (v === undefined || v === null) return '—'
  return String(v)
}
</script>

<template>
  <template v-if="store.snapshot">
    <div class="grid grid-stats">
      <div v-for="c in cards" :key="c.name" class="glass card" style="margin-bottom: 0">
        <div class="stat-name">{{ c.name }}</div>
        <div class="stat-value" :class="c.cls">{{ c.value }}</div>
      </div>
    </div>

    <div class="glass card" style="margin-top: 14px">
      <h2 class="card-title">策略拒单原因分布</h2>
      <RejectionChart :rows="store.strategyRows" />
    </div>

    <div v-if="store.snapshot?.lastError" class="error-banner" style="margin-top: 14px">
      引擎最近错误：{{ store.snapshot.lastError }}
    </div>
  </template>
  <div v-else class="glass card empty">{{ store.loading ? '加载中…' : '暂无快照数据' }}</div>
</template>
