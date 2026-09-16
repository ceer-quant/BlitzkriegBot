<script setup lang="ts">
/**
 * 总览 — the cockpit. KPI tiles (balance / round / trades / mode), a live
 * equity curve, engine counters, rejection attribution and open positions.
 * Every number the old hft.html surfaced is reachable from here or 行情面板.
 */
import { computed } from 'vue'
import {
  CircleDot, ShieldCheck, ArrowUpRight, ArrowDownRight, TriangleAlert,
} from 'lucide-vue-next'
import { usePanelStore } from '@/stores/panel'
import {
  num, compact, money, signedMoney, winRatePct, pct, shortAddr, duration,
} from '@/lib/format'
import { reconcileBalance } from '@/lib/balance'
import StatTile from '@/components/ui/stat/StatTile.vue'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import EquityCurve from '@/components/charts/EquityCurve.vue'
import RejectionChart from '@/components/charts/RejectionChart.vue'

const store = usePanelStore()

const snap = computed(() => store.snapshot)
const strategyRows = computed(() => store.strategyRows)

// ── balance (dry cash is simulation seed, live cash is venue funds) ─────────
const isDry = computed(() => (snap.value?.mode ?? 'dry') === 'dry')
const balanceTitle = computed(() => (isDry.value ? '模拟余额' : '交易所余额'))
const balance = computed(() => snap.value?.balance ?? null)
const walletAddr = computed(() => snap.value?.wallet?.funder ?? snap.value?.wallet?.signer ?? null)

// ── trades ─────────────────────────────────────────────────────────────────
const tradeRows = computed(() => store.tradeRows)
const summary = computed(() => snap.value?.tradeSummary ?? null)
const trades = computed(() => {
  const s = summary.value
  if (s && (s.totalTrades ?? 0) > 0) {
    return {
      count: s.totalTrades ?? 0,
      net: s.totalNetPnl ?? 0,
      fees: s.totalFees ?? 0,
      winRate: winRatePct(s.winRate),
      win: s.wins ?? 0,
      loss: s.losses ?? 0,
    }
  }
  const rows = tradeRows.value
  const win = rows.filter((t) => (Number(t.netPnlUsd) || 0) > 0).length
  return {
    count: rows.length,
    net: rows.reduce((a, t) => a + (Number(t.netPnlUsd) || 0), 0),
    fees: rows.reduce((a, t) => a + (Number(t.feesUsd) || 0), 0),
    winRate: rows.length ? (win / rows.length) * 100 : 0,
    win,
    loss: rows.length - win,
  }
})

/**
 * 模拟余额 vs 本金 + 净利润. The core's own summary spans every run its trade log
 * has ever seen, while the cash balance only reflects the current process, so
 * the two are not automatically equal. Show the arithmetic and the residual
 * rather than implying an identity that may not hold.
 */
const recon = computed(() => reconcileBalance(balance.value, trades.value.net))

const round = computed(() => snap.value?.round ?? null)
const positions = computed(() => snap.value?.positions ?? [])
const engine = computed(() => snap.value?.stats ?? {})

const counters = computed(() => [
  { label: '订单簿', value: compact(engine.value.books), tone: 'default' as const },
  { label: 'Top 快照', value: compact(engine.value.tops), tone: 'default' as const },
  { label: '轮次', value: num(engine.value.rounds), tone: 'default' as const },
  { label: '评估', value: compact(engine.value.evaluations), tone: 'default' as const },
  { label: '信号', value: compact(engine.value.signals), tone: 'gold' as const },
  {
    label: '拒单',
    value: compact(engine.value.placeRejected),
    tone: (engine.value.placeRejected ?? 0) > 0 ? ('down' as const) : ('default' as const),
  },
])

const unrealized = computed(() =>
  positions.value.reduce((a, p) => a + (Number(p.unrealizedPct) || 0) * (Number(p.shares) || 0), 0),
)
</script>

