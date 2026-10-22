<script setup lang="ts">
/**
 * AccountSwitcher — E28 (§9.5): the multi-account book, rendered.
 *
 * Self-contained by territory: the component talks to the SAME gateway
 * command surface the TUI command bar uses (`accounts` / `account <id>` —
 * gateway/command.rs), so no second client surface is minted here. One fetch
 * on open, one on switch; every account row comes verbatim from the core's
 * `account.list` reply.
 *
 * §9.4 discipline extends to the pixel layer: credentials appear only as the
 * presence fact `credentialsLoaded` — there is no field in the wire to leak
 * and this component would not render one if there were.
 */
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { Wallet, ChevronDown, Check, RefreshCw, Link2, Plus, Trash2 } from 'lucide-vue-next'
import { getToken, api } from '../api/client'
import { money } from '../lib/format'
import { usePanelStore } from '../stores/panel'
import AddAccountDialog from './AddAccountDialog.vue'
import ConfirmWordDialog from './ui/confirm/ConfirmWordDialog.vue'

/** Mirror of the core's AccountView (ipc/schema.rs) — camelCase wire.
 *  `venue` 是账户↔交易所绑定（market-plugin 名），只在建户时设定（绑定
 *  即启用）——已有账户不改绑（改绑面已收，数据不混乱），要换交易所 =
 *  删户重建。老内核不回该字段 → undefined，按未绑定（''）渲染。
 *  `tradingEnabled`/`strategies` 是多账户扇出面（关 = 拒开仓放平仓；
 *  strategies = 该账户的策略启用集，非空 = 逐字集合，空 = 显式不跑 ——
 *  INHERIT 已废除，空集不跟随全局开关，issue 552 裁决）
 *  ——老内核不回 → undefined，开关按开启渲染、启用集按「未启用」渲染。
 *  `strategies` 只读展示：启停唯一入口在策略页的启用按钮（账户↔venue
 *  解耦裁决 —— 分配编辑器退场）。 */
interface AccountRow {
  id: string
  name: string
  marketType: string
  status: string
  statusReason?: string
  balance: string | number
  available: string | number
  credentialsLoaded: boolean
  /** 账户绑定的交易所（market-plugin 名）；空串 = 未绑定（可交易任意交易所）。 */
  venue?: string
  /** 交易开关（多账户扇出）；false = 拒开仓、平仓照放。老内核缺省 = 开。 */
  tradingEnabled?: boolean
  /** 策略启用集（多账户扇出）；非空 = 逐字集合，空 = 显式不跑。老内核缺省 = 未启用。 */
  strategies?: string[]
  reserved?: string | number
  openPositions: number
  dayRealizedUsd: string | number
}

/** Mirror of AccountListResult: `active` is the kernel-side session default
 *  (§9.5). issue 561 起它不再驱动显示 —— 面板唯一可信源是 displayAccount。*/
interface AccountsDoc {
  version: string
  active: string
  displayAccount?: string
  accounts: AccountRow[]
}

/** gateway CommandOutcome — `data` carries the raw account.list reply. */
interface CommandOutcome {
  ok: boolean
  message?: string
  data?: AccountsDoc
}

const STATUS: Record<string, { label: string; tone: string }> = {
  active: { label: '正常', tone: 'var(--up)' },
  read_only: { label: '只读', tone: 'var(--amber, #eab308)' },
  frozen: { label: '冻结', tone: 'var(--down)' },
  suspended: { label: '风控暂停', tone: 'var(--down)' },
}
function statusOf(s: string): { label: string; tone: string } {
  return STATUS[s] ?? { label: s, tone: 'var(--faint-fg)' }
}

const store = usePanelStore()
const open = ref(false)
const loading = ref(false)
const switching = ref<string | null>(null)
const error = ref<string | null>(null)
const switchErr = ref<string | null>(null)
const doc = ref<AccountsDoc | null>(null)

/** issue 561：按钮与选中勾都读 store 的 display 账户（唯一可信源）——
 *  不再读本组件私有 `doc.active`（会话格语义，随 dispatcher 重连漂移）。
 *  doc 仍负责下拉列表数据。快照轮询每拍带回内核读数，Header 与页面从此
 *  同源同节拍。 */
