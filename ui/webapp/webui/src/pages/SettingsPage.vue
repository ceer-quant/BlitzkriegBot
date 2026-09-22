<script setup lang="ts">
/**
 * 设置 — E8-d 指令面 & 收尾。
 *
 * 四块：
 *   1. 会话与访问 token：状态探测（/api/ping 三态）、token 掩码展示、复制、
 *      带 ?token= 的面板链接复制（E6-a 会话交接通道）、过期语义提示。
 *   2. Gateway 指令台：网关身份（socket/--manage/managed/pid/重启次数）、
 *      status/start/stop 指令（沿用 lifecycle 的能力闸，不重造规则）。
 *   3. 网络诊断：内核拨测它自己出网要走的每条路径（net.check），回答「是网络还是
 *      交易所」。读法全在 lib/net-check.ts —— 状态词 → 中文标签、未知状态原样回显、
 *      空报告与「探测中」都不算通过。
 *   4. 外观与节奏：主题三态、提示音、行情快速轮询（设置持久化 store）。
 *
 * 本页只做展示与指令；凭证与下单原语永不出内核。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import {
  Copy, Check, KeyRound, RefreshCw, ShieldCheck, ShieldAlert, ShieldQuestion,
  TerminalSquare, Play, Square, LogOut, Info, Server, Network,
} from 'lucide-vue-next'
import {
  api, getToken, loginAt, logout, ping, probeSession,
  type CommandDoc, type NetCheckDoc,
} from '@/api/client'
import { adjudicateSession } from '@/lib/session'
import { readNetCheck } from '@/lib/net-check'
import { usePanelStore } from '@/stores/panel'
import { useSettingsStore } from '@/stores/settings'
import { useTheme, type ThemeMode } from '@/lib/theme'
import { controlState, exitNotice } from '@/lib/lifecycle'
import { dateTime } from '@/lib/format'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import EmptyState from '@/components/ui/empty/EmptyState.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import RollingNumber from '@/components/ui/roll/RollingNumber.vue'

const store = usePanelStore()
const settings = useSettingsStore()
const { theme, setTheme, sound, setSoundEnabled } = useTheme()

// ── session state: reachability from ping, the token's fate from a real call ─
type SessionState = 'checking' | 'valid' | 'expired' | 'unreachable'
const sessionState = ref<SessionState>('checking')

async function checkSession(): Promise<void> {
  sessionState.value = 'checking'
  // `ping` answers reachability only — it is deliberately sessionless, so its
  // `authRequired` flag describes the GATEWAY, never our token. Mapping that
  // flag to "expired" made this badge claim 会话已过期 forever on any gateway
  // that requires login (re-logging in could not change it: the probe never
  // looked at the token). The token's verdict comes from an authenticated call.
  sessionState.value = await adjudicateSession(await ping(), probeSession)
}
onMounted(checkSession)

const sessionBadge = computed(() => {
  switch (sessionState.value) {
    case 'valid': return { text: '会话有效', variant: 'up' as const, icon: ShieldCheck }
    case 'expired': return { text: '会话已过期', variant: 'down' as const, icon: ShieldAlert }
    case 'unreachable': return { text: '网关不可达', variant: 'down' as const, icon: ShieldQuestion }
    default: return { text: '检查中…', variant: 'default' as const, icon: RefreshCw }
  }
})
const sessionHint = computed(() => {
  switch (sessionState.value) {
    case 'valid': return '会话由网关签发并保存在网关内存里；网关重启后所有会话失效，需要重新登录。'
    case 'expired': return '当前 token 已被网关拒绝。网关重启、登出或长时间未活动都会导致过期 — 重新登录即可恢复。'
    case 'unreachable': return '连不上网关（/api/ping 无应答）。检查网关进程与端口后重试。'
    default: return ''
  }
})

// ── token display + copy ────────────────────────────────────────────────────
const tokenMasked = computed(() => {
  const t = getToken()
  if (!t) return '（无会话 token）'
  if (t.length <= 10) return `${t.slice(0, 2)}…`
  return `${t.slice(0, 6)}…${t.slice(-4)}`
})
const loginTime = computed(() => {
  const at = loginAt()
  return at ? dateTime(at) : '—（本会话由 ?token= 链接交接或来自旧版本）'
})

const tokenCopied = ref(false)
const linkCopied = ref(false)
async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    // Clipboard needs a focused document + permission; a rejected copy is a
    // failed copy, and the button state must not lie about it.
    return false
  }
}
async function copyToken(): Promise<void> {
  if (await copyText(getToken())) {
    tokenCopied.value = true
    setTimeout(() => { tokenCopied.value = false }, 1800)
  }
}
/** ?token= hand-off link — the E6-a channel, now one click away. */
const panelLink = computed(() =>
  `${window.location.origin}${window.location.pathname}?token=${getToken()}`,
)
async function copyLink(): Promise<void> {
  if (await copyText(panelLink.value)) {
    linkCopied.value = true
    setTimeout(() => { linkCopied.value = false }, 1800)
  }
}

