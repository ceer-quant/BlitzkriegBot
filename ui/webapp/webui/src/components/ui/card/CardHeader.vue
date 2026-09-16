<script setup lang="ts">
import { cn } from '@/lib/utils'

// Card section header: micro label + optional right-side slot for controls.
defineProps<{ label?: string; class?: string }>()
</script>

<template>
  <header :class="cn('flex items-center justify-between gap-3 mb-3.5', $props.class)">
    <div class="flex items-baseline gap-2 min-w-0">
      <h2 v-if="label" class="label-micro">{{ label }}</h2>
      <slot name="title" />
    </div>
    <div class="flex items-center gap-2 shrink-0">
      <slot name="action" />
    </div>
  </header>
</template>
