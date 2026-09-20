<script setup lang="ts">
/**
 * 设置 — E8-d 指令面 & 收尾。
 *
 * 三块：
 *   1. 会话与访问 token：状态探测（/api/ping 三态）、token 掩码展示、复制、
 *      带 ?token= 的面板链接复制（E6-a 会话交接通道）、过期语义提示。
 *   2. Gateway 指令台：网关身份（socket/--manage/managed/pid/重启次数）、
 *      status/start/stop 指令（沿用 lifecycle 的能力闸，不重造规则）。
 *   3. 外观与节奏：主题三态、提示音、行情快速轮询（设置持久化 store）。
 *
 * 本页只做展示与指令；凭证与下单原语永不出内核。
 */
import { computed, onMounted, ref } from 'vue'
import {
  Copy, Check, KeyRound, RefreshCw, ShieldCheck, ShieldAlert, ShieldQuestion,
  TerminalSquare, Play, Square, LogOut, Info, Server,
} from 'lucide-vue-next'
import { api, getToken, loginAt, logout, ping, probeSession, type CommandDoc } from '@/api/client'
import { adjudicateSession } from '@/lib/session'
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

      <div
        v-if="exit"
        class="mt-3 flex items-start gap-2 rounded-md border px-3 py-2 text-[11.5px] leading-snug"
        :class="exit.kind === 'crash' ? 'border-down/35 bg-down/8 text-down' : 'border-line bg-panel-2 text-muted-fg'"
      >
        <span>{{ exit.kind === 'crash' ? `内核曾崩溃：${exit.description}` : `内核已停止：${exit.description}` }}</span>
      </div>

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
