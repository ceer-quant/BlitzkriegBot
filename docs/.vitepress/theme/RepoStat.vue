<script setup lang="ts">
defineProps<{
  kind: 'commits' | 'pullRequests'
  label: string
}>()

const counts = __BB_REPO_COUNTS__
</script>

<template>
  <div v-if="counts && counts[kind] != null" class="bb-stat">
    <b :data-count="counts[kind]">{{ Number(counts[kind]).toLocaleString('en-US') }}</b>
    <span>{{ label }}</span>
  </div>
</template>
