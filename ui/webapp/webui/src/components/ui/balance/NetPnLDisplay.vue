<script setup lang="ts">
/**
 * 净利润数字显示的唯一组件。
 *
 * 所有面板上展示「净利」数字的地方都应复用此组件，绝不在页面内
 * 各自写 `signedMoney(net)` + RollingNumber。数值口径统一走账本口径
 *（balance − seededTotal），由调用方传入。
 */
import { signedMoney } from '@/lib/format'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'

defineProps<{
  /** 账本口径的净利（已扣费）。 */
  net: number
}>()
</script>

<template>
  <span class="stat-num text-[16px]" :class="net >= 0 ? 'text-up' : 'text-down'">
    <RollingNumber :value="signedMoney(net)" />
  </span>
</template>
