<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'

const release = __BB_RELEASE__
const visible = ref(release != null)
const root = ref<HTMLElement | null>(null)
const storageKey = release ? `bb-release-dismissed:${release.tag}` : ''

function setOffset(px: number) {
  const rootEl = document.documentElement
  rootEl.style.setProperty('--vp-layout-top-height', `${Math.max(0, Math.round(px))}px`)
  rootEl.classList.toggle('bb-release-open', px > 0)
  rootEl.classList.toggle('bb-release-dismissed', px === 0 && !!storageKey)
}

function measure() {
  if (!visible.value || !root.value) {
    setOffset(0)
    return
  }
  setOffset(root.value.getBoundingClientRect().height)
}

let observer: ResizeObserver | null = null

onMounted(() => {
  if (storageKey) {
    try {
      if (localStorage.getItem(storageKey) === '1') visible.value = false
    } catch {
      /* private mode: keep the banner */
    }
  }
  nextTick(() => {
    measure()
    if (!root.value || typeof ResizeObserver === 'undefined') return
    observer = new ResizeObserver(() => measure())
    observer.observe(root.value)
  })
})

watch(visible, () => nextTick(measure))

onBeforeUnmount(() => {
  observer?.disconnect()
  setOffset(0)
})

function dismiss() {
  visible.value = false
  setOffset(0)
  if (!storageKey) return
  try {
    localStorage.setItem(storageKey, '1')
  } catch {
    /* dismissing still hides it for this view */
  }
}
</script>

<template>
  <div v-if="release && visible" ref="root" class="bb-release" role="region" aria-label="最新版本">
    <p class="bb-release-text">
      <span>BlitzkriegBot v{{ release.version }} 已经发布</span>
      <span v-if="release.changes != null">，超过{{ release.changes }}+改动！</span>
    </p>
    <a class="bb-release-link no-icon" :href="release.url" target="_blank" rel="noreferrer">立即获取 &gt;</a>
    <button class="bb-release-close" type="button" aria-label="关闭版本公告" @click="dismiss">×</button>
  </div>
</template>
