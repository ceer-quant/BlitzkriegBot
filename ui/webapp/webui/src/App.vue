<script setup lang="ts">
/**
 * Panel shell — top bar (brand + segmented nav + connection telemetry), page
 * outlet, and the global error surface. Dark is the default identity.
 */
import { computed, onUnmounted, ref } from 'vue'
import { useIntervalFn, useNow } from '@vueuse/core'
import { LayoutDashboard, Activity, History, Boxes, Puzzle, Moon, Sun, Bell, BellOff, LogOut, Radio } from 'lucide-vue-next'
import { usePanelStore } from './stores/panel'
import { hasToken, getToken, logout } from './api/client'
import { useTheme } from './lib/theme'
import { clockTime } from './lib/format'
import OverviewPage from './pages/Overview.vue'
import HftPage from './pages/HftPage.vue'
import BacktestPage from './pages/BacktestPage.vue'
import StrategiesPage from './pages/Strategies.vue'
import PluginsPage from './pages/Plugins.vue'
import LoginView from './components/LoginView.vue'
import SegmentedControl from './components/ui/segmented/SegmentedControl.vue'
import Button from './components/ui/button/Button.vue'
import AlertBanner from './components/ui/alert/AlertBanner.vue'

type TabId = 'overview' | 'hft' | 'backtest' | 'strategies' | 'plugins'

const store = usePanelStore()
const { isDark, sound, toggleTheme, toggleSound } = useTheme()
const authed = ref(hasToken() && !!getToken())
const tab = ref<TabId>('overview')

const pages = {
  overview: OverviewPage,
  hft: HftPage,
  backtest: BacktestPage,
  strategies: StrategiesPage,
  plugins: PluginsPage,
} as const
const activePage = computed(() => pages[tab.value])

const segments = [
  { id: 'overview', label: '总览', icon: LayoutDashboard },
  { id: 'hft', label: '行情面板', icon: Activity },
  { id: 'backtest', label: '回放复盘', icon: History },
  { id: 'strategies', label: '策略', icon: Boxes },
  { id: 'plugins', label: '插件', icon: Puzzle },
]

const now = useNow({ interval: 1000 })
const updatedAt = computed(() => (store.lastUpdated ? clockTime(store.lastUpdated) : '—'))
const modeLabel = computed(() => (store.snapshot?.mode ?? '').toUpperCase())

async function onLogin(): Promise<void> {
  authed.value = true
  await store.refresh()
}

// 15s auto-refresh; pages that need faster pacing run their own tick.
const { pause: stopPoll } = useIntervalFn(() => { void store.refresh() }, 15_000)
onUnmounted(() => { stopPoll() })

if (authed.value) void store.refresh()
</script>

<template>
  <template v-if="authed">
    <header
      class="glass sticky top-3 z-20 mx-auto flex max-w-[1280px] items-center gap-4 px-4 py-2.5"
      style="width: calc(100% - 32px)"
    >
      <!-- brand -->
      <div class="flex shrink-0 items-center gap-2.5">
        <span class="relative grid size-8 place-items-center rounded-[10px] btn-gold">
          <svg class="size-4" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
            <path d="M13 2 4.5 13.5H11L9.5 22 19 10h-6.5z" />
          </svg>
        </span>
        <div class="leading-tight">
          <h1 class="text-[15px] font-bold tracking-[-0.01em]">闪电战机器人</h1>
          <p class="label-micro" style="letter-spacing: 0.14em">BLITZKRIEG</p>
        </div>
      </div>

      <!-- nav -->
      <nav class="mx-auto hidden shrink-0 md:block">
        <SegmentedControl v-model="tab" :segments="segments" />
      </nav>

      <!-- telemetry -->
      <div class="ml-auto flex shrink-0 items-center gap-2">
        <span class="hidden items-center gap-2 rounded-full border border-line bg-panel-2 px-2.5 py-1 lg:flex">
          <span
            class="pulse-dot size-1.5 rounded-full"
            :style="{ background: store.connected ? 'var(--up)' : 'var(--down)' }"
          />
          <span class="text-[11.5px] font-semibold" :class="store.connected ? 'text-up' : 'text-down'">
            {{ store.connected ? '在线' : '离线' }}
          </span>
          <span class="text-[11.5px] text-faint-fg num">{{ modeLabel }}</span>
        </span>
        <span class="hidden items-center gap-1.5 text-[11.5px] text-faint-fg xl:flex">
          <Radio class="size-3.5" />
          <span class="num">{{ clockTime(now.getTime()) }}</span>
          <span class="opacity-50">·</span>
          <span>更新 {{ updatedAt }}</span>
        </span>

        <Button variant="ghost" size="icon-sm" :title="sound ? '关闭提示音' : '开启提示音'" @click="toggleSound">
          <Bell v-if="sound" />
          <BellOff v-else class="opacity-60" />
        </Button>
        <Button variant="ghost" size="icon-sm" :title="isDark ? '切换浅色主题' : '切换深色主题'" @click="toggleTheme">
          <Sun v-if="isDark" />
          <Moon v-else />
        </Button>
        <Button variant="ghost" size="icon-sm" title="退出登录" @click="logout">
          <LogOut />
        </Button>
      </div>
    </header>

    <!-- compact nav for narrow viewports -->
    <nav class="mx-auto mt-3 flex max-w-[1280px] justify-center px-4 md:hidden">
      <SegmentedControl v-model="tab" :segments="segments" size="sm" />
    </nav>

    <main class="mx-auto max-w-[1280px] px-4 pt-3.5 pb-20">
      <div v-if="store.error" class="mb-3.5">
        <AlertBanner title="网关连接异常">{{ store.error }}</AlertBanner>
      </div>
      <component :is="activePage" />
    </main>
  </template>

  <LoginView v-else @ok="onLogin" />
</template>
