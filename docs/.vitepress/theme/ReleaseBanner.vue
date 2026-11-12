<script setup lang="ts">
import { onMounted, ref } from 'vue'

const release = __BB_RELEASE__
const visible = ref(release != null)
const storageKey = release ? `bb-release-dismissed:${release.tag}` : ''

onMounted(() => {
  if (!storageKey) return
  try {
    if (localStorage.getItem(storageKey) === '1') visible.value = false
  } catch {
    /* private mode: keep the banner */
  }
})

function dismiss() {
  visible.value = false
  if (!storageKey) return
  try {
    localStorage.setItem(storageKey, '1')
  } catch {
    /* dismissing still hides it for this view */
  }
}
</script>

<template>
  <div v-if="release && visible" class="bb-release" role="region" aria-label="最新版本">
    <p class="bb-release-text">
      <span>BlitzkriegBot {{ release.version }} 已经发布</span>
      <span v-if="release.changes != null">，超过{{ release.changes }}+改动！</span>
    </p>
    <a class="bb-release-link" :href="release.url" target="_blank" rel="noreferrer">立即获取</a>
    <button class="bb-release-close" type="button" aria-label="关闭版本公告" @click="dismiss">×</button>
  </div>
</template>
