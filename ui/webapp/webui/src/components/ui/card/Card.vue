<script setup lang="ts">
import { cn } from '@/lib/utils'
import { computed, useAttrs } from 'vue'

defineOptions({ inheritAttrs: false })
const attrs = useAttrs()
// `dense` drops the blur for data-heavy cards (tables, audit trails).
const props = withDefaults(defineProps<{ dense?: boolean; solid?: boolean; class?: string }>(), {})
const base = computed(() => (props.dense || props.solid ? 'glass-dense' : 'glass'))
</script>

<template>
  <section v-bind="attrs" :class="cn(base, 'card-pad', props.class)">
    <slot />
  </section>
</template>
