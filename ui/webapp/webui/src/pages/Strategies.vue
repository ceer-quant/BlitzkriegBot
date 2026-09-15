<script setup lang="ts">
import { computed } from 'vue'
import { usePanelStore } from '../stores/panel'

const store = usePanelStore()

const rows = computed(() => store.strategyRows)

function pnlCls(v: string | number): string {
  const n = typeof v === 'string' ? Number(v) : v
  if (!Number.isFinite(n) || n === 0) return ''
  return n > 0 ? 'up' : 'down'
}

function money(v: string | number): string {
  const n = typeof v === 'string' ? Number(v) : v
  if (!Number.isFinite(n)) return '—'
  return n.toFixed(2)
}

function causePills(row: { rejectionCauses: Record<string, number> | null }): { name: string; n: number }[] {
  return Object.entries(row.rejectionCauses ?? {})
    .sort((a, b) => b[1] - a[1])
    .map(([name, n]) => ({ name, n }))
}
</script>

<template>
  <div v-if="rows.length" class="glass card">
    <h2 class="card-title">策略表现（{{ rows.length }}）</h2>
    <div style="overflow-x: auto">
      <table>
        <thead>
          <tr>
            <th>策略</th><th>状态</th><th>来源</th>
            <th>下单</th><th>拒单</th><th>限额拒</th><th>时机挡</th><th>动量挡</th>
            <th>平仓</th><th>胜</th><th>负</th><th>净PnL ($)</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="r in rows" :key="r.name">
            <td style="font-weight: 600">{{ r.name }}</td>
            <td><span class="badge" :class="r.enabled ? 'on' : 'off'">{{ r.enabled ? '启用' : '停用' }}</span></td>
            <td class="sub">{{ r.source }}</td>
            <td>{{ r.ordersPlaced }}</td>
            <td>
              <span v-if="r.ordersRejected" class="pillnum err">{{ r.ordersRejected }}</span>
              <span v-else>0</span>
            </td>
            <td>{{ r.limitRejected }}</td>
            <td>{{ r.blockedTiming ? ` ⚑${r.blockedTiming}` : '0' }}</td>
            <td>{{ r.blockedMomentum ? ` ⚑${r.blockedMomentum}` : '0' }}</td>
            <td>{{ r.closedTrades }}</td>
            <td style="color: var(--bk-green)">{{ r.wins }}</td>
            <td style="color: var(--bk-red)">{{ r.losses }}</td>
            <td :class="`num-mono ${pnlCls(r.netPnlUsd)}`" :style="pnlCls(r.netPnlUsd) ? `color:${pnlCls(r.netPnlUsd) === 'up' ? 'var(--bk-green)' : 'var(--bk-red)'}` : ''">
              {{ money(r.netPnlUsd) }}
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <!-- rejection cause pills per strategy -->
    <div v-for="r in rows.filter((x) => x.rejectionCauses)" :key="`c-${r.name}`" style="margin-top: 8px">
      <span class="sub" style="font-weight: 600">{{ r.name }}：</span>
      <span
        v-for="p in causePills(r)"
        :key="p.name"
        class="pillnum warn"
        style="margin: 2px 4px"
      >{{ p.name }} × {{ p.n }}</span>
    </div>
  </div>
  <div v-else class="glass card empty">{{ store.loading ? '加载中…' : '暂无策略数据' }}</div>
</template>