const active = computed(() => store.displayAccount || '')
const rows = computed(() => doc.value?.accounts ?? [])
async function command(cmd: string): Promise<CommandOutcome> {
  const res = await fetch('/api/command', {
    method: 'POST',
    headers: { 'X-Auth-Token': getToken(), 'Content-Type': 'application/json' },
    body: cmd,
  })
  if (res.status === 401) throw new Error('未登录或会话已失效，请重新登录。')
  if (!res.ok) throw new Error(`网关返回 ${res.status}`)
  return (await res.json()) as CommandOutcome
}

/**
 * issue 561（2026-10-20 操作者裁决）：display account 升格为面板唯一可信
 * 账户事实源 —— 切换动作 = 写 display；一切消费点（Header、行情过滤、
 * 策略归属、风控、流水卡）读 display。
 *
 * switchTo 重排：**先** `display.account.set`（经 POST /api/display-account/
 * set 专用代理），**成功才**乐观写 store（setDisplayAccount）并刷新读数；
 * 失败（网络/未知账户）回滚乐观写并在切换器内 inline 可见提示 —— 不再
 * 静默 catch。旧 `account <id>` 文本动词调用移除（内核保留该动词：CLI/
 * 门禁兼容面），本连接的会话格不再被面板触碰 —— dispatcher 3s 重连清零
 * 会话格也不再引起「页面静默回 default 而 Header 冻结」（两处都读同一
 * display 源，重连后快照带回的 display 账户直接驱动全部消费点）。
 *
 * 切户残留窗口（issue 552 审计）：乐观写后、refresh 落地前，store 里还躺
 * 着旧账户的账户级读数（accountTradeSummary / accountStrategyStats），清
 * 掉它们让页面回落全局面读数，新读数到达后立即恢复。
 *
 * 仍属纯视图上下文：不发任何启停/冻结/平仓动词，切换不启停任何东西的
 * 合同原样成立（check:switch-purity 的牙继续钉住）。
 */
async function switchTo(id: string): Promise<void> {
  if (id === active.value || switching.value) return
  switching.value = id
  switchErr.value = null
  const prev = active.value
  try {
    const out = await api.displayAccountSet({ accountId: id, reason: 'panel account switch' })
    if (out.error) throw new Error(out.error)
    store.setDisplayAccount(id)
    store.clearAccountScoped()
    await refresh()
    store.refresh()
  } catch (e) {
    // 失败回滚乐观写（下一拍快照本会带回内核读数，这里显式复位让 UI 立
    // 刻回到真实状态），错误 inline 呈现在切换器内 —— 可见，不静默。
    if (prev) store.setDisplayAccount(prev)
    switchErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    switching.value = null
  }
}

