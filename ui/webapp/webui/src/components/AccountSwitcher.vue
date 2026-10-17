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
import { Wallet, ChevronDown, Check, RefreshCw, Link2 } from 'lucide-vue-next'
import { getToken, api } from '../api/client'
import { money } from '../lib/format'
import { usePanelStore } from '../stores/panel'

/** Mirror of the core's AccountView (ipc/schema.rs) — camelCase wire.
 *  `venue` 是账户↔交易所绑定（market-plugin 名）；老内核不回该字段 →
 *  undefined，按未绑定（''）渲染。 */
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
        <button
          class="text-faint-fg transition-colors hover:text-fg"
          title="重新加载"
          @click="refresh"
        >
          <RefreshCw class="size-3.5" :class="loading ? 'animate-spin' : ''" />
        </button>
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
          </span>
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
  </div>
</template>
