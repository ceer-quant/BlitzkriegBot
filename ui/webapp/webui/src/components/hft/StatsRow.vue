<script setup lang="ts">
/**
 * 统计行 — the four stat cards (累计净 PnL + equity curve / 胜率分布 /
 * 交易统计 / 今日表现 + balance recon). The 1.6/1/1/1 split only applies once
 * there is room for it (`xl`); `sm:grid-cols-2` pairs them 2×2 across tablet
 * and small-laptop widths. Pure presentation: rows and computed readouts in,
 * no store access.
 */
import type { TradeRow } from '@/api/client'
import type { CumStats } from './types'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import StatRow from '@/components/ui/stat/StatRow.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import EquityCurve from '@/components/charts/EquityCurve.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import { money, signedMoney, pct, signedPct, duration } from '@/lib/format'

defineProps<{
  /** Chronological rows for the equity curve (the account+venue slice). */
  historyRows: TradeRow[]
  cumStats: CumStats
  winBands: { label: string; tone: 'up' | 'down'; count: number; frac: number }[]
  tradeStats: { count: number; today: number; todayNet: number; volume: number; avg: number }
  recon: { equity: number | null; cash: number; seed: number | null; net: number; gross: number; fees: number }
  /** 持仓浮动合计（今日表现卡的浮动行）。 */
  positionUnrealized: number
  /** DRY / LIVE 运行模式（词不是数字 — 那一行 :roll="false"）。 */
  mode: string
  isDry: boolean
  /** 本轮 open markets 数（行情轮次行；undefined = —）。 */
  roundMarkets: number | undefined
  /** 余额瓦片标注后缀（多账户时追加账户 id）。 */
  balanceLabel: string
  /** 兜底一拍的进程级账本披露（单账户/行已到时为空串）。 */
  balanceAccountNote: string
}>()
</script>

