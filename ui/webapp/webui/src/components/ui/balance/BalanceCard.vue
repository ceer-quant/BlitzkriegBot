<script setup lang="ts">
/**
 * 余额卡片 — 净利/毛利/费用的唯一渲染源。
 *
 * 所有余额相关数字都走账本口径（balance − seededTotal），绝不参杂
 * 成交折叠口径。这是全面板净利卡片的唯一组件定义，页面只传入
 * `balance`（来自 useAccountBalance），不再在页面内各自维护一套 recon。
 *
 * 面板上所有净利数字均由本组件内部的 NetPnLDisplay 统一渲染，
 * 绝不在页面内再写 `signedMoney(net)` + RollingNumber。
 */
import type { BalanceView } from '@/lib/balance'
import { computed } from 'vue'
import { signedMoney, money, pct } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import { ArrowUpRight, ArrowDownRight } from 'lucide-vue-next'
import EquityCurve from '@/components/charts/EquityCurve.vue'
import NetPnLDisplay from '@/components/ui/balance/NetPnLDisplay.vue'
import type { TradeRow } from '@/api/client'

defineProps<{
  /** 账本口径的余额视图（唯一来源） */
  balance: Pick<BalanceView, 'net' | 'fees' | 'gross'>
  /** 卡片标题 */
  label?: string
  /** 已平仓笔数（总览页「已平仓交易」卡片需要） */
  count?: number
  /** 胜笔数（总览页「已平仓交易」卡片需要） */
  win?: number
  /** 负笔数（总览页「已平仓交易」卡片需要） */
  loss?: number
  /** 胜率（总览页「已平仓交易」卡片需要） */
  winRate?: number
  /** 是否展示权益曲线（成交行折叠轨迹，成交口径，仅作参考） */
  showCurve?: boolean
  /** 曲线行（showCurve 为 true 时必传） */
  curveRows?: TradeRow[]
}>()
</script>

<template>
  <Card>
    <CardHeader :label="label">
      <template #title>
        <div class="flex items-baseline gap-2">
          <NetPnLDisplay :net="balance.net" />
          <span v-if="count !== undefined" class="text-[11px] text-faint-fg">
            · {{ count }} 笔
          </span>
        </div>
      </template>
      <template #action>
        <Badge variant="default">账本口径</Badge>
      </template>
    </CardHeader>
    <template v-if="showCurve && curveRows">
      <EquityCurve :rows="curveRows" :height="128" :show-axis="false" />
    </template>
    <div class="mt-2 flex items-center gap-4 text-[11px] text-faint-fg">
      <span>毛利 <span class="num text-fg"><RollingNumber :value="signedMoney(balance.gross)" /></span></span>
      <span>费用 <span class="num text-down"><RollingNumber :value="money(balance.fees)" /></span></span>
    </div>
    <div v-if="count !== undefined && (win !== undefined || loss !== undefined || winRate !== undefined)"
         class="mt-2 flex items-center gap-2 text-[11px] text-faint-fg">
      <Badge v-if="win !== undefined" variant="up">
        <ArrowUpRight class="size-3" />
        <span>{{ win }}</span>
      </Badge>
      <Badge v-if="loss !== undefined" variant="down">
        <ArrowDownRight class="size-3" />
        <span>{{ loss }}</span>
      </Badge>
      <span v-if="winRate !== undefined" class="num">胜率 {{ pct(winRate) }}</span>
    </div>
  </Card>
</template>
