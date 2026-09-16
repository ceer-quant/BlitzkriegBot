<script setup lang="ts">
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{ tone?: 'error' | 'warn' | 'info'; title?: string; class?: string; dismissible?: boolean }>(),
  { tone: 'error' },
)
const emit = defineEmits<{ dismiss: [] }>()

const tones = {
  error: 'border-down/30 bg-down/10 text-down',
  warn: 'border-primary/30 bg-primary/10 text-primary',
  info: 'border-info/30 bg-info/10 text-info',
}[props.tone]
</script>

<template>
  <div
    :class="cn('flex items-start gap-2.5 rounded-md border px-3.5 py-2.5 text-[13px]', tones, props.class)"
    role="alert"
  >
    <svg class="mt-[3px] size-4 shrink-0" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
      <circle cx="12" cy="12" r="9" /><path d="M12 8v5M12 16.5v.01" />
    </svg>
    <div class="min-w-0 flex-1">
      <p v-if="props.title" class="font-semibold">{{ props.title }}</p>
      <p class="break-words opacity-90"><slot /></p>
    </div>
    <button
      v-if="props.dismissible"
      class="shrink-0 rounded px-1 text-current/70 transition hover:text-current"
      @click="emit('dismiss')"
    >✕</button>
  </div>
</template>