<template>
  <!--
    Four stat cards. The 1.6/1/1/1 split only applies once there is room for it
    (`xl`), but the fallback used to be a single column all the way down — at
    tablet and small-laptop widths that stacked four full-width cards into a
    long scroll. `sm:grid-cols-2` puts them 2×2 across that whole range instead,
    and 累计净 PnL keeps the wider column whenever the split is active.
  -->
  <div class="mt-3.5 grid gap-3.5 sm:grid-cols-2 xl:grid-cols-[1.6fr_1fr_1fr_1fr]">
    <Card>
      <CardHeader label="累计净 PnL">
        <template #title>
          <span class="stat-num text-[16px]" :class="cumStats.net >= 0 ? 'text-up' : 'text-down'">
            <RollingNumber :value="signedMoney(cumStats.net)" />
          </span>
        </template>
        <template #action>
          <Badge variant="default">扣费口径</Badge>
        </template>
      </CardHeader>
      <EquityCurve :rows="historyRows" :height="128" :show-axis="false" />
      <div class="mt-2 flex items-center gap-4 text-[11px] text-faint-fg">
        <span>毛利 <span class="num text-fg"><RollingNumber :value="signedMoney(cumStats.gross)" /></span></span>
        <span>费用 <span class="num text-down"><RollingNumber :value="money(cumStats.fees)" /></span></span>
      </div>
    </Card>

    <Card>
      <CardHeader label="胜率分布" />
      <div class="stat-num text-[30px] leading-none">
        <RollingNumber :value="pct(cumStats.winRate)" />
      </div>
      <div class="mt-1 flex items-center gap-3 text-[11.5px]">
        <span class="font-semibold text-up"><RollingNumber :value="cumStats.wins" /> 盈</span>
        <span class="font-semibold text-down"><RollingNumber :value="cumStats.losses" /> 亏</span>
      </div>
      <div class="mt-3.5 space-y-1.5">
        <div v-for="b in winBands" :key="b.label" class="flex items-center gap-2">
          <span class="w-[52px] shrink-0 text-[10.5px] text-faint-fg">{{ b.label }}</span>
          <span class="h-1.5 flex-1 overflow-hidden rounded-full bg-panel-2">
            <span
              class="block h-full rounded-full transition-[width] duration-500"
              :style="{
                width: `${b.frac * 100}%`,
                background: b.tone === 'up' ? 'var(--up)' : 'var(--down)',
              }"
            />
          </span>
          <span class="w-7 shrink-0 text-right text-[10.5px] text-muted-fg num"><RollingNumber :value="b.count" /></span>
        </div>
      </div>
    </Card>

    <Card>
      <CardHeader label="交易统计" />
      <div class="grid grid-cols-2 gap-x-4 gap-y-3">
        <div>
          <div class="label-micro">笔数</div>
          <div class="stat-num mt-1 text-[19px] leading-none"><RollingNumber :value="tradeStats.count" /></div>
        </div>
        <div>
          <div class="label-micro">今日</div>
          <div class="stat-num mt-1 text-[19px] leading-none"><RollingNumber :value="tradeStats.today" /></div>
        </div>
        <div>
          <div class="label-micro">成交额</div>
          <div class="stat-num mt-1 text-[15px] leading-none"><RollingNumber :value="money(tradeStats.volume)" /></div>
        </div>
        <div>
          <div class="label-micro">均笔</div>
          <div class="stat-num mt-1 text-[15px] leading-none"><RollingNumber :value="money(tradeStats.avg)" /></div>
        </div>
      </div>
      <div class="mt-3.5 flex items-center justify-between border-t border-line pt-2.5 text-[11.5px]">
        <span class="text-faint-fg">今日净利</span>
        <span class="stat-num" :class="tradeStats.todayNet >= 0 ? 'text-up' : 'text-down'">
          <RollingNumber :value="signedMoney(tradeStats.todayNet)" />
        </span>
      </div>
    </Card>

    <Card>
      <CardHeader label="今日表现" />
      <StatRow label="最佳单笔" :value="signedPct(cumStats.best)" tone="up" />
      <StatRow label="最差单笔" :value="signedPct(cumStats.worst)" tone="down" />
      <StatRow label="持仓浮动" :value="signedPct(positionUnrealized)" :tone="positionUnrealized >= 0 ? 'up' : 'down'" />
      <StatRow label="平均持仓" :value="cumStats.avgHold ? duration(cumStats.avgHold) : '—'" tone="dim" />
      <!-- Words, not figures: DRY / LIVE never rolls, so it stays plain text. -->
      <StatRow label="运行模式" :value="mode.toUpperCase()" :tone="isDry ? 'gold' : 'up'" :roll="false" />
      <StatRow label="行情轮次" :value="roundMarkets ?? '—'" tone="dim" />
      <div class="mt-2.5 border-t border-line pt-2.5">
        <!--
          Headline is 本金 ＋ 净利润 (the real balance). No cash-ledger row
          underneath in DRY: the core's ledger is 本金 ＋ 本次会话净利, a strict
          subset of what the history tab below already shows.
        -->
        <StatRow
          :label="balanceLabel"
          :value="recon.equity !== null ? money(recon.equity) : money(recon.cash)"
          :tone="isDry ? 'gold' : 'default'"
        />
        <p v-if="recon.equity !== null" class="mt-1 text-[10px] leading-snug text-faint-fg">
          本金 {{ money(recon.seed) }} ＋ 已实现净利 {{ signedMoney(recon.net) }}
          <Tooltip :content="`真实余额 = 本金 ${money(recon.seed)} ＋ 扣费后净利 ${signedMoney(recon.net)}。手续费是成本：毛利 ${money(recon.gross)} − 手续费 ${money(recon.fees)} = 净利。手续费不改变本金，因此不并入余额。${balanceAccountNote}`">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              手续费支出 {{ money(recon.fees) }}
            </span>
          </Tooltip>
        </p>
        <!--
          No principal on the wire: either LIVE, or a core older than the field
          that reports it. Show the cash ledger and say the principal is
          missing instead of printing an equation the panel cannot verify.
        -->
        <p v-else-if="isDry" class="mt-1 text-[10px] leading-snug text-faint-fg">
          <Tooltip content="内核未上报本金，无法计算「本金 ＋ 净利」的真实余额，此处显示内核现金账。本金由内核按 --seed-balance 持有；当前运行中的内核早于该字段，重启内核后即可显示真实余额。">
            <span class="cursor-help underline decoration-dotted decoration-line underline-offset-2">
              内核现金账 · 本金未上报
            </span>
          </Tooltip>
        </p>
      </div>
    </Card>
  </div>
</template>