<template>
  <template v-if="snap">
    <!-- KPI row -->
    <div class="grid gap-3.5 sm:grid-cols-2 xl:grid-cols-4">
      <StatTile
        :label="balanceTitle"
        :value="money(balance?.balance)"
        :tone="isDry ? 'gold' : 'default'"
      >
        <template #sub>
          <Tooltip v-if="recon.seed !== null" :content="`余额口径：本金 ${money(recon.seed)} ＋ 已实现净利 ${signedMoney(recon.net)} − 未平仓占用 ${money(recon.reserved)}`">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              本金 {{ money(recon.seed) }} ＋ 净利 {{ signedMoney(recon.net) }}
            </span>
          </Tooltip>
          <!--
            No principal on the wire (LIVE, or a core older than the field): say
            that the ledger cannot be reconciled here rather than assert a cause
            we cannot verify, or print an equation we cannot check.
          -->
          <Tooltip v-else-if="isDry" content="余额是内核的现金账：本金随每笔成交与手续费增减，再减去挂单占用。要显示“本金 ＋ 净利”的对账结果，内核必须上报本金；当前运行中的内核未上报本金，因此这里只标注口径、不虚构等式。重启内核后即可显示完整对账。">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              现金账 · 本金未上报
            </span>
          </Tooltip>
          <template v-else>可用 {{ money(balance?.available) }} · 预留 {{ money(balance?.reserved) }}</template>
        </template>
        <div v-if="recon.drifted" class="mt-2 flex items-start gap-1.5 text-[10.5px] leading-snug text-faint-fg">
          <TriangleAlert class="mt-px size-3 shrink-0" style="color: var(--primary)" />
          <Tooltip :content="`按“本金 + 净利”推算应为 ${money((recon.seed ?? 0) + recon.net + recon.reserved)}，实际余额 ${money(recon.balance)}，差额 ${signedMoney(recon.residual)}。盈亏记录与现金流水口径不一致时会出现差额（例如运行中的内核早于费用入账修复，或记录的成交笔数跨越了多个进程），这是内核数据口径问题，不是界面算错。`">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              对账差 {{ signedMoney(recon.residual) }}
            </span>
          </Tooltip>
        </div>
        <div v-if="isDry" class="mt-2 flex items-center gap-1.5 text-[10.5px] text-faint-fg">
          <CircleDot class="size-3" /> 模拟资金，非真实资产
        </div>
        <div v-else-if="walletAddr" class="mt-2 text-[11px] text-faint-fg num">{{ shortAddr(walletAddr) }}</div>
      </StatTile>

      <StatTile label="当前轮次" :value="round ? `#${round.slot}` : '—'">
        <template #sub>
          <span v-if="round">{{ duration(round.ageSec) }} 已过 · {{ duration(round.timeLeftSec) }} 剩余</span>
          <span v-else>等待行情插件</span>
        </template>
        <div v-if="round" class="mt-2">
          <Badge :variant="round.canTrade ? 'up' : 'gold'" dot>{{ round.canTrade ? '可交易' : '等待窗口' }}</Badge>
        </div>
      </StatTile>

      <StatTile
        label="已平仓交易"
        :value="num(trades.count)"
        :tone="trades.net >= 0 ? 'up' : 'down'"
      >
        <template #sub>
          扣费净利 {{ signedMoney(trades.net) }} · 费用 {{ money(trades.fees) }}
        </template>
        <div class="mt-2 flex items-center gap-2">
          <Badge variant="up"><ArrowUpRight class="size-3" />{{ trades.win }}</Badge>
          <Badge variant="down"><ArrowDownRight class="size-3" />{{ trades.loss }}</Badge>
          <span class="text-[11px] text-faint-fg num">胜率 {{ pct(trades.winRate) }}</span>
        </div>
      </StatTile>

      <StatTile
        label="运行模式"
        :value="(snap.mode ?? '—').toUpperCase()"
        :tone="snap.mode === 'dry' ? 'gold' : 'up'"
      >
        <template #sub>
          {{ snap.connected ? '引擎已连接' : '引擎未连接' }}
          <template v-if="positions.length"> · {{ positions.length }} 持仓</template>
        </template>
        <div class="mt-2 flex items-center gap-1.5">
          <ShieldCheck class="size-3.5" :style="{ color: snap.mode === 'dry' ? 'var(--primary)' : 'var(--up)' }" />
          <span class="text-[10.5px] text-faint-fg">
            {{ snap.mode === 'live' ? '实盘已连接' : 'Live 交易未启用' }}
          </span>
        </div>
      </StatTile>
    </div>

    <!-- equity + counters -->
    <div class="mt-3.5 grid gap-3.5 xl:grid-cols-[1.55fr_1fr]">
      <Card>
        <CardHeader label="权益曲线">
          <template #title>
            <span class="stat-num text-[15px]" :class="trades.net >= 0 ? 'text-up' : 'text-down'">
              {{ signedMoney(trades.net) }}
            </span>
          </template>
          <template #action>
            <Badge variant="default" dot>{{ trades.count }} 笔</Badge>
          </template>
        </CardHeader>
        <EquityCurve :rows="tradeRows" :height="196" />
      </Card>

      <Card>
        <CardHeader label="引擎计数" />
        <div class="grid grid-cols-2 gap-x-5 gap-y-3.5 sm:grid-cols-3 xl:grid-cols-2">
          <div v-for="c in counters" :key="c.label" class="min-w-0">
            <div class="label-micro">{{ c.label }}</div>
            <div
              class="stat-num mt-1 text-[19px] leading-none"
              :class="{
                'text-gold': c.tone === 'gold',
                'text-down': c.tone === 'down',
              }"
            >{{ c.value }}</div>
          </div>
        </div>
        <div
          v-if="unrealized"
          class="mt-4 flex items-center justify-between border-t border-line pt-3 text-[11.5px]"
        >
          <span class="text-faint-fg">未平仓浮动合计</span>
          <span class="stat-num" :class="unrealized >= 0 ? 'text-up' : 'text-down'">
            {{ signedMoney(unrealized) }}
          </span>
        </div>
      </Card>
    </div>

    <!-- rejection attribution + positions -->
    <div class="mt-3.5 grid gap-3.5 xl:grid-cols-2">
      <Card>
        <CardHeader label="策略拒单原因分布">
          <template #action>
            <Badge variant="default">{{ strategyRows.length }} 策略</Badge>
          </template>
        </CardHeader>
        <RejectionChart :rows="strategyRows" :height="210" />
      </Card>

      <Card dense>
        <CardHeader label="当前持仓">
          <template #action>
            <Badge :variant="positions.length ? 'gold' : 'default'">{{ positions.length }}</Badge>
          </template>
        </CardHeader>
        <div v-if="positions.length" class="-mx-1 overflow-x-auto">
          <table class="w-full text-[13px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">资产</th>
                <th class="label-micro px-2 pb-2">方向</th>
                <th class="label-micro px-2 pb-2 text-right">入场</th>
                <th class="label-micro px-2 pb-2 text-right">现价</th>
                <th class="label-micro px-2 pb-2 text-right">份额</th>
                <th class="label-micro px-2 pb-2 text-right">浮动</th>
                <th class="label-micro px-2 pb-2 text-right">剩余</th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(p, i) in positions"
                :key="`${p.asset}-${i}`"
                class="border-t border-line transition-colors hover:bg-panel-2"
              >
                <td class="px-2 py-2.5 font-semibold">{{ p.asset }}</td>
                <td class="px-2 py-2.5">
                  <Badge :variant="p.direction === 'up' ? 'up' : 'down'">
                    {{ p.direction.toUpperCase() }}
                  </Badge>
                </td>
                <td class="px-2 py-2.5 text-right num text-muted-fg">{{ Number(p.entryPrice).toFixed(3) }}</td>
                <td class="px-2 py-2.5 text-right num">{{ Number(p.currentPrice).toFixed(3) }}</td>
                <td class="px-2 py-2.5 text-right num text-muted-fg">{{ p.shares ?? '—' }}</td>
                <td
                  class="px-2 py-2.5 text-right num font-semibold"
                  :class="p.unrealizedPct >= 0 ? 'text-up' : 'text-down'"
                >{{ Number(p.unrealizedPct) >= 0 ? '+' : '' }}{{ Number(p.unrealizedPct).toFixed(1) }}%</td>
                <td class="px-2 py-2.5 text-right num text-faint-fg">
                  {{ p.remainingSec !== undefined ? duration(p.remainingSec) : '—' }}
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <EmptyState v-else text="无持仓" compact />
      </Card>
    </div>

    <div v-if="snap.lastError" class="mt-3.5">
      <AlertBanner tone="warn" title="引擎最近错误">{{ snap.lastError }}</AlertBanner>
    </div>
  </template>

  <Card v-else class="mt-3.5">
    <EmptyState :loading="store.loading" text="暂无快照数据" />
  </Card>
</template>
