<script setup lang="ts">
/**
 * 二元预测市场通用组件（模板页）— 源自 ui/hft.html 的 HFT 面板，
 * 现在是任何二元预测市场策略共用的数据展示模板：倒计时、行情卡
 * (UP/DOWN 价差)、PnL 曲线、胜率分布、交易统计、当前持仓 +
 * 历史订单 tabs、启动/停止。页头标注当前行情插件的身份
 * （二元预测市场/现货市场/合约市场/期货实现）。数据全部来自
 * Rust 端 /api/snapshot — 不依赖 Node 网关。
 */
import { computed, onUnmounted, ref, watch } from 'vue'
import { useIntervalFn } from '@vueuse/core'
import {
  api, marketTypeLabel,
  type MarketPrice, type TradeRow,
} from '../api/client'
import { usePanelStore } from '../stores/panel'

const store = usePanelStore()

// Fast tick: 2s snapshot poll, faster than the global 15s, matching HFT pacing.
const { pause: stopFast } = useIntervalFn(() => { void store.refresh() }, 2_000)
onUnmounted(() => { stopFast() })

const snap = computed(() => store.snapshot)

// ── market identity (which venue plugin drives this session) ──────────────────
const marketName = computed(() => snap.value?.marketActiveName ?? null)
const marketTypeText = computed(() =>
  marketTypeLabel(snap.value?.marketActiveType ?? null),
)

// serverLeft/serverAt hold the last server answer; leftSec recomputes on each
// 500ms tick by locally interpolating the time the snapshot was received.
const serverLeft = ref(0)
const serverAt = ref(0)
watch(snap, (s) => {
  if (s?.round && s.round.timeLeftSec >= 0) {
    serverLeft.value = s.round.timeLeftSec
    serverAt.value = Date.now()
  }
}, { immediate: true })
const leftSec = ref(0)
const { pause: stopTick } = useIntervalFn(() => {
  const elapsed = (Date.now() - serverAt.value) / 1000
  leftSec.value = Math.max(0, Math.round(serverLeft.value - elapsed))
}, 500)
onUnmounted(() => { stopTick() })
const leftText = computed(() => {
  const s = leftSec.value
  return `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`
})

const round = computed(() => snap.value?.round ?? null)

// Balance semantics follow the mode (dry = local seed cash, live = venue
// funds) — same discipline as the Overview balance card.
const isDry = computed(() => (snap.value?.mode ?? 'dry') === 'dry')
const walletAddr = computed(
  () => snap.value?.wallet?.funder ?? snap.value?.wallet?.signer ?? null,
)

// ── market price cards (UP/DOWN + spread cents) ──────────────────────────────
const prices = computed<MarketPrice[]>(() => round.value?.prices ?? [])
function spreadCents(m: MarketPrice): string {
  return `价差 ${(Math.max(0, m.up + m.down - 1) * 100).toFixed(1)}¢`
}

// ── PnL history series (cumulative net PnL across closed trades) ─────────────
const pnlSeries = computed<number[]>(() => {
  const rows = snap.value?.tradeRows ?? []
  // oldest → newest for a left-to-right line
  const ordered = [...rows].reverse()
  let cum = 0
  const pts = ordered.map((t) => (cum += Number(t.netPnlUsd) || 0))
  pts.unshift(0)
  return pts
})
const pnlNet = computed(() => {
  const s = snap.value?.trades
  return s ? s.net : 0
})
const pnlNetClass = computed(() => (pnlNet.value >= 0 ? 'pos' : 'neg'))

