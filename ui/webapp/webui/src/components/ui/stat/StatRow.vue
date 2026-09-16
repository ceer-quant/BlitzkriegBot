<script setup lang="ts">
/** Static label/value row — the workhorse of every stat panel. */
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{
    label: string
    value?: string | number
    tone?: 'default' | 'up' | 'down' | 'gold' | 'dim'
    mono?: boolean
    hint?: string
    class?: string
  }>(),
  { tone: 'default', mono: true },
)

const toneClass = {
  default: 'text-fg',
  up: 'text-up',
  down: 'text-down',
  gold: 'text-primary',
  dim: 'text-faint-fg',
}[props.tone]
</script>

<template>
  <div :class="cn('flex items-baseline justify-between gap-3 py-[5px]', props.class)">
    <span class="text-[12px] text-muted-fg shrink-0">{{ props.label }}</span>
    <span class="flex items-baseline gap-1.5 min-w-0">
      <span :class="cn('text-[13px] font-semibold truncate', toneClass, props.mono && 'num')">
        <slot>{{ props.value }}</slot>
      </span>
      <span v-if="props.hint" class="text-[10.5px] text-faint-fg shrink-0">{{ props.hint }}</span>
    </span>
  </div>
</template>
