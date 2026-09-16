<script setup lang="ts">
/** KPI tile: micro label, big tabular number, optional delta + sparkline slot. */
import { cn } from '@/lib/utils'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'

const props = withDefaults(
  defineProps<{
    label: string
    value: string | number
    sub?: string
    tone?: 'default' | 'up' | 'down' | 'gold'
    icon?: boolean
    class?: string
    /** Set false to print the value flat — for text that is not a number. */
    roll?: boolean
    /** Roll duration in ms. */
    rollDuration?: number
  }>(),
  { tone: 'default', roll: true, rollDuration: 420 },
)

const toneClass = {
  default: 'text-fg',
  up: 'text-up',
  down: 'text-down',
  gold: 'grad-gold',
}[props.tone]
</script>

<template>
  <div :class="cn('glass glass-hover card-pad relative overflow-hidden', props.class)">
    <div class="flex items-start justify-between gap-2">
      <span class="label-micro">{{ props.label }}</span>
      <slot name="action" />
    </div>
    <!--
      The value rolls unless a slot supplies it (a slot can hold arbitrary markup,
      which cannot be split into digits). `toneClass` stays on this div so a gold
      tile publishes `--grad-text` for the digits to clip; see RollingNumber.
    -->
    <div :class="cn('mt-2 stat-num text-[30px] leading-none', toneClass)">
      <slot name="value">
        <RollingNumber v-if="props.roll" :value="props.value" :duration="props.rollDuration" />
        <template v-else>{{ props.value }}</template>
      </slot>
    </div>
    <div v-if="props.sub || $slots.sub" class="mt-2 text-[11.5px] text-faint-fg truncate">
      <slot name="sub">{{ props.sub }}</slot>
    </div>
    <slot />
  </div>
</template>

<style scoped>
.glass-hover {
  transition: transform 0.22s var(--ease-out-soft), box-shadow 0.22s var(--ease-out-soft),
    border-color 0.22s var(--ease-out-soft);
}
.glass-hover:hover {
  transform: translateY(-2px);
  border-color: var(--line-strong);
  box-shadow: var(--shadow-2), 0 0 0 1px oklch(0.78 0.16 68 / 0.16);
}
</style>