const sparkline = computed<string>(() => {
  const pts = pnlSeries.value
  if (pts.length < 2) return ''
  const w = 300
  const h = 64
  const max = Math.max(...pts)
  const min = Math.min(...pts, 0)
  const range = max - min || 1
  const pad = 4
  const path = pts
    .map((v, i) => {
      const x = pad + (i / (pts.length - 1)) * (w - pad * 2)
      const y = pad + (1 - (v - min) / range) * (h - pad * 2)
      return `${i === 0 ? 'M' : 'L'}${x.toFixed(1)},${y.toFixed(1)}`
    })
    .join(' ')
  const zeroY = pad + (1 - (0 - min) / range) * (h - pad * 2)
  const zero = `<line x1="${pad}" y1="${zeroY.toFixed(1)}" x2="${w - pad}" y2="${zeroY.toFixed(1)}" stroke="rgba(255,255,255,0.15)" stroke-dasharray="4 4" stroke-width="1"/>`
  return `<svg viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">${zero}<path d="${path}" fill="none" stroke="${pnlNet.value >= 0 ? 'var(--bk-green)' : 'var(--bk-red)'}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/></svg>`
})

// ── win-rate card with W/L ranges ────────────────────────────────────────────
const winRows = computed(() => {
  const rows = snap.value?.tradeRows ?? []
  const bands = [
    { label: '≥ $1.00', test: (v: number) => v >= 1 },
    { label: '$0 – $1', test: (v: number) => v >= 0.01 && v < 1 },
    { label: '–$1 – $0', test: (v: number) => v <= -0.01 && v > -1 },
    { label: '≤ –$1.00', test: (v: number) => v <= -1 },
  ]
  const c = new Map<string, number>()
  for (const b of bands) c.set(b.label, rows.filter((t) => b.test(Number(t.netPnlUsd) || 0)).length)
  const st = snap.value?.trades
  const wr = st ? (st.winRate > 1.5 ? st.winRate / 100 : st.winRate) * 100 : 0
  const w = st ? rows.filter((t) => (Number(t.netPnlUsd) || 0) > 0).length : 0
  const l = rows.length - w
  return { wr: wr.toFixed(1), w, l, bands: bands.map((b) => ({ label: b.label, count: c.get(b.label) ?? 0 })) }
})

// ── trade volume / avg / today card ──────────────────────────────────────────
const tradeStats = computed(() => {
  const rows = snap.value?.tradeRows ?? []
  const today = new Date().toDateString()
  const todays = rows.filter((t) => today === '')
  const totalVol = rows.reduce((x, t) => x + (t.entryPrice * t.shares || 0), 0)
  const avg = rows.length ? totalVol / rows.length : 0
  // core sends netPnlPct already in percent (e.g. 9.89 = +9.89%)
  const pctVals = rows.map((t) => t.netPnlPct ?? 0)
  return {
    count: rows.length,
    volume: `$${totalVol.toLocaleString(undefined, { maximumFractionDigits: 2 })}`,
    avg: `$${avg.toLocaleString(undefined, { maximumFractionDigits: 2 })}`,
    best: pctVals.length ? `${Math.max(...pctVals).toFixed(2)}%` : '—',
    worst: pctVals.length ? `${Math.min(...pctVals).toFixed(2)}%` : '—',
    today: todays.length,
  }
})

// ── tabs: 当前持仓 / 历史订单 ────────────────────────────────────────────────
const tab = ref<'positions' | 'history'>('positions')
const positions = computed(() => snap.value?.positions ?? [])
const historyRows = computed<TradeRow[]>(() => snap.value?.tradeRows ?? [])
function money(v: number): string {
  return `${v >= 0 ? '+' : ''}$${Math.abs(v).toFixed(2)}`
}
function moneyCls(v: number): string {
  return v > 0 ? 'pos' : v < 0 ? 'neg' : ''
}

// ── start/stop (dispatcher lifecycle; requires --manage gateway) ─────────────
const lifecycle = computed(() => snap.value?.mode != null)
const busy = ref(false)
const cmdNote = ref<string | null>(null)
async function sendLifecycle(verb: 'start' | 'stop'): Promise<void> {
  busy.value = true
  cmdNote.value = null
  try {
    const doc = await api.command(`${verb}`)
    cmdNote.value = doc.message ?? (doc.ok ? `${verb} 已执行` : '执行失败')
  } catch (e) {
    cmdNote.value = e instanceof Error ? e.message : String(e)
  } finally {
    busy.value = false
    void store.refresh()
  }
}
</script>

