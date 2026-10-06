<script setup lang="ts">
import { cn } from '@/lib/utils'

// Card section header: optional identity icon + micro label + right-side slot
// for controls. issue 393 (②): the title row reads 「icon + 文字」 — the identity
// icon leads, the label follows — and because every card in the panel composes
// THIS component, the order is fixed in one place instead of per page.
defineProps<{ label?: string; class?: string }>()
</script>

<template>
  <header :class="cn('flex items-center justify-between gap-3 mb-3.5', $props.class)">
    <div class="flex items-center gap-2 min-w-0">
      <slot name="title" />
      <h2 v-if="label" class="label-micro">{{ label }}</h2>
    </div>
    <div class="flex items-center gap-2 shrink-0">
      <slot name="action" />
    </div>
  </header>
</template>
