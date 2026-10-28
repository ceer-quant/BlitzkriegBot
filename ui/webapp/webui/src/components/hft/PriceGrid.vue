<script setup lang="ts">
/**
 * 行情网格 — the UP/DOWN price tiles for the venue-filtered, gate-cleared rows
 * (`visiblePrices`), plus the three mutually-exclusive non-data faces the page
 * already distinguished: 未绑定 (no venue bound → no market data at all),
 * Loading (venue switched, new venue's first tagged row not here yet), and the
 * empty-state card with its 网络与轮次 dual diagnosis (issue 488 + the round
 * branch). Pure presentation — every input arrives as a prop; the Loading/
 * 未绑定 faces stay gated by the same named props so the 清空 semantics stay
 * greppable (hft-venue-switch 门禁扫的就是这些消费点).
 */
import { TrendingUp, TrendingDown } from 'lucide-vue-next'
import type { MarketPrice } from '@/api/client'
import Card from '@/components/ui/card/Card.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import { cents } from '@/lib/format'

defineProps<{
  /** Gate-cleared, venue-filtered rows (empty while the Loading gate is open). */
  visiblePrices: MarketPrice[]
  /** 未绑定态：该账户没绑交易所 —— 不显示任何行情数据。 */
  venueUnbound: boolean
  /** venue 切换 Loading：旧所行已清空，等新所首行数据到达。 */
  venueLoading: boolean
  /** 行情身份 venue（Loading 文案的主语）。 */
  activeVenue: string
  /** Loading 文案：从哪个 venue 切出（已解除订阅要如实说）。 */
  venueSwitchFrom: string
  /** 行情插件名（empty-state 文案用；null = 尚未接入）。 */
  marketName: string | null | undefined
  /** 空卡的轮次分支：探针/空判信号判定「该所可能不支持此轮次」。 */
  venueRoundUnsupported: boolean
  /** 空态文案里的当前轮次档位名（5m/15m/1h/…）。 */
  roundDurLabel: string
}>()

function spreadCents(m: MarketPrice): number {
  return Math.max(0, m.up + m.down - 1) * 100
}
</script>

<template>
  <!-- 未绑定态：账户没绑交易所 → 不显示任何行情数据（含旧所残留），
       指引去账户切换器绑定。老面板/单账户（无账户读数）不进这里。 -->
  <Card v-if="venueUnbound" class="mt-3.5">
    <EmptyState
      text="该账户未绑定交易所"
      hint="行情数据源跟随账户绑定的交易所 —— 到右上角账户切换器给该账户绑定交易所（绑定即启用），行情卡会立即跟随切换。"
      compact
    />
  </Card>
  <!-- venue 切换 Loading：旧所行已清空，等新所首行数据到达才渲染 ——
       决不残留旧所盘口。 -->
  <Card v-else-if="venueLoading" class="mt-3.5">
    <EmptyState
      loading
      :text="`正在接入 ${activeVenue} 的行情…`"
      :hint="venueSwitchFrom ? `已解除 ${venueSwitchFrom} 的订阅与盘口显示；${activeVenue} 的首行数据到达后本卡立即渲染。` : '新交易所的首行数据到达后本卡立即渲染。'"
      compact
    />
  </Card>
  <div v-else-if="visiblePrices.length" class="mt-3.5 grid gap-3.5 sm:grid-cols-2 xl:grid-cols-4">
    <div
      v-for="m in visiblePrices"
      :key="`${m.venue ?? ''}:${m.asset}`"
      class="glass card-pad transition-transform duration-200 hover:-translate-y-0.5"
    >
      <div class="flex items-center justify-between">
        <span class="flex min-w-0 items-center gap-1.5">
          <span class="truncate text-[13px] font-bold tracking-wide">{{ m.asset }}</span>
        </span>
        <Badge :variant="spreadCents(m) <= 1 ? 'up' : 'gold'">
          {{ spreadCents(m).toFixed(1) }}¢
        </Badge>
      </div>
      <div class="mt-3 grid grid-cols-2 gap-3">
        <div class="rounded-md border border-up/25 bg-up/8 px-2.5 py-2">
          <div class="flex items-center gap-1 text-[10.5px] font-semibold text-up">
            <TrendingUp class="size-3" />UP
          </div>
          <div class="stat-num mt-1 text-[21px] leading-none text-up">
            <RollingNumber :value="Number(m.up).toFixed(3)" />
          </div>
          <div class="mt-0.5 text-[10px] text-faint-fg num">
            <RollingNumber :value="cents(m.up)" />
          </div>
        </div>
        <div class="rounded-md border border-down/25 bg-down/8 px-2.5 py-2">
          <div class="flex items-center gap-1 text-[10.5px] font-semibold text-down">
            <TrendingDown class="size-3" />DOWN
          </div>
          <div class="stat-num mt-1 text-[21px] leading-none text-down">
            <RollingNumber :value="Number(m.down).toFixed(3)" />
          </div>
          <div class="mt-0.5 text-[10px] text-faint-fg num">
            <RollingNumber :value="cents(m.down)" />
          </div>
        </div>
      </div>
    </div>
  </div>
  <Card v-else class="mt-3.5">
    <!--
      issue 488: 连不上 ≠ 未接入。插件已接入（marketName 有值）但行情卡空 =
      数据源拉不到（网络/代理/对端），指向网络自检而不是叫操作员去绑定
      一个已经接入的插件 —— 那条提示是误导。

      轮次分支：探针全通过（连接正常）时，空卡的真因不再是网络，而是当前
      轮次档位在该所没有市场（有的所只带 15m/daily 两个轮次系列，其余档位
      discovery 拿到空集）——此时指向「切换轮次」才对。
    -->
    <EmptyState
      v-if="venueRoundUnsupported"
      :text="`交易所连接正常，但是无法获取对应轮次（${roundDurLabel}），请尝试切换轮次`"
      hint="该交易所未必提供当前轮次长度的市场 —— 到「设置 · 通用」的轮次时间周期卡切换档位（下次会话生效），或换绑定其它交易所的账户。"
      compact
    />
    <EmptyState
      v-else
      :text="marketName ? `行情插件 ${marketName} 已接入，但暂无行情到达` : '暂无行情数据（等待报价插件）'"
      :hint="marketName
        ? '通常是网络/代理拦住了数据源域名。运行网关命令 netcheck 做网络自检，或检查本机代理是否放行该插件的 API 域名。'
        : '行情按账户绑定自动接入：在右上角账户切换器里给账户绑定交易所（绑定即启用）。'"
      compact
    />
  </Card>
</template>
