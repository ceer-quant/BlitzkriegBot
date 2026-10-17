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
 *  `venue` 是账户↔交易所绑定（market-plugin 名）；老内核不回该字段 →
 *  undefined，按未绑定（''）渲染。`tradingEnabled`/`strategies` 是多账户
 *  扇出面（关 = 拒开仓放平仓；空 strategies = 继承全部全局启用）——老内核
 *  不回 → undefined，开关按开启渲染、分配按继承渲染。 */
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
  /** 策略分配（多账户扇出）；空 = 继承全部全局启用。老内核缺省 = 继承。 */
  strategies?: string[]
  reserved?: string | number
  openPositions: number
  dayRealizedUsd: string | number
}

/** Mirror of AccountListResult: `active` is THIS connection's default (§9.5). */
interface AccountsDoc {
  version: string
  active: string
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
const doc = ref<AccountsDoc | null>(null)

const active = computed(() => doc.value?.active ?? '')
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

/** The switch is session-scoped in the core (§9.5); the panel process owns one
 * IPC connection, so this is "my panel's default" — exactly the operator's
 * intent. A successful switch re-pulls every page through the store. */
async function switchTo(id: string): Promise<void> {
  if (id === active.value || switching.value) return
  switching.value = id
  error.value = null
  try {
    const out = await command(`account ${id}`)
    if (!out.ok) throw new Error(out.message ?? '切换被拒绝')
    await refresh()
    store.refresh()
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
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
    // Mirror into the store: pages read the active id + rows from here to
    // label their figures (余额瓦片 / 挂单卡 / 插件页信息行) without each
    // re-speaking the command surface.
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
        reserved: a.reserved ?? '',
      })),
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

// ── 账户↔交易所绑定（account.venue.get / account.venue.set，§9.1）────────────
// 一个交易所 + 一套 API = 一个账户；绑定后该账户只能交易该 venue 的 token
// （内核拒单守卫 refuse_cross_venue_order）。未绑定（''）= 可交易任意交易所，
// 历史行为。这是插件启用的主控制面：下一批内核把「启用集合」改为 boot 时
// 从账户派生 —— 绑定了某 venue 的账户即启用该 venue 插件。
const venueOptions = ref<string[]>([])
const venueLoading = ref(false)
/** 正在编辑绑定的账户 id（一次展开一行）。 */
const venueEditing = ref<string | null>(null)
const venueDraft = ref('')
const venueBusy = ref(false)
const venueErr = ref<string | null>(null)
const venueMsg = ref<string | null>(null)

/** 已注册插件名 = 可绑定的交易所集合（market.plugin.get 的 registered）。 */
async function loadVenueOptions(): Promise<void> {
  venueLoading.value = true
  try {
    const doc = await api.marketPlugin()
    venueOptions.value = (doc.registered ?? []).map((p) => p.name).filter(Boolean)
  } catch {
    venueOptions.value = []
  } finally {
    venueLoading.value = false
  }
}

function editVenue(a: AccountRow): void {
  if (venueEditing.value === a.id) {
    venueEditing.value = null
    return
  }
  venueEditing.value = a.id
  venueDraft.value = a.venue ?? ''
  venueErr.value = null
  void loadVenueOptions()
}

/**
 * 保存绑定：`account.venue.set` params `{accountId, venue, reason}`，
 * venue 空 = 解绑。走 /api/account-venue/set 专用代理路由（body 即 params，
 * 逐字转发）—— /api/command 是文本动词通道，不认 JSON-RPC 信封。
 */
async function saveVenue(a: AccountRow): Promise<void> {
  if (venueBusy.value) return
  venueBusy.value = true
  venueErr.value = null
  venueMsg.value = null
  const next = venueDraft.value.trim()
  try {
    const res = await api.accountVenueSet({
      accountId: a.id,
      venue: next,
      reason: 'panel',
    })
    if (res.error) throw new Error(res.error)
    venueMsg.value = next
      ? `已绑定：账户 ${a.id} → ${next}${res.persisted ? '（已落盘）' : ''}。`
      : `已解绑：账户 ${a.id} 可交易任意交易所${res.persisted ? '（已落盘）' : ''}。`
    venueEditing.value = null
    await refresh()
    store.refresh()
  } catch (e) {
    venueErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    venueBusy.value = false
    setTimeout(() => { venueMsg.value = null }, 6000)
  }
}