<template>
  <template v-if="snap">
    <!-- countdown bar -->
    <div class="glass card countdown-bar">
      <div>
        <div class="card-title">
          <span class="mkt-identity">{{ marketTypeText || '市场' }}</span>
          <span class="sub" style="margin-left: 6px">{{ marketName ?? '未激活插件' }}</span>
          <span class="cd-slot">#{{ round?.slot ?? '—' }}</span>
        </div>
        <div class="sub">
          {{ round?.ageSec ?? '—' }}s 已过 ·
          <span class="cd-state" :class="round?.canTrade ? 'on' : 'off'">
            {{ round?.canTrade ? 'TRADING' : 'WAITING' }}
          </span>
        </div>
      </div>
      <div class="cd-timer" :class="{ dim: !round }">{{ leftText }}</div>
      <div class="btn-group">
        <button class="life-btn start" :disabled="busy" @click="sendLifecycle('start')">启动</button>
        <button class="life-btn stop" :disabled="busy" @click="sendLifecycle('stop')">停止</button>
      </div>
    </div>
    <div v-if="cmdNote" class="sub" style="margin: 6px 2px">命令结果：{{ cmdNote }}</div>

    <!-- market price cards -->
    <div class="prices-grid" v-if="prices.length">
      <div v-for="m in prices" :key="m.asset" class="glass card price-card">
        <div class="pa-asset">{{ m.asset }}</div>
        <div class="pa-row">
          <div class="pa-col">
            <div class="pa-label">UP</div>
            <div class="pa-price up">{{ m.up.toFixed(3) }}</div>
          </div>
          <div class="pa-col">
            <div class="pa-label">DOWN</div>
            <div class="pa-price down">{{ m.down.toFixed(3) }}</div>
          </div>
        </div>
        <div class="pa-spread">{{ spreadCents(m) }}</div>
      </div>
    </div>
    <div v-else class="glass card" style="margin: 14px 0">
      <div class="card-title">行情</div>
      <div class="empty">暂无行情数据（等待报价插件）</div>
    </div>

    <!-- stats row -->
    <div class="stats-row">
      <div class="glass card">
        <h2 class="card-title">累计净 PnL</h2>
        <div class="pnl-big" :class="pnlNetClass">{{ money(pnlNet) }}</div>
        <div class="spark" v-html="sparkline" />
      </div>
      <div class="glass card">
        <h2 class="card-title">胜率分布</h2>
        <div class="pnl-big" style="font-size: 30px">{{ winRows.wr }}%</div>
        <div class="range-row">
          <span class="sub" style="font-weight: 700; color: var(--bk-green)">盈利 {{ winRows.w }}</span>
          <span class="sub" style="font-weight: 700; color: var(--bk-red)">亏损 {{ winRows.l }}</span>
        </div>
        <div v-for="b in winRows.bands" :key="b.label" class="range-row">
          <span class="sub">{{ b.label }}</span><span class="num-mono">{{ b.count }}</span>
        </div>
      </div>
      <div class="glass card">
        <h2 class="card-title">交易统计</h2>
        <div class="grid grid-stats">
          <div><div class="stat-name">笔数</div><div class="num-mono big-num">{{ tradeStats.count }}</div></div>
          <div><div class="stat-name">今日</div><div class="num-mono big-num">{{ tradeStats.today }}</div></div>
          <div><div class="stat-name">成交额</div><div class="num-mono big-num">{{ tradeStats.volume }}</div></div>
          <div><div class="stat-name">均笔</div><div class="num-mono big-num">{{ tradeStats.avg }}</div></div>
        </div>
      </div>
      <div class="glass card">
        <h2 class="card-title">今日表现</h2>
        <div class="range-row"><span class="sub">最佳单笔</span><span class="num-mono" style="color: var(--bk-green)">{{ tradeStats.best }}</span></div>
        <div class="range-row"><span class="sub">最差单笔</span><span class="num-mono" style="color: var(--bk-red)">{{ tradeStats.worst }}</span></div>
        <div class="range-row"><span class="sub">运行模式</span><span class="num-mono">{{ snap.mode ?? '—' }}</span></div>
        <div class="range-row"><span class="sub">市场轮次</span><span class="num-mono">{{ round?.markets ?? '—' }}</span></div>
        <div class="range-row">
          <span class="sub">{{ isDry ? '模拟余额' : '交易所余额' }}</span>
          <span class="num-mono" :class="isDry ? 'dim' : ''">
            ${{ (snap.balance?.balance ?? 0).toFixed(2) }}{{ isDry ? '（模拟）' : '' }}
          </span>
        </div>
        <div v-if="!isDry && walletAddr" class="range-row">
          <span class="sub">钱包</span>
          <span class="num-mono dim" style="font-size: 11px">{{ walletAddr.slice(0, 6) }}…{{ walletAddr.slice(-4) }}</span>
        </div>
      </div>
    </div>

    <!-- positions / history tabs -->
    <div class="glass card" style="margin-top: 14px">
      <div class="tabstrip">
        <button class="tab" :class="{ active: tab === 'positions' }" @click="tab = 'positions'">当前持仓（{{ positions.length }}）</button>
        <button class="tab" :class="{ active: tab === 'history' }" @click="tab = 'history'">历史订单（{{ historyRows.length }}）</button>
      </div>
      <template v-if="tab === 'positions'">
        <table v-if="positions.length">
          <thead>
            <tr><th>资产</th><th>方向</th><th>入场</th><th>现价</th><th>份额</th><th>浮动</th><th>剩余</th></tr>
          </thead>
          <tbody>
            <tr v-for="(p, i) in positions" :key="`${p.asset}-${i}`">
              <td style="font-weight: 600">{{ p.asset }}</td>
              <td><span class="badge" :class="p.direction === 'up' ? 'on' : 'warn'">{{ p.direction.toUpperCase() }}</span></td>
              <td class="num-mono">{{ p.entryPrice.toFixed(3) }}</td>
              <td class="num-mono">{{ p.currentPrice.toFixed(3) }}</td>
              <td class="num-mono">{{ p.shares ?? '—' }}</td>
              <td :class="moneyCls(p.unrealizedPct)" class="num-mono" :style="{ color: p.unrealizedPct >= 0 ? 'var(--bk-green)' : 'var(--bk-red)' }">
                {{ p.unrealizedPct >= 0 ? '+' : '' }}{{ p.unrealizedPct.toFixed(1) }}%
              </td>
              <td class="num-mono">{{ p.remainingSec ?? '—' }}s</td>
            </tr>
          </tbody>
        </table>
        <div v-else class="empty">无持仓</div>
      </template>
      <template v-else>
        <div style="overflow-x: auto">
          <table v-if="historyRows.length">
            <thead>
              <tr><th>资产</th><th>方向</th><th>入场→平仓</th><th>份额</th><th>净 PnL</th><th>收益率</th><th>费用</th><th>原因</th><th>持仓时长</th></tr>
            </thead>
            <tbody>
              <tr v-for="t in historyRows" :key="t.id">
                <td style="font-weight: 600">{{ t.asset }}</td>
                <td><span class="badge" :class="t.direction === 'up' ? 'on' : 'warn'">{{ t.direction.toUpperCase() }}</span></td>
                <td class="num-mono">{{ t.entryPrice.toFixed(3) }} → {{ t.exitPrice.toFixed(3) }}</td>
                <td class="num-mono">{{ t.shares }}</td>
                <td class="num-mono" :style="{ color: t.netPnlUsd >= 0 ? 'var(--bk-green)' : 'var(--bk-red)' }">{{ money(t.netPnlUsd) }}</td>
                <td class="num-mono">{{ (t.netPnlPct ?? 0).toFixed(2) }}%</td>
                <td class="num-mono dim">{{ (t.feesUsd ?? 0).toFixed(2) }}</td>
                <td class="sub">{{ t.exitReason || '—' }}</td>
                <td class="num-mono dim">{{ t.holdTimeSec ?? 0 }}s</td>
              </tr>
            </tbody>
          </table>
          <div v-else class="empty">暂无平仓记录</div>
        </div>
      </template>
    </div>
  </template>
  <div v-else class="glass card empty">{{ store.loading ? '加载中…' : '暂无快照数据' }}</div>