async function refresh(): Promise<void> {
  loading.value = true
  error.value = null
  try {
    const out = await command('accounts')
    if (!out.ok) throw new Error(out.message ?? '账户列表不可用')
    if (!out.data) throw new Error('网关未返回账户数据')
    doc.value = out.data
    // Mirror into the store: pages read the display account id + rows from
    // here to label their figures (余额瓦片 / 挂单卡 / 插件页信息行) without
    // each re-speaking the command surface. issue 561：镜像走 setAccounts
    // 的 display 兜底通道（报文 displayAccount 在场采纳；快照在场时镜像
    // 不覆盖内核读数）。
    store.setAccounts(
      out.data.active,
      out.data.accounts.map((a) => ({
        id: a.id,
        name: a.name,
        marketType: a.marketType,
        status: a.status,
        balance: a.balance,
        available: a.available,
        venue: a.venue ?? '',
        strategies: Array.isArray(a.strategies) ? [...a.strategies] : undefined,
        reserved: a.reserved ?? '',
      })),
      out.data.displayAccount ?? undefined,
    )
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

function toggle(): void {
  open.value = !open.value
  if (open.value) void refresh()
}

const venueLabel = (v: string | undefined): string =>
  v ? v : '未绑定（可交易任意交易所）'

// ── 多账户扇出：交易开关 ────────────────────────────────────────────────────
// 关 = 拒开仓、平仓照放（「关=拒开仓、放平仓」裁决）——exit 照流所以持仓
// 不会被孤儿化，卡片上以「N 笔持仓管理中」披露。走专用代理路由（body 即
// params 逐字转发，同 account-venue/set 的纪律）。
// （策略分配编辑器已退场，账户↔venue 解耦裁决：启停唯一入口 = 策略页的
// 启用按钮，按「账户+策略」级别读写 —— 这里不再有第二张表。）

const tradingBusy = ref<string | null>(null)
const tradingErr = ref<string | null>(null)
const tradingMsg = ref<string | null>(null)

function tradingLabel(a: AccountRow): string {
  if (a.tradingEnabled === false) return a.openPositions > 0 ? `已停止·${a.openPositions} 笔持仓管理中` : '已停止'
  return '交易中'
}

/** 关走确认词弹窗（误触会立即拒掉真实入场）；开是恢复性的，直接执行。 */
const tradingConfirmOpen = ref(false)
const tradingConfirmTarget = ref<AccountRow | null>(null)
const tradingConfirmBusy = ref(false)
const tradingConfirmErr = ref<string | null>(null)

function askTrading(a: AccountRow): void {
  tradingErr.value = null
  if (a.tradingEnabled === false) {
    void setTrading(a, true)
    return
  }
  tradingConfirmTarget.value = a
  tradingConfirmOpen.value = true
}

async function setTrading(a: AccountRow, enabled: boolean): Promise<void> {
  if (tradingBusy.value) return
  tradingBusy.value = a.id
  tradingErr.value = null
  tradingMsg.value = null
  try {
    const res = await api.accountTradingSet({
      accountId: a.id,
      enabled,
      reason: enabled ? 'panel resume' : 'panel stop',
    })
    if (res.error) throw new Error(res.error)
    tradingMsg.value = enabled
      ? `账户 ${a.id} 已恢复交易${res.persisted ? '（已落盘）' : ''}。`
      : `账户 ${a.id} 已停止开仓——现有持仓继续管理到平完。`
    tradingConfirmOpen.value = false
    tradingConfirmTarget.value = null
    await refresh()
    store.refresh()
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    tradingErr.value = msg
    if (tradingConfirmOpen.value) tradingConfirmErr.value = msg
  } finally {
    tradingBusy.value = null
    setTimeout(() => { tradingMsg.value = null }, 6000)
  }
}

// ── 添加账户（#517）：弹窗（选交易所→填 API）。可选项 = 已注册的行情插件。──
const addOpen = ref(false)
const registeredVenues = computed<string[]>(() => {
  const doc = store.plugins
  if (!doc) return []
  return doc.marketPlugins.map((p) => p.name).filter(Boolean)
})
function onCreated(id: string): void {
  void refresh()
  store.refresh()
}

// ── 删除账户（#517 / #535）：只删不改。所有账户都可删 —— default 不再
//    拦截（绑定只在建户时绑定，删户重建是绑定变更的唯一出口）；内核拒绝
//    面 = 有持仓的账户 + 最后一个账户。确认词 = 账户 id（自由文本门，
//    误触不可能被归咎于双击）。
const removeTarget = ref<AccountRow | null>(null)
const removeOpen = ref(false)
const removeBusy = ref(false)
const removeErr = ref<string | null>(null)

function askRemove(a: AccountRow): void {
  removeErr.value = null
  removeTarget.value = a
  removeOpen.value = true
}

async function doRemove(): Promise<void> {
  const a = removeTarget.value
  if (!a || removeBusy.value) return
  removeBusy.value = true
  removeErr.value = null
  try {
    const res = await api.accountRemove({ id: a.id, reason: 'panel remove' })
    if (res.error) throw new Error(res.error)
    removeOpen.value = false
    removeTarget.value = null
    await refresh()
    store.refresh()
  } catch (e) {
    removeErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    removeBusy.value = false
  }
}

const root = ref<HTMLElement | null>(null)
function onOutside(e: MouseEvent): void {
  if (open.value && root.value && !root.value.contains(e.target as Node)) open.value = false
}
function onKey(e: KeyboardEvent): void {
  if (e.key === 'Escape') open.value = false
}
onMounted(() => {
  document.addEventListener('click', onOutside)
  document.addEventListener('keydown', onKey)
  // First paint: the header button shows the active account's id, so the book
  // is pulled once at mount — not only on open — and the store mirror fills
  // for every page that labels its figures with it.
  void refresh()
})
onBeforeUnmount(() => {
  document.removeEventListener('click', onOutside)
  document.removeEventListener('keydown', onKey)
})
</script>

<template>
  <div ref="root" class="relative">
    <button
      class="flex items-center gap-1.5 rounded-full border border-line bg-panel-2 px-2.5 py-1 text-[11.5px] font-semibold text-fg transition-colors hover:bg-panel-2/80"
      :title="active ? `当前显示账户：${active}` : '账户'"
      @click="toggle"
    >
      <Wallet class="size-3.5" />
      <span class="num max-w-[120px] truncate">{{ active || '账户' }}</span>
      <ChevronDown class="size-3 opacity-60" :class="open ? 'rotate-180' : ''" />
    </button>

    <div
      v-if="open"
      class="absolute right-0 top-full z-30 mt-2 w-80 rounded-xl border border-line bg-panel-solid p-2 shadow-lg"
    >
      <div class="mb-1 flex items-center justify-between px-2 py-1">
        <span class="label-micro">账户账本</span>
        <span class="flex items-center gap-1">
          <button
            class="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10.5px] font-semibold text-primary transition-colors hover:bg-primary/10"
            title="添加账户：选交易所 + 填 API（绑定即启用）"
            @click.stop="addOpen = true"
          >
            <Plus class="size-3" />添加账户
          </button>
          <button
            class="text-faint-fg transition-colors hover:text-fg"
            title="重新加载"
            @click="refresh"
          >
            <RefreshCw class="size-3.5" :class="loading ? 'animate-spin' : ''" />
          </button>
        </span>
      </div>

      <p v-if="error" class="px-2 py-1.5 text-[12px] text-down">{{ error }}</p>
      <p v-if="switchErr" class="px-2 py-1.5 text-[12px] text-down">
        切换失败：{{ switchErr }}
      </p>
      <p v-else-if="loading && !rows.length" class="px-2 py-1.5 text-[12px] text-faint-fg">
        加载中…
      </p>

      <button
        v-for="a in rows"
        :key="a.id"
        class="group flex w-full items-start gap-2 rounded-lg px-2 py-2 text-left transition-colors hover:bg-panel-2 disabled:opacity-60"
        :disabled="a.id === active || switching !== null"
        @click="switchTo(a.id)"
      >
        <span class="mt-0.5 flex size-4 shrink-0 items-center justify-center">
          <Check v-if="a.id === active" class="size-3.5 text-up" />
        </span>
        <span class="min-w-0 flex-1">
          <span class="flex items-center gap-1.5">
            <span class="num truncate text-[13px] font-semibold">{{ a.id }}</span>
            <span
              class="rounded-full px-1.5 py-px text-[10px] font-semibold"
              :style="{ color: statusOf(a.status).tone, background: 'color-mix(in oklab, currentColor 12%, transparent)' }"
            >{{ statusOf(a.status).label }}</span>
            <span
              class="rounded-full px-1.5 py-px text-[10px]"
              :class="a.credentialsLoaded ? 'text-up' : 'text-faint-fg'"
              :title="a.credentialsLoaded ? '凭证已从进程环境加载（§9.4：值永不出内核）' : '凭证未加载'"
            >{{ a.credentialsLoaded ? '凭证已载' : '凭证未载' }}</span>
          </span>
          <span class="mt-0.5 flex items-center gap-3 text-[11.5px] text-faint-fg">
            <span>余额 <span class="num text-fg">{{ money(a.balance) }}</span></span>
            <span>可用 <span class="num text-fg">{{ money(a.available) }}</span></span>
            <span>持仓 <span class="num">{{ a.openPositions }}</span></span>
          </span>
          <span class="mt-0.5 flex items-center gap-1.5 text-[11px]">
            <Link2 class="size-3 shrink-0 text-faint-fg" />
            <span
              class="min-w-0 flex-1 truncate"
              :class="a.venue ? 'text-fg' : 'text-faint-fg'"
              :title="a.venue ? `绑定交易所：${a.venue}（只能交易该 venue 的 token；绑定只在建户时设定）` : '未绑定（可交易任意交易所）'"
            >{{ venueLabel(a.venue) }}</span>
            <button
              class="shrink-0 rounded px-1 text-[10.5px] text-faint-fg transition-colors hover:text-down"
              :title="`删除账户 ${a.id}（不可撤销；有持仓或最后一个账户时内核拒绝）`"
              @click.stop="askRemove(a)"
            ><Trash2 class="size-3" /></button>
          </span>
          <!-- 多账户扇出：交易开关 pill（关 = 拒开仓放平仓）+ 启用集只读行
               （启停唯一入口在策略页启用按钮 —— 分配编辑器退场） -->
          <span class="mt-1 flex items-center gap-1.5 text-[11px]">
            <button
              class="shrink-0 rounded-full px-1.5 py-px text-[10px] font-semibold transition-colors"
              :class="a.tradingEnabled === false
                ? 'bg-down/15 text-down hover:bg-down/25'
                : 'bg-up/15 text-up hover:bg-up/25'"
              :title="a.tradingEnabled === false
                ? '交易已停止：拒开仓、平仓照放；点击恢复交易'
                : '交易中；点击停止（走确认弹窗）'"
              :disabled="tradingBusy !== null"
              @click.stop="askTrading(a)"
            >{{ tradingLabel(a) }}</button>
            <span
              class="min-w-0 flex-1 truncate"
              :class="Array.isArray(a.strategies) && a.strategies.length ? 'text-fg' : 'text-faint-fg'"
              :title="Array.isArray(a.strategies) && a.strategies.length ? `已启用 ${a.strategies.length} 个策略（启停在策略页按账户启用）` : '该账户暂无启用中的策略（启停在策略页按账户启用）'"
            >{{ Array.isArray(a.strategies) && a.strategies.length ? `已启用 ${a.strategies.length} 个策略` : '未启用策略' }}</span>
          </span>
          <span v-if="tradingErr" class="mt-0.5 block text-[10.5px] text-down">{{ tradingErr }}</span>
          <span v-else-if="tradingMsg" class="mt-0.5 block text-[10.5px] text-up">{{ tradingMsg }}</span>
          <span v-if="a.statusReason" class="mt-0.5 block truncate text-[11px] text-down">
            {{ a.statusReason }}
          </span>
        </span>
        <span v-if="switching === a.id" class="mt-1 text-[11px] text-faint-fg">切换中…</span>
      </button>

      <p v-if="!loading && !rows.length && !error" class="px-2 py-1.5 text-[12px] text-faint-fg">
        账本为空（核心未就绪或账户簿未配置）。
      </p>
      <p class="mt-1 border-t border-line px-2 pt-1.5 text-[10.5px] leading-relaxed text-faint-fg">
        切换只改变显示账户（面板唯一可信账户源，accounts.toml 持久化、重启保持），行情/策略/流水视图随之跟随
        （各账户的策略在后台持续运行，切换不启停任何东西）；策略的启停在策略页按账户启用。绑定只在建户时设定
        （绑定即启用），已有账户不改绑——需要换交易所就删户重建；绑定了交易所的账户只能交易该交易所。
      </p>
    </div>

    <AddAccountDialog
      v-model:open="addOpen"
      :venues="registeredVenues"
      @created="onCreated"
    />
    <ConfirmWordDialog
      v-model:open="removeOpen"
      :word="removeTarget?.id ?? ''"
      title="删除账户"
      :lines="[
        `账户 ${removeTarget?.id ?? ''} 将从账户簿删除（accounts.toml 落盘）——不可撤销。`,
        '该账户的现金流水历史保留在账本日志里；有持仓的账户与最后一个账户内核会拒绝。',
        '删除的是当前默认账户时，面板默认账户自动回切剩余账户的第一个。',
      ]"
      action-label="确认删除"
      :busy="removeBusy"
      :error="removeErr"
      @confirm="doRemove"
    />
    <ConfirmWordDialog
      v-model:open="tradingConfirmOpen"
      :word="tradingConfirmTarget?.id ?? ''"
      title="停止交易"
      :lines="[
        `账户 ${tradingConfirmTarget?.id ?? ''} 将停止开仓——已有的挂单与持仓继续由策略管理到平完（关 = 拒开仓、放平仓）。`,
        '恢复交易随时可点同一开关；开关状态持久化进 accounts.toml，重启后保持。',
      ]"
      action-label="确认停止"
      :busy="tradingConfirmBusy"
      :error="tradingConfirmErr"
      @confirm="setTrading(tradingConfirmTarget!, false)"
    />
  </div>
</template>
