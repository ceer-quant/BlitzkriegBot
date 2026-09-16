<script setup lang="ts">
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{
    modelValue: string
    type?: string
    placeholder?: string
    autocomplete?: string
    size?: 'sm' | 'default'
    class?: string
  }>(),
  { type: 'text', size: 'default' },
)
const emit = defineEmits<{ 'update:modelValue': [string] }>()
</script>

<template>
  <input
    :type="props.type"
    :value="props.modelValue"
    :placeholder="props.placeholder"
    :autocomplete="props.autocomplete"
    :class="cn(
      'w-full rounded-md border border-line bg-panel-2 text-fg placeholder:text-faint-fg',
      'transition-[border-color,box-shadow] duration-150 outline-none',
      'focus:border-primary/55 focus:shadow-[0_0_0_3px_oklch(0.78_0.16_68_/_0.14)]',
      props.size === 'sm' ? 'h-8 px-2.5 text-[12.5px]' : 'h-9.5 px-3.5 text-[13.5px]',
      props.class,
    )"
    @input="emit('update:modelValue', ($event.target as HTMLInputElement).value)"
  >
</template>
