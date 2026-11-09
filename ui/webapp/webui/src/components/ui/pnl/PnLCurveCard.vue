<script setup lang="ts">
/**
 * 累计净 PnL 卡片（等权益曲线）：Card + CardHeader + EquityCurve 的统一壳，
 * 被 Overview 与 Hft 页共同复用。净利润/笔数/曲线行统一从 panel store
 * 读取（账本口径 = 唯一真相），两个页面不再各自维护一套计算逻辑，
 * 标题统一为「累计净 PnL」。
 */
import { computed } from 'vue'
import { usePanelStore } from '@/stores/panel'
import { useAccountBalance } from '@/lib/account-balance'
import { filterByAccount, filterByVenue } from '@/lib/account-filter'
import { signedMoney } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'
import EquityCurve from '@/components/charts/EquityCurve.vue'

const props = defineProps<{
  /** 曲线高度 */
  height?: number
}>()

const store = usePanelStore()

/** 账本口径净利：balance − seededTotal（操作员裁决 2026-11-08：账本流水=唯一真相） */
const accountBalance = computed(() => {
  const row = store.activeAccountRow
  if (!row) return null
  return useAccountBalance(row)
})

const net = computed(() => accountBalance.value?.net ?? 0)

/** 已平仓笔数 + 权益曲线行：按显示账户 + 显示账户绑定的 venue 过滤，两页保持一致 */
const closedRows = computed(() => {
  if (!store.activeAccount) return []
  return filterByVenue(filterByAccount(store.tradeRows, store.activeAccount).rows, store.activeAccountVenue).rows
})

const count = computed(() => closedRows.value.length)
const curveRows = computed(() => closedRows.value)
</script>

<template>
  <Card>
    <CardHeader label="累计净 PnL">
      <template #title>
        <span class="stat-num text-[15px]" :class="net >= 0 ? 'text-up' : 'text-down'">
          <RollingNumber :value="signedMoney(net)" />
        </span>
      </template>
      <template #action>
        <Badge variant="default" dot><RollingNumber :value="count" /> 笔</Badge>
      </template>
    </CardHeader>
    <EquityCurve :rows="curveRows" :height="height ?? 196" :total="net" />
  </Card>
</template>
