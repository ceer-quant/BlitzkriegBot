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
import { refusalProfile, refusalTotals, REFUSAL_NOTE } from '@/lib/rejections'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import StatTile from '@/components/ui/stat/StatTile.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'

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
    /** Refusal attribution — two disjoint families, see `lib/rejections.ts`. */
    refusal: refusalTotals(rs),
    closed: sum((r) => r.closedTrades),
    wins,
    losses,
    net: sum((r) => Number(r.netPnlUsd) || 0),
    winRate: wins + losses ? (wins / (wins + losses)) * 100 : 0,
  }
})

const profile = (r: StrategyStatsRow) => refusalProfile(r)

/**
 * The tooltip must say how FAR an opt-out reaches (D-31). Two strategies both
 * showing 「时机」 can behave completely differently: one stops at the kernel's
 * window, the other enters right up to the last second. Without the floor the
 * badge would report them identically.
 */
function exemptionTooltip(r: StrategyStatsRow): string {
  const gates = r.gateExemptions.join(' / ')
  const named =
    r.gateExemptions.includes('timing') && r.gateExemptionTimingFloorSec != null
      ? `，其中时机闸门仅在剩余 ≥${r.gateExemptionTimingFloorSec}s 时豁免`
      : ''
  return `声明豁免：${gates}${named}`
}

function causePills(r: StrategyStatsRow): { name: string; n: number }[] {
  return refusalProfile(r).causes
}

const expanded = ref<string | null>(null)
function toggleExpand(name: string): void {
  expanded.value = expanded.value === name ? null : name
}
</script>

