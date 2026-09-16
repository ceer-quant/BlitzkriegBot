<script setup lang="ts">
import { cn } from '@/lib/utils'
import { badgeVariants, type BadgeVariants } from './index'

const props = withDefaults(
  defineProps<{ variant?: BadgeVariants['variant']; class?: string; dot?: boolean }>(),
  {},
)
</script>

<template>
  <span :class="cn(badgeVariants({ variant: props.variant }), props.class)">
    <span v-if="props.dot" class="size-[5px] rounded-full bg-current" />
    <slot />
  </span>
</template>
