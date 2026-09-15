<script setup lang="ts">
import { onUnmounted, ref, computed } from 'vue'
import { useIntervalFn } from '@vueuse/core'
import { usePanelStore } from './stores/panel'
import { hasToken } from './api/client'
import OverviewPage from './pages/Overview.vue'
import StrategiesPage from './pages/Strategies.vue'
import PluginsPage from './pages/Plugins.vue'

const store = usePanelStore()
const tokenInput = ref('')
const authed = ref(hasToken())
const tab = ref<'overview' | 'strategies' | 'plugins'>('overview')

const pages = { overview: OverviewPage, strategies: StrategiesPage, plugins: PluginsPage } as const
const activePage = computed(() => pages[tab.value])

const tabs = [
  { id: 'overview', label: '总览' },
  { id: 'strategies', label: '策略' },
  { id: 'plugins', label: '插件' },
] as const

function submitToken(): void {
  store.applyToken(tokenInput.value)
  // 401 from connect would surface as store.error; success flips the shell.
  void store.refresh().then(() => {
    if (!store.error) authed.value = true
    else if (hasToken()) {
      // bad token: clear so the prompt stays up with the error visible
      store.applyToken('')
    }
  })
}

// 15s auto-refresh; tab switch or token submit also triggers a refresh.
const { pause: stopPoll } = useIntervalFn(() => { void store.refresh() }, 15_000)
onUnmounted(() => { stopPoll() })

if (authed.value) void store.refresh()
</script>

<template>
  <template v-if="authed">
    <header class="topbar glass">
      <div class="brand">
        <span class="brand-dot" :class="store.connected ? 'dot-ok' : 'dot-bad'"></span>
        <h1>闪电战机器人</h1>
      </div>
      <nav class="tabs">
        <button
          v-for="t in tabs"
          :key="t.id"
          class="tab"
          :class="{ active: tab === t.id }"
          @click="tab = t.id"
        >{{ t.label }}</button>
      </nav>
      <span class="sub" style="margin-left: auto">
        {{ store.connected ? '引擎已连接' : '引擎未连接' }}
        <template v-if="store.lastUpdated"> · 更新于 {{ new Date(store.lastUpdated).toLocaleTimeString() }}</template>
      </span>
    </header>

    <main class="page">
      <div v-if="store.error" class="error-banner">{{ store.error }}</div>
      <component :is="activePage" />
    </main>
  </template>

  <template v-else>
    <div class="token-bar glass">
      <div style="width: 100%">
        <h1 style="text-align: center; margin-bottom: 6px">接入控制面板</h1>
        <p class="sub" style="text-align: center; margin: 0 0 16px">
          运行网关后，把启动时打印的一次性 token 粘贴到下面。
        </p>
        <div style="display: flex; gap: 10px">
          <input
            v-model="tokenInput"
            class="token-input"
            placeholder="40 位十六进制 token"
            @keyup.enter="submitToken"
          />
          <button class="btn gold" @click="submitToken">连 接</button>
        </div>
        <p v-if="store.error" class="sub" style="color: var(--bk-red); text-align: center; margin-top: 10px">
          {{ store.error }}
        </p>
      </div>
    </div>
  </template>
</template>

<style>
.tab { font-family: inherit; }
</style>
