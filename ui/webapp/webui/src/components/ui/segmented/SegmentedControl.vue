<script setup lang="ts">
/**
 * Segmented control (Apple-style) — a sliding gold pill behind the active
 * segment. Keyboard-navigable; the indicator moves with a spring ease.
 */
import { computed } from 'vue'
import { cn } from '@/lib/utils'

export interface Segment {
  id: string
  label: string
  badge?: string | number
}

const props = withDefaults(
  defineProps<{
    modelValue: string
    segments: Segment[]
    size?: 'sm' | 'default'
    class?: string
  }>(),
  { size: 'default' },
)
const emit = defineEmits<{ 'update:modelValue': [string] }>()

const activeIndex = computed(() => Math.max(0, props.segments.findIndex((s) => s.id === props.modelValue)))
const count = computed(() => Math.max(1, props.segments.length))
</script>

<template>
  <div
    :class="cn(
      'relative inline-grid items-center gap-0.5 rounded-full border border-line bg-panel-2 p-1 backdrop-blur-md',
      props.class,
    )"
    :style="{ gridTemplateColumns: `repeat(${count}, minmax(0, 1fr))` }"
    role="tablist"
  >
    <!-- sliding indicator -->
    <span
      class="pointer-events-none absolute top-1 bottom-1 rounded-full btn-gold transition-[left] duration-300 ease-[var(--ease-spring)]"
      :style="{
        left: `calc(${activeIndex} * (100% - 8px) / ${count} + 4px)`,
        width: `calc((100% - 8px) / ${count})`,
      }"
    />
    <button
      v-for="s in props.segments"
      :key="s.id"
      type="button"
      role="tab"
      :aria-selected="s.id === props.modelValue"
      :class="cn(
        'relative z-10 inline-flex items-center justify-center gap-1.5 rounded-full font-semibold whitespace-nowrap',
        'transition-colors duration-200 focus-visible:outline-none',
        props.size === 'sm' ? 'h-6.5 px-3 text-[11.5px]' : 'h-7.5 px-4 text-[12.5px]',
        s.id === props.modelValue ? 'text-primary-ink' : 'text-muted-fg hover:text-fg',
      )"
      @click="emit('update:modelValue', s.id)"
    >
      {{ s.label }}
      <span
        v-if="s.badge !== undefined"
        :class="cn(
          'rounded-full px-1.5 py-px text-[10px] font-bold tabular-nums',
          s.id === props.modelValue ? 'bg-primary-ink/18 text-primary-ink' : 'bg-line text-faint-fg',
        )"
      >{{ s.badge }}</span>
    </button>
  </div>
</template>
