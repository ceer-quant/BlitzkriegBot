<script setup lang="ts">
defineProps<{
  kind: 'changes' | 'pullRequests'
  label: string
}>()

const release = __BB_RELEASE__
</script>

<template>
  <div v-if="release && release[kind] != null" class="bb-stat">
    <b :data-count="release[kind]">{{ Number(release[kind]).toLocaleString('en-US') }}</b>
    <span>{{ label }}</span>
  </div>
</template>
