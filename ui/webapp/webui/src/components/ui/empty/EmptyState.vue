<script setup lang="ts">
/** Empty / loading placeholder that never looks broken. */
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{ text?: string; loading?: boolean; icon?: boolean; class?: string; compact?: boolean }>(),
  { loading: false, icon: true },
)
</script>

<template>
  <div
    :class="cn(
      'flex flex-col items-center justify-center gap-2 text-center text-faint-fg',
      props.compact ? 'py-6' : 'py-12',
      props.class,
    )"
  >
    <span
      v-if="props.loading && props.icon"
      class="size-4 animate-spin rounded-full border-2 border-line-strong border-t-primary"
    />
    <svg
      v-else-if="props.icon"
      class="size-6 opacity-45"
      viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"
      stroke-linecap="round" stroke-linejoin="round"
    >
      <path d="M3 7h18M3 12h18M3 17h11" />
    </svg>
    <p class="text-[12.5px]">{{ props.loading ? '加载中…' : (props.text ?? '暂无数据') }}</p>
    <slot />
  </div>
</template>