<template>
  <div v-if="rows.length" class="rise-in">
    <!-- fleet KPIs -->
    <div class="grid gap-3.5 sm:grid-cols-2 xl:grid-cols-5">
      <StatTile label="策略规模" :value="String(totals.enabled)" tone="gold">
        <template #sub>共 <RollingNumber :value="totals.count" /> 个已注册</template>
      </StatTile>
      <!--
        「拒单」是两族互不包含的计数,不可相加,也不可互相解释。旧文案把
        ordersRejected 写成「笔被风控拒绝」并把 限额/时机/动量 并列在后,读起来
        像后者应当是前者的明细 —— 于是一个 3 万多的数旁边摆着四个 0,看起来像
        风控没上报,实际是风控本身也在那 3 万多里。这里按发生位置分开表述。
      -->
      <StatTile label="下单 / 下单被拒" :value="num(totals.refusal.placed)">
        <template #sub>
          <Tooltip :content="REFUSAL_NOTE">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              <span class="text-down font-semibold"><RollingNumber :value="num(totals.refusal.rejected)" /></span> 笔被拒 · 拒绝率
              <RollingNumber :value="pct(totals.refusal.refuseRate)" />
            </span>
          </Tooltip>
        </template>
      </StatTile>
      <StatTile label="信号前拦截" :value="num(totals.refusal.preGate)">
        <template #sub>
          <Tooltip :content="REFUSAL_NOTE">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              时机 <RollingNumber :value="num(totals.refusal.timing)" /> · 动量 <RollingNumber :value="num(totals.refusal.momentum)" /> · 限额
              <RollingNumber :value="num(totals.refusal.limitRejected)" />
            </span>
          </Tooltip>
        </template>
      </StatTile>
      <StatTile label="平仓 / 胜率" :value="num(totals.closed)" :tone="totals.winRate >= 50 ? 'up' : 'down'">
        <template #sub><RollingNumber :value="totals.wins" /> 盈 / <RollingNumber :value="totals.losses" /> 亏 · 胜率 <RollingNumber :value="pct(totals.winRate)" /></template>
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
      <AlertBanner
        :tone="flash.ok ? 'info' : 'error'"
        :title="flash.name"
        :hint="flash.ok ? undefined : '若持续失败，确认网关在线后重试。'"
        dismissible
        @dismiss="flash = null"
      >
        {{ flash.msg }}
      </AlertBanner>
    </div>

    <Card class="mt-3.5" dense>
      <CardHeader label="策略表现">
        <template #action>
          <Badge variant="gold" dot><RollingNumber :value="rows.length" /> 行</Badge>
        </template>
      </CardHeader>

      <div class="-mx-2 overflow-x-auto">
        <!--
          `table-fixed` + explicit colgroup, not automatic layout. Under auto
          layout the browser derives column widths from cell content, so the
          expanded detail row (`<td colspan="13">` holding the cause pills and
          the long 计数口径 paragraph) made the numeric columns want more width:
          expanding a row slid 「来源」 left by ~12px and 「下单被拒」 right by
          ~34px, so the header row visibly jolted sideways. Pinning the columns
          removes that coupling — the detail row can no longer influence the
          layout, and the widths below are the ones auto layout was already
          producing, so nothing else moves.
        -->
        <table class="w-full min-w-[1080px] table-fixed text-[13px]">
          <colgroup>
            <col class="w-[11.71%]" />
            <col class="w-[4.81%]" />
            <col class="w-[35.28%]" />
            <col class="w-[3.47%]" />
            <col class="w-[5.5%]" />
            <col class="w-[4.49%]" />
            <col class="w-[4.49%]" />
            <col class="w-[4.82%]" />
            <col class="w-[3.61%]" />
            <col class="w-[3.47%]" />
            <col class="w-[4.54%]" />
            <col class="w-[5.58%]" />
            <col class="w-[8.24%]" />
          </colgroup>
          <thead>
            <tr class="text-left">
              <th class="label-micro px-2 pb-2.5">策略</th>
              <th class="label-micro px-2 pb-2.5">启用</th>
              <th class="label-micro px-2 pb-2.5">来源</th>
              <th class="label-micro px-2 pb-2.5 text-right">下单</th>
              <th class="label-micro px-2 pb-2.5 text-right">
                <Tooltip :content="REFUSAL_NOTE">
                  <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">下单被拒</span>
                </Tooltip>
              </th>
              <th class="label-micro px-2 pb-2.5 text-right">
                <Tooltip :content="REFUSAL_NOTE">
                  <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">限额拒</span>
                </Tooltip>
              </th>
              <th class="label-micro px-2 pb-2.5 text-right">
                <Tooltip :content="REFUSAL_NOTE">
                  <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">时机挡</span>
                </Tooltip>
              </th>
              <th class="label-micro px-2 pb-2.5 text-right">
                <Tooltip :content="REFUSAL_NOTE">
                  <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">动量挡</span>
                </Tooltip>
              </th>
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
                  <span class="inline-flex min-w-0 items-center gap-2">
                    <span
                      class="size-1.5 shrink-0 rounded-full"
                      :style="{ background: r.enabled ? 'var(--up)' : 'var(--faint-fg)' }"
                    />
                    <span class="truncate" :title="r.name">{{ r.name }}</span>
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
                <td class="px-2 py-2.5 text-right num"><RollingNumber :value="num(r.ordersPlaced)" /></td>
                <td
                  class="px-2 py-2.5 text-right num"
                  :class="r.ordersRejected ? 'font-semibold text-down' : 'text-faint-fg'"
                ><RollingNumber :value="num(r.ordersRejected)" /></td>
                <td class="px-2 py-2.5 text-right num text-muted-fg"><RollingNumber :value="num(r.limitRejected)" /></td>
                <td
                  class="px-2 py-2.5 text-right num"
                  :class="r.blockedTiming ? 'font-semibold text-primary' : 'text-faint-fg'"
                ><RollingNumber :value="num(r.blockedTiming)" /></td>
                <td
                  class="px-2 py-2.5 text-right num"
                  :class="r.blockedMomentum ? 'font-semibold text-primary' : 'text-faint-fg'"
                ><RollingNumber :value="num(r.blockedMomentum)" /></td>
                <td class="px-2 py-2.5 text-right">
                  <Tooltip
                    v-if="r.gateExemptions?.length"
                    :content="exemptionTooltip(r)"
                  >
                    <span class="num inline-flex items-center gap-1 font-semibold text-info">
                      <ShieldOff class="size-3" /><RollingNumber :value="r.gateExemptions.length" />
                    </span>
                  </Tooltip>
                  <span v-else class="num text-faint-fg">0</span>
                </td>
                <td class="px-2 py-2.5 text-right num"><RollingNumber :value="num(r.closedTrades)" /></td>
                <td class="px-2 py-2.5 text-right num whitespace-nowrap">
                  <span class="text-up"><RollingNumber :value="num(r.wins)" /></span>
                  <span class="text-faint-fg"> / </span>
                  <span class="text-down"><RollingNumber :value="num(r.losses)" /></span>
                </td>
                <td
                  class="px-2 py-2.5 text-right num font-semibold"
                  :class="Number(r.netPnlUsd) >= 0 ? 'text-up' : 'text-down'"
                ><RollingNumber :value="signedMoney(r.netPnlUsd)" /></td>
                <td class="px-2 py-2.5 text-right">
                  <Button
                    v-if="profile(r).causes.length"
                    variant="ghost"
                    size="sm"
                    :title="expanded === r.name ? '收起拒单原因' : '展开拒单原因'"
                    @click="toggleExpand(r.name)"
                  >
                    <AlertTriangle class="size-3.5 text-primary" />
                    <span class="num"><RollingNumber :value="profile(r).causes.length" /></span>
                    <ChevronDown
                      class="size-3 transition-transform"
                      :class="expanded === r.name && 'rotate-180'"
                    />
                  </Button>
                  <!--
                    Refusals with no attribution on the wire: the core predates the
                    cause buckets (#65) or the bucket map arrived empty. Saying "no
                    causes" would imply nothing was refused; the shield would imply
                    all clear. Neither is true, so it says the attribution is absent.
                  -->
                  <Tooltip
                    v-else-if="profile(r).causesMissing"
                    content="该策略确实有下单被拒，但内核未上报拒单原因。通常是内核版本早于拒单归因（rejectionCauses）功能——重启内核后即可看到分类。"
                  >
                    <span class="num cursor-help text-[11.5px] text-faint-fg">
                      <RollingNumber :value="num(profile(r).rejected)" /> 未归因
                    </span>
                  </Tooltip>
                  <ShieldCheck v-else class="ml-auto size-4 text-up/45" />
                </td>
              </tr>
              <tr v-if="expanded === r.name" :key="`${r.name}-causes`" class="border-t border-line bg-panel-2">
                <td colspan="13" class="px-3 py-3">
                  <div class="label-micro mb-2">拒单原因分布（下单函数内部拒绝）</div>
                  <div class="flex flex-wrap gap-2">
                    <!-- A pill with a tone triple is a Badge — the kit's `gold`
                         variant is this exact shape, so the page composes it
                         instead of copying its tint (issue 260). -->
                    <Badge v-for="p in causePills(r)" :key="p.name" variant="gold">
                      {{ p.name }}
                      <span class="num font-bold">×<RollingNumber :value="p.n" /></span>
                    </Badge>
                  </div>
                  <!--
                    The bare counts look implausible next to a single open position,
                    so state the measurement basis here: a refusal is counted per
                    evaluation cycle, and a refused candidate is retried each cycle.
                  -->
                  <p class="mt-2.5 text-[11px] leading-snug text-faint-fg">
                    计数口径：被拒的候选不会记为「挂单中」，引擎每个评估周期（约 50ms，即每秒约 20 次）会重新提出同一信号并再次被拒，
                    因此上述数字是<span class="text-muted-fg">拒绝次数随时间的累积</span>，不是不同信号的笔数。
                    一个持续被挡的信号（例如同标的有持仓、持仓已满、熔断未解除）每小时可累积约 7 万次。
                  </p>
                </td>
              </tr>
            </template>
          </tbody>
        </table>
      </div>
    </Card>
  </div>

  <Card v-else class="rise-in">
    <EmptyState
      :loading="store.loading"
      text="暂无策略数据"
      hint="注册策略后自动出现；新策略可用 blitzkrieg-new-strategy 脚手架生成。"
    />
  </Card>
</template>
