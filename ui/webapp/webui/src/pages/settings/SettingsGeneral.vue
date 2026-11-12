<script setup lang="ts">
/**
 * 设置 · 通用 —— 会话与访问 token（探测 / 掩码 / 复制），轮次时间周期
 * （next_session 切换 5m/15m/1h/4h/日轮），交易环境重置（issue 458：确认词门，
 * 停策略/撤挂单/清记录/复原 seed 一次完成），外观与刷新节奏（主题三态、
 * 提示音、行情快速轮询）。
 */
import { computed, onMounted, ref } from 'vue'
import { Copy, Check, KeyRound, RefreshCw, ShieldCheck, ShieldAlert, ShieldQuestion, LogOut, Info, SlidersHorizontal, Timer, RotateCcw, FileKey } from 'lucide-vue-next'
import { getToken, loginAt, logout, ping, probeSession, api } from '@/api/client'
import type { Position, ResetTradingResultDoc } from '@/api/client'
import { adjudicateSession } from '@/lib/session'
import { dateTime } from '@/lib/format'
import { useRoundDuration } from '@/lib/round-duration'
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
const envReloadBusy = ref(false)
const envReloadErr = ref<string | null>(null)
const envReloadMsg = ref<string | null>(null)

async function reloadEnv(): Promise<void> {
  envReloadBusy.value = true
  envReloadErr.value = null
  envReloadMsg.value = null
  try {
    const result = await api.envReload()
    if (result.error || !result.ok) {
      const failures = [result.accounts, result.policy, result.shadow].filter(Boolean).join('；')
      envReloadErr.value = (result.error ?? failures) || '重新加载失败'
    } else {
      envReloadMsg.value = '已重新加载账户元数据、执行策略与影子进化开关；轮次等启动期设置仍需重启。'
    }
  } catch (e) {
    envReloadErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    envReloadBusy.value = false
  }
}
/** 指引卡跳转到顶栏账户切换器（添加账户弹窗；requestNav 是面板级导航的统一入口）。 */
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
// 2026-10-24 操作者指令：切换入口主阵地挪到行情页倒计时卡（盯盘时切周期
// 不离开语境）；本卡保留为同一 composable 的第二卡面 —— 共享一份读数与
// 写路径（lib/round-duration.ts），语义零改动。
const {
  doc: roundDoc, err: roundErr, msg: roundMsg, busy: roundBusy,
  segments: roundSegments, value: roundValue, dirty: roundDirty,
  flagLocked: roundFlagLocked, execSec: roundExecSec,
  configSec: roundConfigSec, split: roundSplit, roundLabel,
  save: saveRound, reset: resetRound,
} = useRoundDuration()

// 本单（「立即结束当前轮」）：有在途仓位时必弹确认框（N 仓位 + 滑点警告），
// 无仓位直接切；flagLocked（启动参数在管）时整条路径禁用。
const roundConfirmOpen = ref(false)
const roundConfirmN = ref(0)
const roundConfirmSec = ref(0)
const roundImmediate = ref(false)

async function doSave(): Promise<void> {
  if (!roundImmediate.value) {
    await saveRound('default')
    return
  }
  // 立即切换：先查在途仓位，有则必弹确认框，不静默；无仓位则直接切。
  try {
    const snap = await api.snapshot()
    const n = (snap.positions ?? []).filter(
      (p: Position) => (p.accountId ?? 'default') === 'default' && (p.shares ?? 0) > 0,
    ).length
    if (n > 0) {
      roundConfirmN.value = n
      roundConfirmSec.value = Number(roundValue.value)
      roundConfirmOpen.value = true
      return
    }
  } catch (e) {
    roundErr.value = e instanceof Error ? e.message : String(e)
    return
  }
  await saveRound('default', { immediate: true })
}

async function doSaveImmediate(): Promise<void> {
  roundConfirmOpen.value = false
  await saveRound('default', { immediate: true })
}

