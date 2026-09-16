<script setup lang="ts">
/**
 * 策略 — 分策略表现总表 + 风控拦截归因（E9-g）。每行可直接启用/停用
 * （网关 `strategy <name> on|off`）；拒单原因按需展开。
 */
import { computed, ref } from 'vue'
import { AlertTriangle, ChevronDown, ShieldCheck, ShieldOff } from 'lucide-vue-next'
import { api, type StrategyStatsRow } from '@/api/client'
import { usePanelStore } from '@/stores/panel'
import { num, signedMoney, pct } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import StatTile from '@/components/ui/stat/StatTile.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'

const store = usePanelStore()
const rows = computed<StrategyStatsRow[]>(() => store.strategyRows)

const busy = ref<string | null>(null)
const flash = ref<{ name: string; ok: boolean; msg: string } | null>(null)

async function toggle(r: StrategyStatsRow, next: boolean): Promise<void> {
  busy.value = r.name
  flash.value = null
  try {
    const res = await api.setStrategy(r.name, next)
    flash.value = {
      name: r.name,
      ok: res.ok !== false,
      msg: res.message ?? `${r.name} 已${next ? '启用' : '停用'}`,
    }
    await store.refresh()
  } catch (e) {
    flash.value = { name: r.name, ok: false, msg: e instanceof Error ? e.message : String(e) }
  } finally {
    busy.value = null
    setTimeout(() => { flash.value = null }, 5000)
  }
}

const totals = computed(() => {
  const rs = rows.value
  const sum = (f: (r: StrategyStatsRow) => number) => rs.reduce((a, r) => a + (Number(f(r)) || 0), 0)
  const wins = sum((r) => r.wins)
  const losses = sum((r) => r.losses)
  return {
    count: rs.length,
    enabled: rs.filter((r) => r.enabled).length,
    placed: sum((r) => r.ordersPlaced),
    rejected: sum((r) => r.ordersRejected),
    blocked: sum((r) => r.blockedTiming + r.blockedMomentum),
    closed: sum((r) => r.closedTrades),
    wins,
    losses,
    net: sum((r) => Number(r.netPnlUsd) || 0),
    winRate: wins + losses ? (wins / (wins + losses)) * 100 : 0,
  }
})

function causePills(r: StrategyStatsRow): { name: string; n: number }[] {
  return Object.entries(r.rejectionCauses ?? {})
    .filter(([, n]) => Number(n) > 0)
    .sort((a, b) => Number(b[1]) - Number(a[1]))
    .map(([name, n]) => ({ name, n: Number(n) }))
}

const expanded = ref<string | null>(null)
function toggleExpand(name: string): void {
  expanded.value = expanded.value === name ? null : name
}
</script>

