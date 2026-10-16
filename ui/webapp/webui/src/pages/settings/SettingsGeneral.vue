<script setup lang="ts">
/**
 * 设置 · 通用 —— 会话与访问 token（探测 / 掩码 / 复制），轮次时间周期
 * （next_session 切换 5m/15m/1h/4h/日轮），交易环境重置（issue 458：确认词门，
 * 停策略/撤挂单/清记录/复原 seed 一次完成），外观与刷新节奏（主题三态、
 * 提示音、行情快速轮询）。
 */
import { computed, onMounted, ref } from 'vue'
import { Copy, Check, KeyRound, RefreshCw, ShieldCheck, ShieldAlert, ShieldQuestion, LogOut, Info, SlidersHorizontal, Timer, RotateCcw } from 'lucide-vue-next'
import { getToken, loginAt, logout, ping, probeSession, api } from '@/api/client'
import type { RoundDurationDoc, ResetTradingResultDoc } from '@/api/client'
import { adjudicateSession } from '@/lib/session'
import { dateTime } from '@/lib/format'
import { useRestartCore, RESTART_DELAY_SEC } from '@/lib/restart-core'
import { useSettingsStore } from '@/stores/settings'
import { usePanelStore } from '@/stores/panel'
import { useTheme, type ThemeMode } from '@/lib/theme'
import Card from '@/components/ui/card/Card.vue'
import CardHeader from '@/components/ui/card/CardHeader.vue'
import Badge from '@/components/ui/badge/Badge.vue'
import Button from '@/components/ui/button/Button.vue'
import SegmentedControl from '@/components/ui/segmented/SegmentedControl.vue'
import Switch from '@/components/ui/switch/Switch.vue'
import Tooltip from '@/components/ui/tooltip/Tooltip.vue'
import AlertBanner from '@/components/ui/alert/AlertBanner.vue'
import ConfirmWordDialog from '@/components/ui/confirm/ConfirmWordDialog.vue'

const settings = useSettingsStore()
/** 指引卡跳转到插件页的凭据卡（requestNav 是面板级导航的统一入口）。 */
function navToPlugins(): void {
  usePanelStore().requestNav('plugins')
}
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
  return at ? dateTime(at) : '—（本会话来自旧版本，或登录时间未知）'
})

const tokenCopied = ref(false)
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

// ── 轮次时间周期（next_session：轮次网格是坐标系，热切会错解存量 round_slot）──
const roundDoc = ref<RoundDurationDoc | null>(null)
const roundErr = ref<string | null>(null)
const roundMsg = ref<string | null>(null)
const roundBusy = ref(false)
const roundPending = ref<number | null>(null)

// issue 488：写入持久格成功后 10s 自动重启内核（可在倒计时内取消/立即）。
const restart = useRestartCore()

const roundSegments = computed(() => {
  const allowed = roundDoc.value?.allowed ?? []
  const seen = new Set<number>()
  const out: { id: string; label: string }[] = []
  for (const o of allowed) {
    if (seen.has(o.sec)) continue
    seen.add(o.sec)
    out.push({ id: String(o.sec), label: o.label ?? `${o.sec}s` })
  }
  return out
})
const roundValue = computed<string>({
  // 预填「下次会话将跑的值」（持久格优先），不是本会话生效值 —— 卡片承诺
  // 的是「重启后是什么」。
  get: () => String(roundPending.value ?? roundDoc.value?.nextSessionSec ?? ''),
  set: (v) => { roundPending.value = Number(v) },
})
const roundDirty = computed(
  () => roundPending.value !== null
    && roundPending.value !== roundDoc.value?.nextSessionSec,
)
const roundFlagLocked = computed(
  () => roundDoc.value?.source === 'cli_or_env',
)
const roundSourceLabel = computed(() => {
  switch (roundDoc.value?.source) {
    case 'cli_or_env': return '命令行/环境变量'
    case 'runtime': return '面板设置'
    case 'toml': return '配置文件'
    case 'env': return '环境变量'
    default: return '出厂默认'
  }
})

async function loadRound(): Promise<void> {
  roundBusy.value = true
  try {
    roundDoc.value = await api.roundDuration()
    roundErr.value = roundDoc.value.error ?? null
    roundPending.value = null
  } catch (e) {
    roundErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    roundBusy.value = false
  }
}