</template>

<style scoped>
.countdown-bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
}
.mkt-identity {
  display: inline-block;
  background: var(--bk-gold-soft);
  color: var(--bk-gold);
  font-weight: 700;
  font-size: 12px;
  padding: 3px 10px;
  border-radius: 999px;
}
.cd-slot { color: var(--bk-text-dim); margin-left: 8px; font-weight: 600; }
.cd-state { font-weight: 700; letter-spacing: 0.5px; }
.cd-state.on { color: var(--bk-green); }
.cd-state.off { color: var(--bk-gold); }
.cd-timer {
  font-size: 36px;
  font-weight: 800;
  color: var(--bk-text);
  font-variant-numeric: tabular-nums;
  letter-spacing: 2px;
}
.cd-timer.dim { color: var(--bk-text-dim); }
.btn-group { display: flex; gap: 10px; }
.life-btn {
  border: none;
  border-radius: 10px;
  padding: 10px 22px;
  font-weight: 700;
  font-size: 13px;
  cursor: pointer;
  color: inherit;
  font-family: inherit;
  transition: all 0.15s;
}
.life-btn.start { background: var(--bk-green); color: #04240f; }
.life-btn.start:hover { filter: brightness(1.1); }
.life-btn.stop { background: var(--bk-red); color: #fff; }
.life-btn.stop:hover { filter: brightness(1.1); }
.life-btn:disabled { opacity: 0.5; pointer-events: none; }

.prices-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
  gap: 14px;
  margin-top: 14px;
}
.price-card { text-align: center; padding: 16px; }
.pa-asset { font-weight: 700; margin-bottom: 10px; }
.pa-row { display: flex; justify-content: space-around; }
.pa-label { font-size: 10px; text-transform: uppercase; color: var(--bk-text-dim); letter-spacing: 0.5px; }
.pa-price { font-size: 22px; font-weight: 800; margin-top: 2px; font-variant-numeric: tabular-nums; }
.pa-price.up { color: var(--bk-green); }
.pa-price.down { color: var(--bk-red); }
.pa-spread {
  font-size: 11px;
  color: var(--bk-text-dim);
  margin-top: 8px;
  padding-top: 8px;
  border-top: 1px solid var(--bk-border);
}

.stats-row {
  display: grid;
  grid-template-columns: 1.5fr 1fr 1fr 1fr;
  gap: 18px;
  margin-top: 14px;
}
@media (max-width: 960px) {
  .stats-row { grid-template-columns: 1fr; }
}
.pnl-big { font-size: 34px; font-weight: 800; letter-spacing: -1px; }
.pnl-big.pos { color: var(--bk-green); }
.pnl-big.neg { color: var(--bk-red); }
.spark { height: 64px; margin-top: 12px; }
.spark :deep(svg) { width: 100%; height: 100%; display: block; }
.range-row {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 5px 0;
  font-size: 12px;
  border-bottom: 1px solid var(--bk-border);
}
.range-row:last-child { border-bottom: none; }
.big-num { font-size: 20px; font-weight: 800; }
.dim { color: var(--bk-text-dim); }
.tabstrip { display: flex; gap: 8px; margin-bottom: 12px; }
</style>
