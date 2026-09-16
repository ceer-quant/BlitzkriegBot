<script setup lang="ts">
import { SwitchRoot, SwitchThumb } from 'reka-ui'
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{ modelValue?: boolean; disabled?: boolean; class?: string; label?: string }>(),
  {},
)
const emit = defineEmits<{ 'update:modelValue': [boolean] }>()
</script>

<template>
  <label :class="cn('inline-flex items-center gap-2 shrink-0', !props.disabled && 'cursor-pointer')">
    <SwitchRoot
      :model-value="props.modelValue"
      :disabled="props.disabled"
      :class="cn(
        'relative inline-flex h-[22px] w-[38px] shrink-0 items-center rounded-full border transition-colors duration-200',
        'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary/40',
        'disabled:opacity-45',
        props.modelValue
          ? 'border-primary/35 bg-primary/85'
          : 'border-line bg-panel-2 hover:border-line-strong',
        props.class,
      )"
      @update:model-value="emit('update:modelValue', $event)"
    >
      <SwitchThumb
        :class="cn(
          'block size-[17px] rounded-full bg-white shadow-[0_1px_3px_rgba(0,0,0,.35)]',
          'transition-transform duration-200 ease-[var(--ease-spring)]',
          'data-[state=checked]:translate-x-[18px] data-[state=unchecked]:translate-x-[2px]',
        )"
      />
    </SwitchRoot>
    <span v-if="props.label" class="text-[12.5px] font-medium text-muted-fg select-none">{{ props.label }}</span>
  </label>
</template>