async function saveRound(): Promise<void> {
  if (roundPending.value === null) return
  roundBusy.value = true
  roundErr.value = null
  roundMsg.value = null
  try {
    const res = await api.roundDurationSet({
      durationSec: roundPending.value,
      reason: 'webui 面板编辑（轮次时间周期卡）',
    })
    if (res.error) {
      roundErr.value = res.error
    } else {
      roundMsg.value = `已保存：${RESTART_DELAY_SEC}s 后自动重启内核生效，将按 ${res.nextSessionSec}s 轮次运行；本会话网格不变。`
      await loadRound()
      restart.schedule()
    }
  } catch (e) {
    roundErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    roundBusy.value = false
  }
}

async function resetRound(): Promise<void> {
  roundBusy.value = true
  roundErr.value = null
  roundMsg.value = null
  try {
    const res = await api.roundDurationSet({ reset: true, reason: 'webui 面板清除（轮次时间周期卡）' })
    if (res.error) {
      roundErr.value = res.error
    } else {
      roundMsg.value = `已清除面板设置：${RESTART_DELAY_SEC}s 后自动重启，下次会话回落到启动参数解析（命令行 > 环境变量 > 配置文件 > 出厂默认）。`
      await loadRound()
      restart.schedule()
    }
  } catch (e) {
    roundErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    roundBusy.value = false
  }
}

onMounted(loadRound)

// ── 交易环境重置（issue 458：确认词门；内核三道门 + manual-ops 审计）────────────
const resetOpen = ref(false)
const resetBusy = ref(false)
const resetErr = ref<string | null>(null)
const resetMsg = ref<string | null>(null)
const resetResult = ref<ResetTradingResultDoc | null>(null)

async function doResetTrading(): Promise<void> {
  resetBusy.value = true
  resetErr.value = null
  try {
    const res = await api.systemResetTrading({
      confirm: 'RESET',
      reason: 'webui 面板（交易环境重置卡）',
    })
    if (res.error) {
      resetErr.value = res.error
    } else {
      resetResult.value = res
      const b = res.balances[0]?.[1] ?? '?'
      resetMsg.value = `已重置：停用 ${res.disabledStrategies.length} 个策略、撤销 ${res.cancelledOrders} 张挂单、清空 ${res.clearedClosedTrades} 条成交记录，现金已复原到 ${b}。动作已写入 manual-ops 审计。`
    }
  } catch (e) {
    resetErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    resetBusy.value = false
    resetOpen.value = false
  }
}
</script>

