<script setup lang="ts">
import { onUnmounted, ref, computed } from 'vue'
import { useIntervalFn } from '@vueuse/core'
import { usePanelStore } from './stores/panel'
import { hasToken, getToken, logout } from './api/client'
import OverviewPage from './pages/Overview.vue'
import StrategiesPage from './pages/Strategies.vue'
import PluginsPage from './pages/Plugins.vue'
import LoginView from './components/LoginView.vue'

const store = usePanelStore()
const authed = ref(hasToken() && !!getToken())
const tab = ref<'overview' | 'strategies' | 'plugins'>('overview')

const pages = { overview: OverviewPage, strategies: StrategiesPage, plugins: PluginsPage } as const
const activePage = computed(() => pages[tab.value])

const tabs = [
  { id: 'overview', label: '总览' },
  { id: 'strategies', label: '策略' },
  { id: 'plugins', label: '插件' },
] as const

async function onLogin(): Promise<void> {
  authed.value = true
  await store.refresh()
}

// 15s auto-refresh.
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
        <template v-if="store.lastUpdated">
          · 更新于 {{ new Date(store.lastUpdated).toLocaleTimeString() }}
        </template>
        <button class="logout-btn" title="退出登录" @click="logout">退出</button>
      </span>
    </header>

    <main class="page">
      <div v-if="store.error" class="error-banner">{{ store.error }}</div>
      <component :is="activePage" />
    </main>
  </template>

  <LoginView v-else @ok="onLogin" />
</template>

<style>
.logout-btn {
  border: none;
  background: transparent;
  color: var(--bk-text-dim);
  font-size: 12px;
  cursor: pointer;
  margin-left: 12px;
  padding: 4px 10px;
  border-radius: 8px;
  font-family: inherit;
  transition: all 0.15s;
}
.logout-btn:hover {
  color: var(--bk-red);
  background: rgba(229, 72, 77, 0.08);
}
</style>
