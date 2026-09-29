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
import { Wallet, ChevronDown, Check, RefreshCw } from 'lucide-vue-next'
import { getToken } from '../api/client'
import { money } from '../lib/format'
import { usePanelStore } from '../stores/panel'

/** Mirror of the core's AccountView (ipc/schema.rs) — camelCase wire. */
interface AccountRow {
  id: string
  name: string
  marketType: string
  status: string
  statusReason?: string
  balance: string | number
  available: string | number
  credentialsLoaded: boolean
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
        <span class="label-micro">账户账本 · E28</span>
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
      </p>
    </div>
  </div>
</template>
