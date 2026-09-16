<script lang="ts">
import type { HTMLAttributes } from 'vue'
import { buttonVariants, type ButtonVariants } from './index'

export default {
  name: 'UiButton',
}
</script>

<script setup lang="ts">
import { Primitive, type PrimitiveProps } from 'reka-ui'
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{
    variant?: ButtonVariants['variant']
    size?: ButtonVariants['size']
    as?: PrimitiveProps['as']
    asChild?: boolean
    class?: HTMLAttributes['class']
  }>(),
  { as: 'button', variant: 'default', size: 'default' },
)
</script>

<template>
  <Primitive
    :as="props.as"
    :as-child="props.asChild"
    :class="cn(buttonVariants({ variant: props.variant, size: props.size }), props.class)"
  >
    <slot />
  </Primitive>
</template>