const venueLabel = (v: string | undefined): string =>
  v ? v : '未绑定（可交易任意交易所）'

// ── 多账户扇出：交易开关 + 策略分配 ─────────────────────────────────────────
// 关 = 拒开仓、平仓照放（「关=拒开仓、放平仓」裁决）——exit 照流所以持仓
// 不会被孤儿化，卡片上以「N 笔持仓管理中」披露。走专用代理路由（body 即
// params 逐字转发，同 account-venue/set 的纪律）。

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

/** 策略分配编辑器：一次展开一行；选项 = 已注册策略（strategy 名单）。 */
const strategiesEditing = ref<string | null>(null)
const strategiesDraft = ref<string[]>([])
const strategiesBusy = ref(false)
const strategiesErr = ref<string | null>(null)
const strategiesMsg = ref<string | null>(null)
const strategyOptions = ref<string[]>([])
const strategiesLoading = ref(false)

function strategiesLabel(a: AccountRow): string {
  const list = a.strategies ?? []
  return list.length ? list.join('、') : '继承全局启用'
}

async function loadStrategyOptions(): Promise<void> {
  strategiesLoading.value = true
  try {
    const doc = await api.plugins()
    strategyOptions.value = (doc.strategies ?? [])
      .map((s: { name?: string }) => s.name ?? '')
      .filter(Boolean)
  } catch {
    strategyOptions.value = []
  } finally {
    strategiesLoading.value = false
  }
}

function editStrategies(a: AccountRow): void {
  if (strategiesEditing.value === a.id) {
    strategiesEditing.value = null
    return
  }
  strategiesEditing.value = a.id
  strategiesDraft.value = [...(a.strategies ?? [])]
  strategiesErr.value = null
  void loadStrategyOptions()
}

function toggleDraft(name: string): void {
  const i = strategiesDraft.value.indexOf(name)
  if (i >= 0) strategiesDraft.value.splice(i, 1)
  else strategiesDraft.value.push(name)
}

