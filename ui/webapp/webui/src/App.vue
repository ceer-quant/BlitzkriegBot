<script setup lang="ts">
/**
 * Panel shell — top bar (brand + segmented nav + connection telemetry), page
 * outlet, and the global error surface. Dark is the default identity.
 *
 * issue 381 IA 重构（审核修订版）：主导航 6 个平铺短标签 —— 总览/行情/回测/
 * 策略/插件/设置。唯一的组「策略」被选中时，组内叶子（策略/蓝图/裁决流/进化）
 * 作为页内 tab 呈现（SettingsPage 的域切换模式），导航条上没有二级控件。
 * 导航本身是纯数据（lib/nav.ts），门禁与页表同源；组只是导航归拢，页签集合
 * 不变。
 */
import { computed, defineAsyncComponent, onMounted, onUnmounted, ref, watch } from 'vue'
import { Moon, Sun, SunMoon, Bell, BellOff, LogOut, Radio } from 'lucide-vue-next'
import { useIntervalFn, useNow } from '@vueuse/core'
import { usePanelStore } from './stores/panel'
import { hasToken, getToken, logout, ping } from './api/client'
import { readEventStream } from './lib/sse'
import { SESSION_EXPIRED_REASON } from './lib/session'
import { useTheme } from './lib/theme'
import { clockTime } from './lib/format'
import { NAV, groupOf, type TabId } from './lib/nav'
import brandMark from './assets/logo.png'
import LoginView from './components/LoginView.vue'
import OnboardingTour from './components/OnboardingTour.vue'
import AccountSwitcher from './components/AccountSwitcher.vue'
import SegmentedControl from './components/ui/segmented/SegmentedControl.vue'
import Button from './components/ui/button/Button.vue'
import AlertBanner from './components/ui/alert/AlertBanner.vue'
import {
  connectionBanner, freezeBanner, lastErrorBanner, selfCheckBanner, archiveStoppedBanner,
  accountFreezeBanners, configGateBanners, overdueReceivableBanner,
} from '@/lib/safety'

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
// 首屏前内核尚未可达：壳与骨架立即渲染，authed 只门禁数据区；连接期间显示
// 「正在连接内核…」，而非全页空白。
const connecting = computed(() => authed.value && !store.connected)
const tab = ref<TabId>('overview')

/**
 * Route pages load on demand (item 781): the shell no longer ships every page
 * in the entry chunk, so first paint only pays for the shell. Each loader is a
 * static `import()` specifier — Vite emits one chunk per page and preloads it
 * when the tab is opened.
 */
const pages = {
  overview: defineAsyncComponent(() => import('./pages/Overview.vue')),
  hft: defineAsyncComponent(() => import('./pages/HftPage.vue')),
  backtest: defineAsyncComponent(() => import('./pages/BacktestPage.vue')),
  blueprint: defineAsyncComponent(() => import('./pages/BlueprintPage.vue')),
  strategies: defineAsyncComponent(() => import('./pages/Strategies.vue')),
  decisions: defineAsyncComponent(() => import('./pages/Decisions.vue')),
  evolution: defineAsyncComponent(() => import('./pages/EvolutionPage.vue')),
  plugins: defineAsyncComponent(() => import('./pages/Plugins.vue')),
  settings: defineAsyncComponent(() => import('./pages/SettingsPage.vue')),
} as const
const activePage = computed(() => pages[tab.value])

/** 主导航：5 个直连页 + 1 个组（策略）。组被点中时落组内第一个叶子。 */
const topSegments = NAV.map((e) => ({ id: e.id as string, label: e.label }))
const GROUP_IDS = new Set<string>(NAV.filter((e) => e.kind === 'group').map((e) => e.id))
const activeGroup = computed(() => groupOf(tab.value))
/** 组不是页：v-model 落到组 id 时换成组内第一个叶子页。 */
function onTopNav(id: string): void {
  if (GROUP_IDS.has(id)) {
    const g = NAV.find((e) => e.id === id)
    if (g && g.kind === 'group') tab.value = g.children[0].id
    return
  }
  tab.value = id as TabId
}

const now = useNow({ interval: 1000 })
const updatedAt = computed(() => (store.lastUpdated ? clockTime(store.lastUpdated) : '—'))
const modeLabel = computed(() => (store.snapshot?.mode ?? '').toUpperCase())