// ── gateway console ─────────────────────────────────────────────────────────
const gateway = computed(() => store.snapshot?.gateway ?? null)
const engineUp = computed(() => store.snapshot?.connected ?? false)
/** Reuse the lifecycle rules verbatim — the console must not invent its own. */
const control = computed(() => controlState(gateway.value, engineUp.value))
const exit = computed(() => exitNotice(gateway.value, engineUp.value))

const busy = ref(false)
const cmdMsg = ref<string | null>(null)
async function send(cmd: 'start' | 'stop' | 'status'): Promise<void> {
  busy.value = true
  cmdMsg.value = null
  try {
    const res: CommandDoc = await api.command(cmd)
    cmdMsg.value = res.message ?? `${cmd} 已下发`
    await store.refresh()
  } catch (e) {
    cmdMsg.value = e instanceof Error ? e.message : String(e)
  } finally {
    busy.value = false
    setTimeout(() => { cmdMsg.value = null }, 6000)
  }
}

// ── 网络诊断 ────────────────────────────────────────────────────────────────
const netDoc = ref<NetCheckDoc | null>(null)
/** 读法（标签、计数、空/探测中/失败三态）全部来自 lib/net-check.ts。 */
const netView = computed(() => readNetCheck(netDoc.value))
const netErr = ref<string | null>(null)
const netBusy = ref(false)
/** 探测在网关侧后台跑；这里只轮询缓存，直到 `probing` 落下。 */
let netTimer: ReturnType<typeof setTimeout> | null = null

function scheduleNetPoll(): void {
  if (netTimer !== null) {
    clearTimeout(netTimer)
    netTimer = null
  }
  if (netDoc.value?.probing) {
    netTimer = setTimeout(() => void loadNet(), 1200)
  }
}

async function loadNet(): Promise<void> {
  try {
    netDoc.value = await api.netCheck()
    // 「面板调不到网关」与「内核没应答探测」是两件事，分开报：前者是这一行，
    // 后者是 netView.hint（网关把内核的错误原样带在 error 里）。
    netErr.value = null
  } catch (e) {
    netErr.value = e instanceof Error ? e.message : String(e)
  }
  scheduleNetPoll()
}

async function probeNet(): Promise<void> {
  netBusy.value = true
  try {
    netDoc.value = await api.probeNetCheck()
    netErr.value = null
  } catch (e) {
    netErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    netBusy.value = false
    scheduleNetPoll()
  }
}

onMounted(() => void loadNet())
onUnmounted(() => {
  if (netTimer !== null) clearTimeout(netTimer)
})

const netBadge = computed(() => {
  switch (netView.value.tone) {
    case 'up': return { text: '全部通过', variant: 'up' as const }
    case 'down': return { text: '有失败', variant: 'down' as const }
    default: return { text: '探测中', variant: 'default' as const }
  }
})

// ── theme segmented ─────────────────────────────────────────────────────────
const themeSegments = [
  { id: 'system', label: '跟随系统' },
  { id: 'light', label: '浅色' },
  { id: 'dark', label: '深色' },
]
const themeValue = computed<ThemeMode>({
  get: () => theme.value,
  set: (v) => setTheme(v as ThemeMode),
})
</script>