<template>
  <div>
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

    <!-- ── 轮次时间周期（next_session）────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="轮次时间周期">
        <template #title>
          <Timer class="size-4 text-faint-fg" />
        </template>
        <template #action>
          <Badge v-if="roundDoc" variant="default" dot>
            本会话 {{ roundDoc.effectiveSec }}s（{{ roundSourceLabel }}）
          </Badge>
        </template>
      </CardHeader>

      <AlertBanner v-if="!roundDoc && !roundErr" class="mb-3" tone="info">正在读取轮次周期…</AlertBanner>
      <AlertBanner v-else-if="!roundDoc" class="mb-3" tone="warn">
        读不到轮次周期读数（{{ roundErr ?? '内核离线或版本过旧' }}）。这个内核可能不认识 round_duration.get。
      </AlertBanner>

      <template v-else>
        <div class="flex items-center justify-between gap-3">
          <div>
            <div class="text-[12.5px] font-semibold">下次会话的轮次长度</div>
            <p class="mt-0.5 text-[11px] text-faint-fg">
              切换在重启内核后生效 —— 轮次网格是坐标系（round_slot 按会话的周期推导），热切会错解存量仓位与订单。
            </p>
          </div>
          <SegmentedControl
            v-model="roundValue"
            :segments="roundSegments"
            size="sm"
            :disabled="roundBusy"
          />
        </div>

        <div class="mt-3 flex items-start justify-between gap-3 rounded-md border border-line bg-panel-2 px-3 py-2">
          <div class="flex items-start gap-2 text-[11.5px] leading-snug text-muted-fg">
            <Info class="mt-px size-3.5 shrink-0 text-faint-fg" />
            <span v-if="roundFlagLocked">
              当前轮次由启动参数（--round-sec 或 BK_ROUND_SEC 环境变量）显式指定。面板写入仍会保存，但显式参数优先 —— 想让面板设置生效，请去掉该启动参数后重启。
            </span>
            <span v-else-if="roundDoc.persisted">
              已有面板设置：下次会话按 {{ roundDoc.nextSessionSec }}s 运行。清除后回落到启动参数解析。
            </span>
            <span v-else>
              尚无面板设置：下次会话按 {{ roundSourceLabel }}的 {{ roundDoc.effectiveSec }}s 运行。
            </span>
          </div>
          <div class="flex shrink-0 items-center gap-2">
            <Button
              variant="ghost"
              size="sm"
              :disabled="roundBusy || !roundDoc.persisted"
              title="清除面板设置，下次会话回落到启动参数解析"
              @click="resetRound"
            >
              <RefreshCw class="size-3.5" />清除
            </Button>
            <Button
              size="sm"
              :disabled="roundBusy || !roundDirty"
              title="把选中的轮次周期写入持久格（重启内核后生效）"
              @click="saveRound"
            >
              <Check class="size-3.5" />保存
            </Button>
          </div>
        </div>

        <AlertBanner v-if="roundErr" class="mt-3" tone="warn">{{ roundErr }}</AlertBanner>
        <AlertBanner v-if="roundMsg" class="mt-3" tone="info">{{ roundMsg }}</AlertBanner>

        <!-- issue 488：倒计时条（写入成功后出现）。10s 自动重启；可立即执行或取消。 -->
        <div v-if="restart.counting.value" class="mt-3 flex flex-wrap items-center gap-2 rounded-md border border-line bg-panel-2 px-3 py-2">
          <span class="text-[12px] font-semibold text-gold">{{ restart.countdown.value }}s 后自动重启内核</span>
          <Button size="sm" variant="outline" :disabled="restart.restarting.value" @click="restart.now">立即重启</Button>
          <Button size="sm" variant="ghost" :disabled="restart.restarting.value" @click="restart.cancel">取消</Button>
        </div>
        <AlertBanner
          v-if="restart.restarting.value"
          class="mt-3"
          tone="info"
          title="内核重启中…"
        >本页读数会短暂断开，内核重新上线后自动恢复。</AlertBanner>
        <AlertBanner
          v-if="restart.restartErr.value"
          class="mt-3"
          tone="error"
          :title="restart.restartErr.value"
        />
        <AlertBanner
          v-if="restart.restartMsg.value && !restart.counting.value"
          class="mt-3"
          tone="info"
          :title="restart.restartMsg.value"
        />
      </template>
    </Card>

    <!-- ── 交易环境重置（issue 458）──────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="交易环境重置">
        <template #title>
          <RotateCcw class="size-4 text-faint-fg" />
        </template>
      </CardHeader>

      <div class="flex items-start justify-between gap-3">
        <div>
          <div class="text-[12.5px] font-semibold">清空本地交易历史并复原虚拟资金</div>
          <p class="mt-0.5 text-[11px] leading-snug text-faint-fg">
            停用全部已启用策略 → 撤销全部挂单 → 清空订单/成交/持仓记录 → 按配置 seed 复原现金。
            配置、策略文件、审计流与策略启用状态保留；动作写入 manual-ops 审计（含重置前快照）。
            仅 DRY/模拟盘可用 —— 实盘模式下内核拒绝。
          </p>
        </div>
        <Button variant="danger-solid" size="sm" class="shrink-0" :disabled="resetBusy" @click="resetOpen = true">
          <RotateCcw class="size-3.5" />重置…
        </Button>
      </div>

      <AlertBanner v-if="resetErr" class="mt-3" tone="error">{{ resetErr }}</AlertBanner>
      <AlertBanner v-if="resetMsg" class="mt-3" tone="info">{{ resetMsg }}</AlertBanner>
    </Card>

    <ConfirmWordDialog
      v-model:open="resetOpen"
      word="RESET"
      title="交易环境重置"
      :lines="[
        '停用全部已启用策略（重启后不会自动恢复）',
        '撤销全部 resting 挂单，清空订单/成交/持仓记录（不可恢复）',
        '每个账户的现金复原到配置 seed 值',
        '保留：配置、策略文件、全部审计流；动作写入 manual-ops 审计',
        '实盘模式下内核会拒绝执行',
      ]"
      action-label="确认重置"
      :busy="resetBusy"
      :error="resetErr"
      @confirm="doResetTrading"
    />

    <!-- ── 外观与节奏 ──────────────────────────────────────────────────── -->
    <Card class="mt-3.5">
      <!-- issue 393 (③): same icon-leads-title pattern as every other card. -->
      <CardHeader label="外观与刷新">
        <template #title>
          <SlidersHorizontal class="size-4 text-faint-fg" />
        </template>
      </CardHeader>
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

    <!-- ── 如何创建 API（行情插件凭据指引）：静态指引卡。配置入口在插件页
         的 API 凭据卡；这里只回答「去哪申请、哪个变量填什么」。 ─────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="如何创建 API（行情插件凭据）">
        <template #title>
          <KeyRound class="size-4 text-faint-fg" />
        </template>
      </CardHeader>
      <div class="space-y-2.5 text-[12px] text-muted-fg">
        <p>
          行情与发现不需要凭据即可看数据；只有<b class="text-fg">下单执行器</b>在启动时读取环境变量。
          配置入口在 <button class="text-primary underline" @click="navToPlugins">插件页 → API 凭据</button>
          （或直接写 <span class="font-mono">.env</span>，真实环境变量始终优先）。
        </p>
        <div class="overflow-x-auto">
          <table class="w-full min-w-[620px] text-[12px]">
            <thead>
              <tr class="text-left">
                <th class="label-micro px-2 pb-2">插件</th>
                <th class="label-micro px-2 pb-2">变量</th>
                <th class="label-micro px-2 pb-2">怎么拿到</th>
              </tr>
            </thead>
            <tbody>
              <tr class="border-t border-line">
                <td class="px-2 py-2 font-semibold" rowspan="2">polymarket</td>
                <td class="px-2 py-2 font-mono text-[11px]">POLYMARKET_PRIVATE_KEY</td>
                <td class="px-2 py-2 text-faint-fg">Polygon 钱包私钥（导出钱包时获得）</td>
              </tr>
              <tr class="border-t border-line">
                <td class="px-2 py-2 font-mono text-[11px]">POLYMARKET_FUNDER_ADDRESS</td>
                <td class="px-2 py-2 text-faint-fg">Polymarket 账户的资金地址（0x…，Profile 页可复制）</td>
              </tr>
              <tr class="border-t border-line">
                <td class="px-2 py-2 font-semibold" rowspan="2">kalshi</td>
                <td class="px-2 py-2 font-mono text-[11px]">KALSHI_API_KEY_ID</td>
                <td class="px-2 py-2 text-faint-fg">Kalshi 账户 API 页创建的 Key ID（demo 环境用 demo 站点的）</td>
              </tr>
              <tr class="border-t border-line">
                <td class="px-2 py-2 font-mono text-[11px]">KALSHI_RSA_PRIVATE_KEY</td>
                <td class="px-2 py-2 text-faint-fg">创建 Key 时下载的 RSA 私钥文件全文（PEM，含首尾行）</td>
              </tr>
              <tr class="border-t border-line">
                <td class="px-2 py-2 font-semibold" rowspan="2">predictfun</td>
                <td class="px-2 py-2 font-mono text-[11px]">PREDICT_API_KEY</td>
                <td class="px-2 py-2 text-faint-fg">Predict.fun 账户设置里生成的 API Key</td>
              </tr>
              <tr class="border-t border-line">
                <td class="px-2 py-2 font-mono text-[11px]">PREDICT_PRIVATE_KEY</td>
                <td class="px-2 py-2 text-faint-fg">签名用的钱包私钥</td>
              </tr>
              <tr class="border-t border-line">
                <td class="px-2 py-2 font-semibold">polysimulator</td>
                <td class="px-2 py-2 font-mono text-[11px]">POLYSIM_API_KEY</td>
                <td class="px-2 py-2 text-faint-fg">PolySimulator（第三方模拟盘）启动时自己签发的 Key</td>
              </tr>
            </tbody>
          </table>
        </div>
        <p class="text-[11.5px] text-faint-fg">
          安全：值只下行不回显，保存在 <span class="font-mono">data/runtime/config.json</span>（仅属主可读），
          随 .env 一样留在本机；Live 模式风控与审计照常生效。改完凭据记得重启内核（凭据卡会自动安排）。
        </p>
      </div>
    </Card>
  </div>
</template>