// ── trading-safety banners (顶部，任何页面均可见) ──────────────────────────
// 冻结 / 最近拒绝 / 自检失败 / 归档停录 / 应收账款超龄 — 读统一的 reading 规则
//（lib/safety.ts），这里只负责渲染与去重 dismiss。
const OVERDUE_RECEIVABLE_MAX_AGE_MS = 172_800_000
const stats = computed(() => store.snapshot?.stats ?? null)
const connBanner = computed(() => connectionBanner(store.snapshot))
const freeze = computed(() => freezeBanner(stats.value))
	// 风控冻结（issue 943 / issue 1044）：唯一来源是内核 freeze book stats.accountFreezes。
		// 配态停用（account.status === 'frozen'）与风控冻结是两回事，不得混入同一 red
		// banner；配态停用在设置页单独呈现为「配置停用」，不提供面板解冻。
		const accountFreezes = computed(() => accountFreezeBanners(stats.value))
		// 配置态闸门（read_only / suspended / trading disabled）：由账户配置派生，
		// 不提供面板解冻，呈现为可关闭的 amber 提示，不钉死在 cockpit。
		const configGates = computed(() => configGateBanners(stats.value))
	const lastError = computed(
	  () => lastErrorBanner(stats.value, freeze.value !== null || accountFreezes.value.length > 0),
	)
const selfCheck = computed(() => selfCheckBanner(stats.value))
const archiveStopped = computed(() => archiveStoppedBanner(stats.value))
const overdueReceivable = computed(() =>
  overdueReceivableBanner(stats.value, OVERDUE_RECEIVABLE_MAX_AGE_MS),
)

// Dismiss memory lives in sessionStorage, scoped to the tab session.
const AUTO_DISMISS_MS = 40_000
const DISMISS_KEY = 'overview.dismissed-banners'
const loadDismissed = (): Set<string> => {
  try {
    const raw = sessionStorage.getItem(DISMISS_KEY)
    return new Set(raw ? (JSON.parse(raw) as string[]) : [])
  } catch { return new Set() }
}
const dismissed = ref(loadDismissed())
const dismiss = (identity: string) => {
  const next = new Set(dismissed.value)
  next.add(identity)
  dismissed.value = next
  try { sessionStorage.setItem(DISMISS_KEY, JSON.stringify([...next])) } catch { /* private mode */ }
}
const bannerAutoKey = (identity?: string) => identity ?? ''
const lastErrorVisible = computed(
  () => lastError.value && !(lastError.value.identity && dismissed.value.has(lastError.value.identity)),
)
const connBannerVisible = computed(
  () => connBanner.value && !(connBanner.value.identity && dismissed.value.has(connBanner.value.identity)),
)
const archiveStoppedVisible = computed(
  () => archiveStopped.value && !(archiveStopped.value.identity && dismissed.value.has(archiveStopped.value.identity)),
)
const overdueReceivableVisible = computed(
  () => overdueReceivable.value && !(overdueReceivable.value.identity && dismissed.value.has(overdueReceivable.value.identity)),
)

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

/**
 * issue 393: cross-page entries (the strategy page's 创建策略 / 编辑策略) ask the
 * shell through the store's one-slot nav request. The tab ids come from
 * lib/nav.ts either way — pages never switch pages themselves, they file a
 * request and the shell (the only owner of `tab`) consumes it.
 */
watch(() => store.navRequest, (req) => {
  if (!req) return
  tab.value = req.tab as TabId
  store.clearNavRequest()
})

// 15s auto-refresh; pages that need faster pacing run their own tick.
const { pause: stopPoll } = useIntervalFn(() => { void store.refresh() }, 15_000)
onUnmounted(() => { stopPoll() })

/**
 * The SSE change stream (issue 838) rides BESIDE the poll loop, never instead
 * of it. Frames carry the exact /api/snapshot document, so each delivery is a
 * freshness upgrade (event-shaped readings — banners, order terminal states —
 * land within the server's 1s tick instead of the next poll); when the
 * stream dies the loop below is untouched and the panel stays informed. The
 * reconnect only fires after a stream that actually delivered something
 * (a refused stream means the session/gateway needs polling's own alert
 * path, not a hot loop of failed handshakes). `getToken()` per attempt: a
 * re-login must feed the NEXT reconnect, not a captured stale token.
 */
