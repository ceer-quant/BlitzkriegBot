<script setup lang="ts">
import { computed } from 'vue'
import { usePanelStore } from '../stores/panel'
import RejectionChart from '../components/RejectionChart.vue'

const store = usePanelStore()

const snap = computed(() => store.snapshot)

const engineCards = computed(() => {
  const st = snap.value?.stats
  return [
    { name: '订单簿', value: n(st?.books) },
    { name: 'Top 快照', value: n(st?.tops) },
    { name: '轮次', value: n(st?.rounds) },
    { name: '评估', value: n(st?.evaluations) },
    { name: '信号', value: n(st?.signals) },
    { name: '拒单', value: n(st?.placeRejected), cls: st?.placeRejected ? 'down' : '' },
  ]
})

function n(v: unknown): string {
  if (v === undefined || v === null) return '—'
  const num = Number(v)
  return Number.isFinite(num) ? num.toLocaleString() : String(v)
}

function fmtWinRate(v: number): string {
  // Rust core returns a 0..1 fraction (occasionally >1 historically with
  // partial records); normalize to a percent string without double-scaling.
  const frac = v > 1.5 ? v / 100 : v
  return `${(frac * 100).toFixed(1)}%`
}

function money(v?: number | null): string {
  if (v === undefined || v === null) return '—'
  return `$${v.toFixed(2)}`
}

const round = computed(() => snap.value?.round ?? null)
const balance = computed(() => snap.value?.balance ?? null)

// Balance semantics follow the run mode: dry cash is the local simulation
// seed (not real money); live cash is the venue-reported balance. The card
// must never present one as the other (mirrors ui/hft.html's wallet panel).
const isDry = computed(() => (snap.value?.mode ?? 'dry') === 'dry')
const balanceTitle = computed(() => (isDry.value ? '模拟余额' : '交易所余额'))
const walletAddr = computed(() => snap.value?.wallet?.funder ?? snap.value?.wallet?.signer ?? null)
const positions = computed(() => snap.value?.positions ?? [])
</script>

<template>
  <template v-if="snap">
    <div class="grid grid-cols">
      <div class="glass card">
        <h2 class="card-title">{{ balanceTitle }}</h2>
        <div class="stat-value" :class="isDry ? 'gold' : ''">{{ money(balance?.balance) }}</div>
        <div class="sub">
          可用 {{ money(balance?.available) }} · 预留 {{ money(balance?.reserved) }}
          <template v-if="isDry"> · 模拟资金，非真实资产</template>
        </div>
        <div v-if="!isDry && walletAddr" class="sub num-mono" style="font-size: 11px">
          钱包 {{ walletAddr.slice(0, 6) }}…{{ walletAddr.slice(-4) }}
        </div>
      </div>
      <div class="glass card">
        <h2 class="card-title">当前轮次</h2>
        <div v-if="round" class="stat-value">#{{ round.slot }}</div>
        <div v-else class="stat-value dim">—</div>
        <div class="sub" v-if="round">
          {{ round.ageSec }}s 已过 · {{ round.timeLeftSec }}s 剩余 ·
          <span :style="{ color: round.canTrade ? 'var(--bk-green)' : 'var(--bk-gold)' }">
            {{ round.canTrade ? '可交易' : '等待' }}
          </span>
        </div>
      </div>
      <div class="glass card">
        <h2 class="card-title">已平仓交易</h2>
        <template v-if="snap.trades">
          <div class="stat-value">{{ snap.trades.count }}</div>
          <div class="sub">
            扣费净利 {{ money(snap.trades.net) }} · 胜率 {{ fmtWinRate(snap.trades.winRate) }}
          </div>
        </template>
        <template v-else>
          <div class="stat-value dim">—</div>
          <div class="sub">暂无交易数据</div>
        </template>
      </div>
      <div class="glass card">
        <h2 class="card-title">运行模式</h2>
        <div class="stat-value" :class="snap.mode === 'dry' ? 'gold' : ''">{{ snap.mode ?? '—' }}</div>
        <div class="sub">{{ snap.connected ? '引擎已连接' : '引擎未连接' }}</div>
      </div>
    </div>

    <div class="glass card" style="margin-top: 14px">
      <h2 class="card-title">引擎计数</h2>
      <div class="grid grid-stats">
        <div v-for="c in engineCards" :key="c.name">
          <div class="stat-name">{{ c.name }}</div>
          <div class="stat-value sm" :class="c.cls">{{ c.value }}</div>
        </div>
      </div>
    </div>

    <div class="glass card" style="margin-top: 14px">
      <h2 class="card-title">策略拒单原因分布</h2>
      <RejectionChart :rows="store.strategyRows" />
    </div>

    <div class="glass card" style="margin-top: 14px">
      <h2 class="card-title">当前持仓（{{ positions.length }}）</h2>
      <table v-if="positions.length">
        <thead>
          <tr><th>资产</th><th>方向</th><th>入场</th><th>现价</th><th>浮动</th></tr>
        </thead>
        <tbody>
          <tr v-for="(p, i) in positions" :key="`${p.asset}-${i}`">
            <td style="font-weight: 600">{{ p.asset }}</td>
            <td><span class="badge" :class="p.direction === 'up' ? 'on' : 'warn'">{{ p.direction.toUpperCase() }}</span></td>
            <td>{{ p.entryPrice }}</td>
            <td>{{ p.currentPrice }}</td>
            <td :style="{ color: p.unrealizedPct >= 0 ? 'var(--bk-green)' : 'var(--bk-red)' }">
              {{ p.unrealizedPct >= 0 ? '+' : '' }}{{ p.unrealizedPct.toFixed(1) }}%
            </td>
          </tr>
        </tbody>
      </table>
      <div v-else class="empty">无持仓</div>
    </div>

    <div v-if="snap.lastError" class="error-banner" style="margin-top: 14px">
      引擎最近错误：{{ snap.lastError }}
    </div>
  </template>
  <div v-else class="glass card empty">{{ store.loading ? '加载中…' : '暂无快照数据' }}</div>
</template>

<style scoped>
.grid-cols {
  grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
}
.stat-value.sm { font-size: 20px; }
.stat-value.dim { color: var(--bk-text-dim); }
</style>