// ── 交易环境重置（issue 458：确认词门；内核三道门 + manual-ops 审计）────────────
// issue 561（裁决 2）：重置按账户隔离 —— 显式携带面板 display 账户（唯一可
// 信源），重置只清洗该账户的账簿/挂单/持仓/记录，其他账户分文不动。
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
      accountId: usePanelStore().activeAccount ?? undefined,
    })
    if (res.error) {
      resetErr.value = res.error
    } else {
      resetResult.value = res
      const b = res.balances[0]?.[1] ?? '?'
      resetMsg.value = `已重置账户 ${res.accountId}：停用 ${res.disabledStrategies.length} 个策略、撤销 ${res.cancelledOrders} 张挂单、清空 ${res.clearedClosedTrades} 条成交记录，现金已复原到 ${b}。其他账户不受影响。动作已写入 manual-ops 审计。`
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
        <div class="flex min-w-0 flex-1 items-start gap-2 text-[11.5px] leading-snug text-muted-fg">
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

    <Card class="mt-3.5">
      <CardHeader label="环境变量">
        <template #title>
          <FileKey class="size-4 text-faint-fg" />
        </template>
      </CardHeader>
      <div class="flex items-start justify-between gap-3">
        <div class="min-w-0 flex-1">
          <div class="text-[12.5px] font-semibold">重新读取 .env</div>
          <p class="mt-0.5 text-[11px] leading-snug text-faint-fg">
            原地更新账户元数据、执行策略和影子进化开关，并保留余额、挂单与持仓。新增或删除账户、轮次等启动期设置仍需重启。
          </p>
        </div>
        <Button size="sm" class="shrink-0" :disabled="envReloadBusy" @click="reloadEnv">
          <RefreshCw class="size-3.5" :class="{ 'animate-spin': envReloadBusy }" />
          {{ envReloadBusy ? '加载中…' : '重新加载' }}
        </Button>
      </div>
      <AlertBanner v-if="envReloadErr" class="mt-3" tone="error">{{ envReloadErr }}</AlertBanner>
      <AlertBanner v-if="envReloadMsg" class="mt-3" tone="info">{{ envReloadMsg }}</AlertBanner>
    </Card>

    <!-- ── 轮次时间周期（next_session）────────────────────────────────── -->
    <Card class="mt-3.5">
    <CardHeader label="轮次时间周期（本会话）">
      <template #title>
        <Timer class="size-4 text-faint-fg" />
      </template>
    </CardHeader>

      <AlertBanner v-if="!roundDoc && !roundErr" class="mb-3" tone="info">正在读取轮次周期…</AlertBanner>
      <AlertBanner v-else-if="!roundDoc" class="mb-3" tone="warn">
        读不到轮次周期读数（{{ roundErr ?? '内核离线或版本过旧' }}）。这个内核可能不认识 round_duration.get。
      </AlertBanner>

      <template v-else>
        <!-- 单一真相源：本会话执行值（per-account 覆盖优先，缺省回落 default 网格）+
             下一轮值。配置/执行“双真相”行与分裂红条已收敛（issue 1151）：卡面只展示
             执行真相，分裂告警能力保留于内核门禁，不在卡面常态重复。 -->
        <div class="mt-3 flex flex-col sm:flex-row items-start sm:items-center justify-between gap-3 sm:gap-0">
          <div class="min-w-0 sm:min-w-auto">
            <SegmentedControl
              v-model="roundValue"
              :segments="roundSegments"
              size="sm"
              :disabled="roundBusy || roundFlagLocked"
              class="w-full sm:w-auto"
            />
          </div>
        </div>

        <div class="mt-3 flex flex-col sm:flex-row items-start sm:items-center justify-between gap-3 sm:gap-0 rounded-md border border-line bg-panel-2 px-3 py-2">
          <div class="flex min-w-0 flex-1 items-start gap-2 text-[11.5px] leading-snug text-muted-fg">
            <Info class="mt-px size-3.5 shrink-0 text-faint-fg" />
            <span>
              本会话按 <span class="font-medium">{{ roundExecSec }}</span>s 运行；
              <span v-if="roundDirty">下一轮起按 <span class="font-medium">{{ roundValue }}</span>s</span>
              <span v-else>下一轮起按 <span class="font-medium">{{ roundExecSec }}</span>s</span>。
              在途轮保持出生网格结算。
              <span v-if="roundFlagLocked" class="block mt-0.5 text-[11px] text-faint-fg">
                当前轮次由启动参数（--round-sec / BK_ROUND_SEC）显式指定，面板写入不会覆盖；去掉该启动参数后重启即生效。
              </span>
            </span>
          </div>
          <div class="flex flex-wrap items-center gap-2 sm:shrink-0 sm:flex-nowrap">
            <Switch
              v-model="roundImmediate"
              :disabled="roundBusy || roundFlagLocked || !roundDirty"
              label="立即结束当前轮（市价平掉在途仓位）"
            />
            <Button
              variant="ghost"
              size="sm"
              :disabled="roundBusy || roundDoc.perAccount?.['default'] === undefined"
              title="清除面板设置，本会话回落到启动参数解析"
              @click="resetRound('default')"
            >
              <RefreshCw class="size-3.5" />清除
            </Button>
            <Button
              size="sm"
              :disabled="roundBusy || !roundDirty"
              title="把选中的轮次周期写入持久格；开启「立即结束当前轮」时会先平掉在途仓位再切网格"
              @click="doSave"
            >
              <Check class="size-3.5" />保存
            </Button>
          </div>
        </div>

        <!-- 分裂告警能力保留（红条一级）：default 账户配置网格 ≠ 执行网格时红条，
             不静默。配置/执行并列的“常态显示”已收敛，此处只在真分裂时出现（issue 1151）。 -->
        <AlertBanner v-if="roundSplit" class="mt-3" tone="error">
          轮次配置与执行分裂：配置 {{ roundLabel(roundConfigSec) }} / 实际 {{ roundLabel(roundExecSec) }}
          —— 不静默。default 账户 per-account 覆盖与市场声明族不一致；在途轮保持原网格结算。
        </AlertBanner>
        <AlertBanner v-if="roundErr" class="mt-3" tone="warn">{{ roundErr }}</AlertBanner>
        <AlertBanner v-if="roundMsg" class="mt-3" tone="info">{{ roundMsg }}</AlertBanner>
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
        <div class="min-w-0 flex-1">
          <div class="text-[12.5px] font-semibold">清空当前账户的交易历史并复原虚拟资金</div>
          <p class="mt-0.5 text-[11px] leading-snug text-faint-fg">
            对当前显示账户：停用其已启用策略 → 撤销其挂单 → 清空其订单/成交/持仓记录 → 按配置 seed 复原其现金。
            其他账户与配置、策略文件、审计流、策略启用状态一律保留；动作写入 manual-ops 审计（含重置前快照与账户范围）。
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
        '只重置当前显示账户：停用其已启用策略（重启后不会自动恢复）',
        '撤销该账户全部 resting 挂单，清空其订单/成交/持仓记录（不可恢复）',
        '该账户的现金复原到配置 seed 值；其他账户分文不动',
        '保留：配置、策略文件、全部审计流；动作写入 manual-ops 审计',
        '实盘模式下内核会拒绝执行',
      ]"
      action-label="确认重置"
      :busy="resetBusy"
      :error="resetErr"
      @confirm="doResetTrading"
    />

    <!-- 本单：立即切换轮次的确认框——有在途仓位时必弹，N 仓位 + 滑点警告。 -->
    <ConfirmWordDialog
      v-model:open="roundConfirmOpen"
      word="YES"
      title="立即切换轮次"
      :lines="[
        `将立即平掉 ${roundConfirmN} 个在途仓位，并按 ${roundLabel(roundConfirmSec)}s 开新轮，`,
        '强平按市价成交，可能产生滑点损失；已成交部分如实入账，不静默。',
      ]"
      action-label="确认立即切换"
      :busy="roundBusy"
      :error="roundErr"
      @confirm="doSaveImmediate"
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

    <!-- ── 如何创建 API（行情插件凭据指引）：静态指引卡。凭据录入的入口在
         顶栏账户切换器（添加账户弹窗，选交易所 + 填 API）；这里只回答
         「去哪申请、哪个变量填什么」。 ─────────────── -->
    <Card class="mt-3.5">
      <CardHeader label="如何创建 API（行情插件凭据）">
        <template #title>
          <KeyRound class="size-4 text-faint-fg" />
        </template>
      </CardHeader>
      <div class="space-y-2.5 text-[12px] text-muted-fg">
        <p>
          行情与发现不需要凭据即可看数据；只有<b class="text-fg">下单执行器</b>在启动时读取环境变量。
          配置入口在 <button class="text-primary underline" @click="navToPlugins">顶栏账户切换器 → 添加账户</button>
          （或直接写 <span class="font-mono">.env</span>，真实环境变量始终优先）。
        </p>
        <div class="overflow-x-auto">
          <table class="w-full text-[12px]">
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
          安全：值只下行不回显，保存在 <span class="font-mono">.env</span>（唯一持久凭据格，仅属主可读），
          留在本机；Live 模式风控与审计照常生效。写入后重启一次内核生效（执行器在启动时读取凭据）；
          账户切换器里的「凭证已载」徽章在重启后翻绿。
        </p>
      </div>
    </Card>
  </div>
</template>