async function saveStrategies(a: AccountRow): Promise<void> {
  if (strategiesBusy.value) return
  strategiesBusy.value = true
  strategiesErr.value = null
  strategiesMsg.value = null
  try {
    const res = await api.accountStrategiesSet({
      accountId: a.id,
      strategies: strategiesDraft.value,
      reason: 'panel assign',
    })
    if (res.error) throw new Error(res.error)
    const kept = res.keptExitsOnly ?? []
    strategiesMsg.value = kept.length
      ? `分配已保存；${kept.join('、')} 仍持仓，以只平不开保活到平完。`
      : `分配已保存${res.persisted ? '（已落盘）' : ''}。`
    strategiesEditing.value = null
    await refresh()
    store.refresh()
  } catch (e) {
    strategiesErr.value = e instanceof Error ? e.message : String(e)
  } finally {
    strategiesBusy.value = false
    setTimeout(() => { strategiesMsg.value = null }, 8000)
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

// ── 删除账户（#517）：只删不改。default 与有持仓的账户由内核拒绝；
//    确认词 = 账户 id（自由文本门，误触不可能被归咎于双击）。
const removeTarget = ref<AccountRow | null>(null)
const removeOpen = ref(false)
const removeBusy = ref(false)
const removeErr = ref<string | null>(null)

function askRemove(a: AccountRow): void {
  if (a.id === 'default') return
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
      :title="active ? `当前默认账户：${active}` : '账户'"
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
              class="truncate"
              :class="a.venue ? 'text-fg' : 'text-faint-fg'"
              :title="a.venue ? `绑定交易所：${a.venue}（只能交易该 venue 的 token）` : '未绑定（可交易任意交易所）'"
            >{{ venueLabel(a.venue) }}</span>
            <button
              class="ml-auto shrink-0 rounded px-1 text-[10.5px] text-faint-fg transition-colors hover:text-fg"
              :title="'编辑账户绑定的交易所'"
              @click.stop="editVenue(a)"
            >{{ venueEditing === a.id ? '收起' : '绑定' }}</button>
            <button
              v-if="a.id !== 'default'"
              class="shrink-0 rounded px-1 text-[10.5px] text-faint-fg transition-colors hover:text-down"
              :title="`删除账户 ${a.id}（不可撤销；有持仓时内核拒绝）`"
              @click.stop="askRemove(a)"
            ><Trash2 class="size-3" /></button>
          </span>
          <!-- 多账户扇出：交易开关 pill（关 = 拒开仓放平仓）+ 策略分配行 -->
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
              :class="(a.strategies?.length ?? 0) ? 'text-fg' : 'text-faint-fg'"
              :title="`策略分配：${strategiesLabel(a)}`"
            >{{ strategiesLabel(a) }}</span>
            <button
              class="shrink-0 rounded px-1 text-[10.5px] text-faint-fg transition-colors hover:text-fg"
              title="编辑该账户跑的策略组合"
              @click.stop="editStrategies(a)"
            >{{ strategiesEditing === a.id ? '收起' : '分配' }}</button>
          </span>
          <span v-if="strategiesEditing === a.id" class="mt-1.5 block" @click.stop>
            <span class="flex flex-wrap gap-1">
              <button
                v-for="s in strategyOptions"
                :key="s"
                class="rounded-full px-1.5 py-px text-[10.5px] font-semibold transition-colors"
                :class="strategiesDraft.includes(s)
                  ? 'bg-primary/20 text-primary'
                  : 'bg-panel-2 text-faint-fg hover:text-fg'"
                :disabled="strategiesBusy || strategiesLoading"
                @click="toggleDraft(s)"
              >{{ s }}</button>
            </span>
            <span class="mt-1 flex items-center gap-1.5">
              <button
                class="shrink-0 rounded bg-panel-2 px-2 py-1 text-[11px] font-semibold text-fg transition-colors hover:bg-panel-2/80 disabled:opacity-50"
                :disabled="strategiesBusy || strategiesLoading"
                @click="saveStrategies(a)"
              >{{ strategiesBusy ? '保存中…' : '保存分配' }}</button>
              <span class="text-[10.5px] text-faint-fg">不勾 = 继承全部全局启用</span>
            </span>
            <span v-if="strategiesLoading" class="mt-0.5 block text-[10.5px] text-faint-fg">策略清单加载中…</span>
            <span v-if="strategiesErr" class="mt-0.5 block text-[10.5px] text-down">{{ strategiesErr }}</span>
          </span>
          <span v-if="tradingErr && venueEditing !== a.id" class="mt-0.5 block text-[10.5px] text-down">{{ tradingErr }}</span>
          <span v-else-if="tradingMsg" class="mt-0.5 block text-[10.5px] text-up">{{ tradingMsg }}</span>
          <span v-if="strategiesMsg && strategiesEditing !== a.id" class="mt-0.5 block text-[10.5px] text-up">{{ strategiesMsg }}</span>
          <span v-if="venueEditing === a.id" class="mt-1.5 block" @click.stop>
            <span class="flex items-center gap-1.5">
              <select
                v-model="venueDraft"
                class="min-w-0 flex-1 rounded border border-line bg-panel-2 px-1.5 py-1 text-[11.5px] text-fg"
                :disabled="venueLoading || venueBusy"
              >
                <option value="">未绑定（可交易任意交易所）</option>
                <option v-for="v in venueOptions" :key="v" :value="v">{{ v }}</option>
              </select>
              <button
                class="shrink-0 rounded bg-panel-2 px-2 py-1 text-[11px] font-semibold text-fg transition-colors hover:bg-panel-2/80 disabled:opacity-50"
                :disabled="venueBusy || venueLoading"
                @click="saveVenue(a)"
              >{{ venueBusy ? '保存中…' : '保存' }}</button>
            </span>
            <span v-if="venueLoading" class="mt-0.5 block text-[10.5px] text-faint-fg">交易所清单加载中…</span>
            <span v-if="venueErr" class="mt-0.5 block text-[10.5px] text-down">{{ venueErr }}</span>
            <span v-else-if="venueMsg" class="mt-0.5 block text-[10.5px] text-up">{{ venueMsg }}</span>
          </span>
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
        切换只改变本面板连接的默认账户（§9.5 会话级）；其他连接不受影响。
        绑定交易所的账户只能交易该交易所（插件启用将随账户派生——绑定即启用）。
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
        '该账户的现金流水历史保留在账本日志里；default 账户与有持仓的账户内核会拒绝。',
        '删除的是当前默认账户时，面板默认账户自动回切 default。',
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