<template>
  <div v-if="rows.length" class="rise-in">
    <!-- fleet KPIs -->
    <div class="grid gap-3.5 sm:grid-cols-2 xl:grid-cols-4">
      <StatTile label="策略规模" :value="String(totals.enabled)" tone="gold">
        <template #sub>共 {{ totals.count }} 个已注册</template>
      </StatTile>
      <StatTile label="下单 / 拒单" :value="num(totals.placed)">
        <template #sub>
          <span class="text-down font-semibold">{{ num(totals.rejected) }}</span> 笔被风控拒绝 ·
          限额/时机/动量挡 {{ num(totals.blocked) }}
        </template>
      </StatTile>
      <StatTile label="平仓 / 胜率" :value="num(totals.closed)" :tone="totals.winRate >= 50 ? 'up' : 'down'">
        <template #sub>{{ totals.wins }} 盈 / {{ totals.losses }} 亏 · 胜率 {{ pct(totals.winRate) }}</template>
      </StatTile>
      <StatTile
        label="净 PnL（扣费）"
        :value="signedMoney(totals.net)"
        :tone="totals.net >= 0 ? 'up' : 'down'"
      >
        <template #sub>分策略账本合计</template>
      </StatTile>
    </div>

    <div v-if="flash" class="mt-3.5">
      <AlertBanner :tone="flash.ok ? 'info' : 'error'" :title="flash.name" dismissible @dismiss="flash = null">
        {{ flash.msg }}
      </AlertBanner>
    </div>

    <Card class="mt-3.5" dense>
      <CardHeader label="策略表现">
        <template #action>
          <Badge variant="gold" dot>{{ rows.length }} 行</Badge>
        </template>
      </CardHeader>

      <div class="-mx-2 overflow-x-auto">
        <table class="w-full min-w-[1080px] text-[13px]">
          <thead>
            <tr class="text-left">
              <th class="label-micro px-2 pb-2.5">策略</th>
              <th class="label-micro px-2 pb-2.5">启用</th>
              <th class="label-micro px-2 pb-2.5">来源</th>
              <th class="label-micro px-2 pb-2.5 text-right">下单</th>
              <th class="label-micro px-2 pb-2.5 text-right">拒单</th>
              <th class="label-micro px-2 pb-2.5 text-right">限额拒</th>
              <th class="label-micro px-2 pb-2.5 text-right">时机挡</th>
              <th class="label-micro px-2 pb-2.5 text-right">动量挡</th>
              <th class="label-micro px-2 pb-2.5 text-right">豁免</th>
              <th class="label-micro px-2 pb-2.5 text-right">平仓</th>
              <th class="label-micro px-2 pb-2.5 text-right">胜 / 负</th>
              <th class="label-micro px-2 pb-2.5 text-right">净 PnL</th>
              <th class="label-micro px-2 pb-2.5 text-right">原因</th>
            </tr>
          </thead>
          <tbody>
            <template v-for="r in rows" :key="r.name">
              <tr class="border-t border-line transition-colors hover:bg-panel-2">
                <td class="px-2 py-2.5 font-semibold">
                  <span class="inline-flex items-center gap-2">
                    <span
                      class="size-1.5 shrink-0 rounded-full"
                      :style="{ background: r.enabled ? 'var(--up)' : 'var(--faint-fg)' }"
                    />
                    {{ r.name }}
                  </span>
                </td>
                <td class="px-2 py-2.5">
                  <Switch
                    :model-value="r.enabled"
                    :disabled="busy === r.name"
                    @update:model-value="toggle(r, $event)"
                  />
                </td>
                <td class="px-2 py-2.5 text-[12px] text-muted-fg">{{ r.source }}</td>
                <td class="px-2 py-2.5 text-right num">{{ num(r.ordersPlaced) }}</td>
                <td
                  class="px-2 py-2.5 text-right num"
                  :class="r.ordersRejected ? 'font-semibold text-down' : 'text-faint-fg'"
                >{{ num(r.ordersRejected) }}</td>
                <td class="px-2 py-2.5 text-right num text-muted-fg">{{ num(r.limitRejected) }}</td>
                <td
                  class="px-2 py-2.5 text-right num"
                  :class="r.blockedTiming ? 'font-semibold text-primary' : 'text-faint-fg'"
                >{{ num(r.blockedTiming) }}</td>
                <td
                  class="px-2 py-2.5 text-right num"
                  :class="r.blockedMomentum ? 'font-semibold text-primary' : 'text-faint-fg'"
                >{{ num(r.blockedMomentum) }}</td>
                <td class="px-2 py-2.5 text-right">
                  <Tooltip
                    v-if="r.gateExemptions?.length"
                    :content="`声明豁免：${r.gateExemptions.join(' / ')}`"
                  >
                    <span class="num inline-flex items-center gap-1 font-semibold text-info">
                      <ShieldOff class="size-3" />{{ r.gateExemptions.length }}
                    </span>
                  </Tooltip>
                  <span v-else class="num text-faint-fg">0</span>
                </td>
                <td class="px-2 py-2.5 text-right num">{{ num(r.closedTrades) }}</td>
                <td class="px-2 py-2.5 text-right num whitespace-nowrap">
                  <span class="text-up">{{ num(r.wins) }}</span>
                  <span class="text-faint-fg"> / </span>
                  <span class="text-down">{{ num(r.losses) }}</span>
                </td>
                <td
                  class="px-2 py-2.5 text-right num font-semibold"
                  :class="Number(r.netPnlUsd) >= 0 ? 'text-up' : 'text-down'"
                >{{ signedMoney(r.netPnlUsd) }}</td>
                <td class="px-2 py-2.5 text-right">
                  <Button
                    v-if="causePills(r).length"
                    variant="ghost"
                    size="sm"
                    :title="expanded === r.name ? '收起' : '展开拒单原因'"
                    @click="toggleExpand(r.name)"
                  >
                    <AlertTriangle class="size-3.5 text-primary" />
                    <span class="num">{{ causePills(r).length }}</span>
                    <ChevronDown
                      class="size-3 transition-transform"
                      :class="expanded === r.name && 'rotate-180'"
                    />
                  </Button>
                  <ShieldCheck v-else class="ml-auto size-4 text-up/45" />
                </td>
              </tr>
              <tr v-if="expanded === r.name" :key="`${r.name}-causes`" class="border-t border-line bg-panel-2">
                <td colspan="13" class="px-3 py-3">
                  <div class="label-micro mb-2">拒单原因分布</div>
                  <div class="flex flex-wrap gap-2">
                    <span
                      v-for="p in causePills(r)"
                      :key="p.name"
                      class="inline-flex items-center gap-1.5 rounded-full border border-primary/30 bg-primary/12 px-2.5 py-1 text-[11.5px] text-primary"
                    >
                      {{ p.name }}
                      <span class="num font-bold">×{{ p.n }}</span>
                    </span>
                  </div>
                </td>
              </tr>
            </template>
          </tbody>
        </table>
      </div>
    </Card>
  </div>

  <Card v-else class="rise-in">
    <EmptyState :loading="store.loading" text="暂无策略数据" />
  </Card>
</template>