<template>
  <div class="rise-in">
    <!-- ── 会话与访问 token ─────────────────────────────────────────────── -->
    <Card>
      <CardHeader label="会话与访问 token">
        <template #title>
          <KeyRound class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge :variant="sessionBadge.variant" dot>
            <component :is="sessionBadge.icon" class="size-3" /> {{ sessionBadge.text }}
          </Badge>
        </template>
      </CardHeader>

      <div class="grid gap-3 sm:grid-cols-2">
        <div class="rounded-lg border border-line bg-panel-2 p-3">
          <div class="label-micro">当前 token</div>
          <div class="mt-1.5 flex items-center gap-2">
            <span class="truncate font-mono text-[13px]">{{ tokenMasked }}</span>
            <Tooltip content="复制完整 token。它等于会话本身 — 交给谁，谁就能打开这个面板。">
              <Button variant="ghost" size="sm" :title="tokenCopied ? '已复制' : '复制 token'" @click="copyToken">
                <Check v-if="tokenCopied" class="size-3.5 text-up" />
                <Copy v-else class="size-3.5" />
              </Button>
            </Tooltip>
          </div>
          <p class="mt-1.5 text-[11px] leading-snug text-faint-fg">
            登录时间：{{ loginTime }}
          </p>
        </div>

        <div class="rounded-lg border border-line bg-panel-2 p-3">
          <div class="label-micro">会话交接链接（?token=）</div>
          <div class="mt-1.5 flex items-center gap-2">
            <span class="truncate text-[12px] text-muted-fg num">{{ panelLink }}</span>
            <Tooltip content="复制带 token 的面板地址：新浏览器打开即登录，无需输入密码。链接只在当前会话有效。">
              <Button variant="ghost" size="sm" :title="linkCopied ? '已复制' : '复制链接'" @click="copyLink">
                <Check v-if="linkCopied" class="size-3.5 text-up" />
                <Copy v-else class="size-3.5" />
              </Button>
            </Tooltip>
          </div>
          <p class="mt-1.5 text-[11px] leading-snug text-faint-fg">
            链接会随会话失效；请勿发到不受信任的聊天/邮件渠道。
          </p>
        </div>
      </div>

      <div class="mt-3 flex items-start justify-between gap-3 rounded-md border border-line bg-panel-2 px-3 py-2">
        <div class="flex items-start gap-2 text-[11.5px] leading-snug text-muted-fg">
          <Info class="mt-px size-3.5 shrink-0 text-faint-fg" />
          <span>{{ sessionHint || '正在探测网关会话状态…' }}</span>
        </div>
        <div class="flex shrink-0 items-center gap-2">
          <Button variant="outline" size="sm" :disabled="sessionState === 'checking'" @click="checkSession">
            <RefreshCw class="size-3.5" />重新探测
          </Button>
          <Button variant="ghost" size="sm" title="清除本地会话并回到登录页" @click="logout">
            <LogOut class="size-3.5" />退出登录
          </Button>
        </div>
      </div>
    </Card>

    <!-- ── Gateway 指令台 ──────────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="Gateway 指令台">
        <template #title>
          <TerminalSquare class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge :variant="engineUp ? 'up' : 'down'" dot>{{ engineUp ? '内核在线' : '内核离线' }}</Badge>
        </template>
      </CardHeader>

      <div v-if="gateway" class="grid gap-x-6 gap-y-2 text-[12px] sm:grid-cols-2">
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">socket</span>
          <span class="truncate text-right num" :title="gateway.socket">{{ gateway.socket }}</span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">进程控制（--manage）</span>
          <Badge :variant="gateway.lifecycleEnabled ? 'up' : 'default'">
            {{ gateway.lifecycleEnabled ? '已开启' : '未开启（只读）' }}
          </Badge>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">内核归属</span>
          <span>{{ gateway.managed ? '本网关启动（可停止）' : '外部进程（只接管读取）' }}</span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">内核 PID</span>
          <span class="num">
            <template v-if="gateway.corePid != null"><RollingNumber :value="gateway.corePid" /></template>
            <template v-else>—</template>
          </span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">崩溃自动重启次数</span>
          <span class="num"><RollingNumber :value="gateway.restarts ?? 0" /></span>
        </div>
        <div class="flex items-center justify-between gap-3 border-b border-line pb-1.5">
          <span class="text-faint-fg">已放弃重启</span>
          <Badge :variant="gateway.restartGivenUp ? 'down' : 'default'">{{ gateway.restartGivenUp ? '是' : '否' }}</Badge>
        </div>
      </div>
      <p v-else class="flex items-center gap-2 text-[12px] text-faint-fg">
        <Server class="size-3.5" />当前网关未上报进程控制信息（旧版网关或只读适配器），指令台不可用。
      </p>

      <!--
        A crash is an alert, so it wears the kit's alert tint rather than a
        page-local copy of it (issue 260): the hand-rolled `border-down/35 bg-down/8`
        was the same semantics at a second concentration, which is how the panel
        ended up with two reds.
      -->
      <AlertBanner
        v-if="exit"
        class="mt-3"
        :tone="exit.kind === 'crash' ? 'error' : 'info'"
      >{{ exit.kind === 'crash' ? `内核曾崩溃：${exit.description}` : `内核已停止：${exit.description}` }}</AlertBanner>

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button variant="default" size="sm" :disabled="busy" title="查询网关与内核状态" @click="send('status')">
          <RefreshCw class="size-3.5" />状态查询
        </Button>
        <Button
          variant="up"
          size="sm"
          :disabled="busy || !control.canStart"
          :title="control.canStart ? '启动内核' : (control.blockedReason ?? '内核已在运行')"
          @click="send('start')"
        >
          <Play class="size-3.5" />启动内核
        </Button>
        <Button
          variant="danger"
          size="sm"
          :disabled="busy || !control.canStop"
          :title="control.canStop ? '停止本网关启动的内核' : (control.blockedReason ?? '内核未运行')"
          @click="send('stop')"
        >
          <Square class="size-3.5" />停止内核
        </Button>
        <span class="ml-auto text-[11px] text-faint-fg">指令经 /api/command 下发，token 鉴权；本面板不下发任何交易参数</span>
      </div>

      <div v-if="!control.usable && control.blockedReason" class="mt-3 flex items-start gap-2 rounded-md border border-line bg-panel-2 px-3 py-2 text-[11.5px] leading-snug text-muted-fg">
        <Info class="mt-px size-3.5 shrink-0 text-faint-fg" />
        <span>{{ control.blockedReason }}</span>
      </div>
      <div v-if="cmdMsg" class="mt-2.5 rounded-md border border-line bg-panel-2 px-3 py-2 text-[12px] text-muted-fg">
        {{ cmdMsg }}
      </div>
    </Card>

    <!-- ── 网络诊断 ────────────────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="网络诊断">
        <template #title>
          <Network class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge :variant="netBadge.variant" dot>{{ netBadge.text }}</Badge>
        </template>
      </CardHeader>

      <p class="text-[11.5px] leading-snug text-muted-fg">
        内核拨测它自己出网用到的每条路径（解析 → TCP → TLS → 一次廉价请求）。
        读数是内核进程本身看到的网络，不是浏览器这一侧 —— 这正是「是网络还是交易所」的分界。
      </p>

      <div v-if="netView.rows.length" class="mt-3">
        <div
          v-for="(row, i) in netView.rows"
          :key="row.name"
          class="flex flex-wrap items-center gap-x-2 gap-y-1 py-2"
          :class="i > 0 ? 'border-t border-line' : ''"
        >
          <Badge :variant="row.ok ? 'up' : 'down'" dot>{{ row.label }}</Badge>
          <span class="text-[12.5px] font-semibold">{{ row.name }}</span>
          <span class="min-w-0 truncate text-[11.5px] text-faint-fg" :title="row.target">{{ row.target }}</span>
          <span class="ml-auto flex items-center gap-2 text-[11.5px] text-muted-fg">
            <!--
              The TUI paints this fact in its WARN colour, so it may not sit in
              the panel's faintest tier here: "the resolver answered with a
              fake IP" changes how a failure below should be read, and the two
              faces must not disagree about how loud it is (issue 260). `text-primary`
              is the panel's existing warn token — the same one AlertBanner's
              `warn` tone uses.
            -->
            <Tooltip v-if="row.fakeIp" content="解析到的地址全在代理的 fake-IP 段内：这是解析器的事实，不是这张路径的故障。">
              <span class="text-primary">fake-IP</span>
            </Tooltip>
            <span v-if="row.ms != null" class="num">{{ row.ms }} ms</span>
          </span>
          <p v-if="row.detail" class="w-full text-[11px] leading-snug text-faint-fg">{{ row.detail }}</p>
        </div>
      </div>

      <EmptyState
        v-else
        compact
        :loading="netView.tone === 'default'"
        :text="netView.summary"
        :hint="netView.emptyHint ?? undefined"
      />

      <!--
        The verdict strip is an alert, so it is the kit's `AlertBanner` and not a
        page-local tint (issue 260). `info` covers both "all paths OK, here is the
        reading" and "no verdict yet" — the same tone the Plugins page uses for a
        normal state; only a real failure gets the error tint.
      -->
      <AlertBanner
        class="mt-3"
        :tone="netView.tone === 'down' ? 'error' : 'info'"
        :title="netView.title"
        :hint="netView.hint"
      >{{ netView.summary }}</AlertBanner>

      <!-- Same fact, same volume as the TUI's WARN-coloured proxy line (issue 260):
           a proxy in front of the probe is the explanation for half the
           readings below, so it may not be the faintest text on the card. -->
      <p v-if="netView.proxyNote" class="mt-2 text-[11px] leading-snug text-primary">{{ netView.proxyNote }}</p>
      <p v-if="netErr" class="mt-2 text-[11px] leading-snug text-down">面板调用失败：{{ netErr }}</p>

      <div class="mt-3 flex flex-wrap items-center gap-2">
        <Button
          variant="outline"
          size="sm"
          :disabled="netBusy || netView.tone === 'default'"
          title="让内核重新拨测一遍（结果在网关侧缓存 20 秒）"
          @click="probeNet"
        >
          <RefreshCw class="size-3.5" />重新探测
        </Button>
        <span class="text-[11px] text-faint-fg">
          <template v-if="netView.ageNote">结果时间：{{ netView.ageNote }}；探测在网关后台运行，页面自动刷新</template>
          <template v-else>还没有探测结果</template>
        </span>
      </div>
    </Card>

    <!-- ── 外观与节奏 ──────────────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="外观与刷新" />
      <div class="grid gap-4 sm:grid-cols-2">
        <div class="flex items-center justify-between gap-3">
          <div>
            <div class="text-[12.5px] font-semibold">主题</div>
            <p class="mt-0.5 text-[11px] text-faint-fg">跟随系统 / 浅色 / 深色，本地记忆</p>
          </div>
          <SegmentedControl v-model="themeValue" :segments="themeSegments" size="sm" />
        </div>
        <div class="flex items-center justify-between gap-3">
          <div>
            <div class="text-[12.5px] font-semibold">提示音</div>
            <p class="mt-0.5 text-[11px] text-faint-fg">开/平仓、熔断提示音，本地记忆</p>
          </div>
          <Switch :model-value="sound" @update:model-value="setSoundEnabled" />
        </div>
        <div class="flex items-center justify-between gap-3">
          <div>
            <div class="text-[12.5px] font-semibold">行情快速刷新</div>
            <p class="mt-0.5 text-[11px] text-faint-fg">行情面板 2s 轮询；关闭后跟全局 15s，适合只盯盘不交易</p>
          </div>
          <Switch :model-value="settings.fastPoll" @update:model-value="settings.setFastPoll" />
        </div>
      </div>
    </Card>
  </div>
</template>
