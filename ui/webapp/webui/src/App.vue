<script setup lang="ts">
/**
 * Panel shell — top bar (brand + segmented nav + connection telemetry), page
 * outlet, and the global error surface. Dark is the default identity.
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useIntervalFn, useNow } from '@vueuse/core'
import { LayoutDashboard, Activity, History, Boxes, Puzzle, Settings, Moon, Sun, SunMoon, Bell, BellOff, LogOut, Radio, FlaskConical } from 'lucide-vue-next'
import { usePanelStore } from './stores/panel'
import { hasToken, logout, ping } from './api/client'
import { SESSION_EXPIRED_REASON } from './lib/session'
import { useTheme } from './lib/theme'
import { clockTime } from './lib/format'
import brandMark from './assets/logo.png'
import OverviewPage from './pages/Overview.vue'
import HftPage from './pages/HftPage.vue'
import BacktestPage from './pages/BacktestPage.vue'
import StrategiesPage from './pages/Strategies.vue'
import EvolutionPage from './pages/EvolutionPage.vue'
import PluginsPage from './pages/Plugins.vue'
import SettingsPage from './pages/SettingsPage.vue'
import LoginView from './components/LoginView.vue'
import OnboardingTour from './components/OnboardingTour.vue'
import SegmentedControl from './components/ui/segmented/SegmentedControl.vue'
import Button from './components/ui/button/Button.vue'
import AlertBanner from './components/ui/alert/AlertBanner.vue'

type TabId = 'overview' | 'hft' | 'backtest' | 'strategies' | 'evolution' | 'plugins' | 'settings'

const store = usePanelStore()
const { theme, isDark, cycleTheme, sound, toggleSound } = useTheme()

/**
 * One button, three states — the accessible name has to say which one is next,
 * and what is on screen right now, since the icon alone cannot.
 */
const themeTitle = computed(() => {
  const now = theme.value === 'system' ? `跟随系统（当前${isDark.value ? '深色' : '浅色'}）` : isDark.value ? '深色' : '浅色'
  const next = theme.value === 'system' ? '浅色' : theme.value === 'light' ? '深色' : '跟随系统'
  return `主题：${now} · 点击切换到${next}`
})
/**
 * Whether to show the panel or the login form.
 *
 * A stored token gets the benefit of the doubt on first paint — it is usually
 * valid, and bouncing through an empty login form on every reload would be
 * wrong. Trust is provisional: the first 401 clears it (see the watch below),
 * which is the part the panel used to be missing. `hasToken()` only ever told us
 * a string existed, so a token from a previous gateway run left the operator
 * staring at a 网关连接异常 alert with no route back to the form.
 */
const authed = ref(hasToken())
const tab = ref<TabId>('overview')

const pages = {
  overview: OverviewPage,
  hft: HftPage,
  backtest: BacktestPage,
  strategies: StrategiesPage,
  evolution: EvolutionPage,
  plugins: PluginsPage,
  settings: SettingsPage,
} as const
const activePage = computed(() => pages[tab.value])

const segments = [
  { id: 'overview', label: '总览', icon: LayoutDashboard },
  { id: 'hft', label: '行情面板', icon: Activity },
  { id: 'backtest', label: '回放复盘', icon: History },
  { id: 'strategies', label: '策略', icon: Boxes },
  { id: 'evolution', label: '进化', icon: FlaskConical },
  { id: 'plugins', label: '插件', icon: Puzzle },
  { id: 'settings', label: '设置', icon: Settings },
]

const now = useNow({ interval: 1000 })
const updatedAt = computed(() => (store.lastUpdated ? clockTime(store.lastUpdated) : '—'))
const modeLabel = computed(() => (store.snapshot?.mode ?? '').toUpperCase())

/**
 * A refused session drops us back to the login form, carrying the reason so the
 * operator knows why they are being asked again. The token is already gone —
 * the store cleared it — so nothing more can be attempted with it.
 */
watch(() => store.sessionExpired, (expired) => {
  if (expired) authed.value = false
})

/**
 * Why the login form is showing, when it is not a cold start. See
 * `lib/session.ts` — the wording lives there so the shell and its check agree.
 */
const loginReason = computed(() => (store.sessionExpired ? SESSION_EXPIRED_REASON : ''))

async function onLogin(): Promise<void> {
  store.acknowledgeSessionReset()
  authed.value = true
  await store.refresh()
}

/** The 首次引导 card asks to be taken to the page where its step gets done. */
function goOnboard(target: 'plugins' | 'strategies'): void {
  tab.value = target
}

// 15s auto-refresh; pages that need faster pacing run their own tick.
const { pause: stopPoll } = useIntervalFn(() => { void store.refresh() }, 15_000)
onUnmounted(() => { stopPoll() })

onMounted(() => {
  if (!authed.value) return
  // Ask the gateway whether it is even up before spending the token. If it does
  // not answer, the failure is a connection problem, not a stale session — and
  // the alert for that is accurate, so the form must not appear.
  void (async () => {
    const alive = await ping()
    if (alive === null) return // unreachable; the alert path is correct
    await store.refresh() // a 401 here flips `sessionExpired`
  })()
})
</script>

<template>
  <template v-if="authed">
    <header
      class="glass-header sticky top-3 z-20 mx-auto flex max-w-[1280px] items-center gap-4 px-4 py-2.5"
      style="width: calc(100% - 32px)"
    >
      <!-- brand -->
      <div class="flex shrink-0 items-center gap-2.5">
        <img
          :src="brandMark"
          alt=""
          aria-hidden="true"
          class="size-9 shrink-0 select-none drop-shadow-[0_2px_6px_rgba(0,0,0,0.35)]"
          draggable="false"
        >
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
        <Button variant="ghost" size="icon-sm" :title="themeTitle" @click="cycleTheme">
          <SunMoon v-if="theme === 'system'" />
          <Sun v-else-if="isDark" />
          <Moon v-else />
        </Button>
        <Button variant="ghost" size="icon-sm" title="退出登录" @click="logout">
          <LogOut />
        </Button>
      </div>
    </header>

    <!-- compact nav for narrow viewports -->
    <nav class="mx-auto mt-6 flex max-w-[1280px] justify-center px-4 md:hidden">
      <SegmentedControl v-model="tab" :segments="segments" size="sm" />
    </nav>

    <!--
      The header floats (sticky top-3) and casts a shadow, so the content needs
      real clearance beneath it rather than the token-thin 14px it used to sit
      at.

      Two gaps, measured on the rendered panel rather than guessed. The nav sits
      directly under the sticky header, and because `top-3` shifts the header
      12px below its flow position, its `mt-4` was a visible 4px — the nav pills
      read as part of the header. `mt-6` restores a real 12px. And on narrow
      screens the compact nav is in the same flow between header and content, so
      the gap that matters there is nav→card: `pt-8` gives 32px of clearance
      instead of 24px. From `md` the nav moves inside the header and `pt-10`
      (40px) clears the sticky offset plus the shadow.
    -->
    <main class="mx-auto max-w-[1280px] px-4 pt-8 pb-20 md:pt-10">
      <div v-if="store.error" class="mb-3.5">
        <AlertBanner
          title="网关连接异常"
          hint="检查网关进程与端口；网关恢复后本面板会自动重连，无需刷新页面。"
        >{{ store.error }}</AlertBanner>
      </div>
      <component :is="activePage" />
    </main>

    <OnboardingTour @navigate="goOnboard" />
  </template>

  <LoginView v-else :reason="loginReason" @ok="onLogin" />
</template>