const sseRunning = ref(false)
watch(authed, (on) => {
  if (!on) { sseRunning.value = false; return }
  if (sseRunning.value) return
  sseRunning.value = true
  void (async () => {
    while (sseRunning.value && hasToken()) {
      const delivered = await readEventStream(getToken(), (frame) => {
        if (frame.kind === 'snapshot') store.applySseSnapshot(frame.doc)
      })
      if (!delivered) break // never delivered: polling owns this session
      await new Promise((r) => setTimeout(r, 3000)) // brief, bounded backoff
    }
    sseRunning.value = false
  })()
})
onUnmounted(() => { sseRunning.value = false })

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
      class="glass-header sticky top-3 z-20 mx-auto flex max-w-[1280px] items-center justify-between gap-2 px-2.5 py-2 sm:gap-4 sm:px-4 sm:py-2.5"
      style="width: calc(100% - 16px); max-width: 1280px;"
    >
      <!-- brand -->
      <div class="flex shrink-0 items-center gap-2 sm:gap-2.5">
        <img
          :src="brandMark"
          alt=""
          aria-hidden="true"
          class="size-8 shrink-0 select-none drop-shadow-[0_2px_6px_rgba(0,0,0,0.35)] sm:size-9"
          draggable="false"
        >
        <div class="leading-tight">
          <h1 class="max-w-[90px] truncate text-[13px] font-bold tracking-[-0.01em] xs:max-w-none sm:text-[15px]">闪电战机器人</h1>
          <p class="label-micro hidden xs:block" style="letter-spacing: 0.14em">BLITZKRIEG</p>
        </div>
      </div>

      <!-- nav: 6 个平铺短标签（组的叶子在页内 tab 上，导航条无二级控件）-->
      <nav class="mx-auto hidden shrink-0 items-center gap-2 md:flex">
        <SegmentedControl
          :model-value="activeGroup?.id ?? tab"
          :segments="topSegments"
          @update:model-value="onTopNav"
        />
      </nav>

      <!-- telemetry -->
      <div class="ml-auto flex shrink-0 items-center gap-1 sm:gap-2">
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
        <!-- E28 多账户：切换的是本面板连接的默认账户（§9.5 本会话）。成功后
             AccountSwitcher 自己 store.refresh()，快照/插件读数随下一次轮询
             全量重拉，无需各页另挂监听。 -->
        <AccountSwitcher />
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

    <!-- compact nav for narrow viewports with smooth horizontal scroll and touch padding -->
    <nav class="mx-auto mt-6 flex max-w-[1280px] justify-start overflow-x-auto px-4 no-scrollbar sm:mt-6 sm:justify-center sm:px-4 md:hidden">
      <SegmentedControl
        :model-value="activeGroup?.id ?? tab"
        :segments="topSegments"
        size="sm"
        class="min-w-max"
        @update:model-value="onTopNav"
      />
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
      <!-- 首屏骨架：首次快照到达前不留白，数据区显示连接中 -->
      <div v-if="connecting" class="flex flex-col items-center justify-center min-h-[320px] pt-20">
        <p class="text-sm text-faint-fg">正在连接内核…</p>
      </div>
      <div v-else>
        <div v-if="store.error" class="mb-3.5">
        <AlertBanner
          title="网关连接异常"
          hint="检查网关进程与端口；网关恢复后本面板会自动重连，无需刷新页面。"
        >{{ store.error }}</AlertBanner>
      </div>

	      <!-- trading-safety banners — 顶部，任何页面均可见：红色紧急（冻结/自检）
	           钉住、无 ✕、不自动消失；琥珀（连接/归档/应收账款/其余拒单，含 RISK_REJECTED
	           等瞬时拒单）可关 + 40s 自动消失，不出冻结文案、不指路解冻 -->
      <div v-if="connBanner && connBannerVisible" class="mb-3.5">
        <AlertBanner
          tone="warn"
          :title="connBanner.title"
          :hint="connBanner.hint"
          dismissible
          :auto-dismiss-ms="AUTO_DISMISS_MS"
          :auto-key="bannerAutoKey(connBanner.identity)"
          @dismiss="connBanner.identity && dismiss(connBanner.identity)"
        >{{ connBanner.body }}</AlertBanner>
      </div>
      <div v-if="freeze" class="mb-3.5">
        <!-- 紧急：交易此刻停了 —— 红色、钉住、无 ✕（藏掉唯一的红 = 藏掉事故） -->
        <AlertBanner tone="error" :title="freeze.title" :hint="freeze.hint">{{ freeze.body }}</AlertBanner>
      </div>
      <div v-else-if="lastError && lastErrorVisible" class="mb-3.5">
        <!-- 仅 accountGates[acct].blocked 的账户才出冻结红横幅（accountFreeze
             循环下方）；RISK_REJECTED 等瞬时拒单在此处走普通琥珀警告，不出
             冻结文案、不指路解冻。 -->
        <AlertBanner
          :tone="lastError.tone"
          :title="lastError.title"
          :hint="lastError.hint"
          :dismissible="!lastError.urgent"
          :auto-dismiss-ms="!lastError.urgent ? AUTO_DISMISS_MS : 0"
          :auto-key="bannerAutoKey(lastError.identity)"
          @dismiss="lastError.identity && dismiss(lastError.identity)"
        >
          <code v-if="lastError.code" class="mr-1.5 font-semibold">{{ lastError.code }}</code>{{ lastError.body }}
        </AlertBanner>
      </div>
      <div v-for="af in accountFreezes" :key="`account-freeze-${af.title}`" class="mb-3.5">
        <!-- 紧急：单账户冻结不设全局 kill，藏掉它就是把事故说成正常。每冻结户一条红横幅 -->
        <AlertBanner tone="error" :title="af.title" :hint="af.hint">{{ af.body }}</AlertBanner>
      </div>
      <div v-for="cg in configGates" :key="cg.identity" class="mb-3.5">
        <!-- 配置态闸门：amber、可关、自动消失，无解冻门；改账户配置并重启即可恢复 -->
        <AlertBanner tone="warn" :title="cg.title" :hint="cg.hint" dismissible :auto-dismiss-ms="AUTO_DISMISS_MS"
          :auto-key="`config-${cg.identity}`" @dismiss="cg.identity && dismiss(cg.identity)"
        >
          {{ cg.body }}
        </AlertBanner>
      </div>
      <div v-if="selfCheck" class="mb-3.5">
        <!-- 紧急：自检失败伴随冻结面 —— 同冻结一样钉住 -->
        <AlertBanner tone="error" :title="selfCheck.title">
          {{ selfCheck.body }}
          <span v-for="item in selfCheck.items" :key="item.name" class="block">
            {{ item.name }}：{{ item.detail }}
          </span>
        </AlertBanner>
      </div>
      <div v-if="archiveStopped && archiveStoppedVisible" class="mb-3.5">
        <AlertBanner
          tone="warn"
          :title="archiveStopped.title"
          :hint="archiveStopped.hint"
          dismissible
          :auto-dismiss-ms="AUTO_DISMISS_MS"
          :auto-key="bannerAutoKey(archiveStopped.identity)"
          @dismiss="archiveStopped.identity && dismiss(archiveStopped.identity)"
        >{{ archiveStopped.body }}</AlertBanner>
      </div>
      <div v-if="overdueReceivable && overdueReceivableVisible" class="mb-3.5">
        <AlertBanner
          tone="warn"
          :title="overdueReceivable.title"
          :hint="overdueReceivable.hint"
          dismissible
          :auto-dismiss-ms="AUTO_DISMISS_MS"
          :auto-key="bannerAutoKey(overdueReceivable.identity)"
          @dismiss="overdueReceivable.identity && dismiss(overdueReceivable.identity)"
        >{{ overdueReceivable.body }}</AlertBanner>
      </div>

      <!-- 组内叶子是页内 tab（SettingsPage 的域切换模式）：内容上方一排页签，
           不在导航条上出现第二级控件。 -->
      <nav v-if="activeGroup" class="mb-4 px-4 sm:px-0">
        <SegmentedControl v-model="tab" :segments="activeGroup.children" />
      </nav>
      <component :is="activePage" />
      </div>
    </main>

    <OnboardingTour @navigate="goOnboard" />
  </template>

  <LoginView v-else :reason="loginReason" @ok="onLogin" />
</template>
