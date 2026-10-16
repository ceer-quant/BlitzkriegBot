/**
 * Panel store — snapshot polling + strategy/plugin data, shared by all pages.
 */
import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import {
  api, clearToken, hasToken,
  type Snapshot, type PluginsDoc, type StrategyStatsRow, type TradeRow,
} from '../api/client'
import { dedupeTrades } from '../lib/trades'
import { registryStrategyRows } from '../lib/strategy-source'
import { feedProgress } from '../lib/feed'
import { classifyOutcome } from '../lib/session'

/**
 * One account row as `account.list` reports it (E28 §12.1 AccountView) — only
 * the fields pages render. Decimals cross as strings or numbers (the core's
 * Decimal wire allows both); pages format through lib/format.
 */
export interface PanelAccountRow {
  id: string
  name: string
  marketType: string
  status: string
  balance: string | number
  available: string | number
  /** 只读：老内核回读缺该字段时以 '' 渲染（未绑定，可交易任意交易所）。 */
  venue?: string
  /** 账户自有账本的预留/可用（账户级余额瓦片走这里，进程级 ledger 不再是
   *  余额来源 —— E28 §9.5：一个账户一套账本）。 */
  reserved?: string | number
}

export const usePanelStore = defineStore('panel', () => {
  const snapshot = ref<Snapshot | null>(null)
  const plugins = ref<PluginsDoc | null>(null)
  const error = ref<string | null>(null)
  const loading = ref(false)
  const lastUpdated = ref<number | null>(null)

  /**
   * Set when the gateway answered 401: the session is gone — expired, revoked,
   * or issued by a run that has since restarted (the token lives in server
   * memory, so a restart invalidates every session).
   *
   * Deliberately separate from `error`. A dropped session is not a fault to
   * report, it is a state to act on: the shell drops back to the login form.
   * Conflating the two is what used to strand the panel on a 网关连接异常 alert —
   * an alert describes a problem the operator cannot fix from that screen.
   */
  const sessionExpired = ref(false)

  /**
   * When the orderbook counters last moved — the panel's only evidence that
   * market data is still arriving (see `lib/feed.ts` for why nothing else in
   * the snapshot can tell us). Tracked here rather than per-page so every page
   * judges liveness from the same clock, and so it survives navigation.
   */
  const feedAt = ref<number | null>(null)
  let lastProgress = -1

  /**
   * #393: one pending cross-page navigation. Pages only ever SET it; the shell
   * (App.vue) watches, switches its own tab (the tab ids are lib/nav.ts's —
   * the single source the parity/separation gates parse too) and clears it.
   * Page switching stays the shell's business; this is just the wire.
   */
  const navRequest = ref<{ tab: string } | null>(null)

  /**
   * #393: a strategy package the blueprint page should preload (set by the
   * strategy page's 编辑策略 entry). Consumed exactly once, on the blueprint
   * page's mount — a stale request must not overwrite a later manual edit.
   */
  const blueprintPreload = ref<string | null>(null)

  /**
   * E28 (§9.5) 镜像：本面板连接的默认账户（id）与账户簿（account.list 的行）。
   * 唯一写入方是 AccountSwitcher（`accounts` / `account <id>` 走 /api/command，
   * 与 TUI 命令条同一命令面）——这里只是读取视图，页面据此标注「这屏数字属于
   * 哪个账户」。切账户不持久：进程重启回到内核配置的初始账户，所以 readback
   * 为 null 时渲染「未知」而不是猜 `default`。
   */
  const activeAccount = ref<string | null>(null)
  const accounts = ref<PanelAccountRow[]>([])

  function setAccounts(active: string, rows: PanelAccountRow[]): void {
    activeAccount.value = active
    accounts.value = rows
  }

  /**
   * active 账户的绑定 venue（''/undefined = 未绑定，可交易任意交易所）。
   * 行情页的 venue 过滤是这里的纯派生 —— 账户切换器就是交易所切换器，
   * 页面不另设 venue 控件（账户↔venue 配对卡的下游跟随者）。
   */
  const activeAccountVenue = computed(() => {
    const id = activeAccount.value
    if (!id) return ''
    return accounts.value.find((a) => a.id === id)?.venue ?? ''
  })

  /** active 账户的行（账户级余额瓦片读它的 balance/available/reserved）。 */
  const activeAccountRow = computed(() => {
    const id = activeAccount.value
    if (!id) return null
    return accounts.value.find((a) => a.id === id) ?? null
  })

  function requestNav(tab: string): void {
    navRequest.value = { tab }
  }
  function clearNavRequest(): void {
    navRequest.value = null
  }
  function requestBlueprintPreload(name: string): void {
    blueprintPreload.value = name
  }
  function takeBlueprintPreload(): string | null {
    const v = blueprintPreload.value
    blueprintPreload.value = null
    return v
  }

  const connected = computed(() => snapshot.value?.connected ?? plugins.value?.connected ?? false)

  /**
   * Closed-trade rows with cross-run id collisions collapsed. `hft-N` restarts
   * at 1 on every core boot while the trade log is append-only, so a long-lived
   * log holds several rows per id; summing the raw list over-counts trades and
   * skews net PnL and win rate against the kernel's own counters. Every page
   * reads rows through here so they all report the same totals.
   */
  const tradeRows = computed<TradeRow[]>(() => dedupeTrades(snapshot.value?.tradeRows ?? []))

  // Prefer engine snapshot strategyStats (richer); fall back to /api/plugins.
  const strategyRows = computed<StrategyStatsRow[]>(() => {
    const st = snapshot.value?.strategyStats
    const fromStats = (snapshot.value as unknown as { stats?: { strategies?: StrategyStatsRow[] } } | undefined)?.stats?.strategies
    const plug = plugins.value?.strategies
    if ((st && st.length) || (fromStats && fromStats.length)) {
      const rows = st && st.length ? st : fromStats ?? []
      // E27 (§8.3): engine.stats rows predate the mode handshake and carry no
      // compat verdict; the registry rows (`strategy.list`) now do. Merge by
      // name so the table shows one consistent view regardless of which path
      // provided the counters.
      if (plug?.length) {
        const reg = new Map(plug.map((r) => [r.name, r]))
        return rows.map((row) => {
          const match = reg.get(row.name)
          if (!match) return row
          return {
            ...row,
            modes: match.modes ?? null,
            compatible: match.compatible ?? true,
            incompatibleReason: match.incompatibleReason ?? null,
          }
        })
      }
      return rows
    }
    if (plug) {
      // Registry-only rows: no counters and no provenance. `strategySource`
      // reports the source as unknown rather than defaulting to `builtin` —
      // `kind` is a market class, not provenance, and since PR-B the kernel
      // ships zero builtins, so that default mislabelled every external
      // strategy as in-tree. See `lib/strategy-source.ts`.
      return registryStrategyRows(plug)
    }
    return []
  })

  /**
   * Act on one poll's outcome. A 401 discards the token here rather than in the
   * shell, so no page can act on a session that has already been refused.
   * `sessionExpired` then drives the login fallback; see `lib/session.ts` for
   * why a dead session is kept distinct from an unreachable gateway.
   */
  function noteOutcome(results: PromiseSettledResult<unknown>[]): void {
    const verdict = classifyOutcome(results)
    if (verdict.kind === 'expired') {
      clearToken()
      sessionExpired.value = true
      error.value = null
      return
    }
    error.value = verdict.kind === 'unreachable' ? verdict.message : null
  }

  async function refresh(): Promise<void> {
    if (!hasToken()) return
    loading.value = true
    try {
      const [snap, plug] = await Promise.allSettled([
        api.snapshot(),
        api.plugins(),
      ])
      if (snap.status === 'fulfilled') {
        snapshot.value = snap.value
        const progress = feedProgress(snap.value.stats?.books, snap.value.stats?.tops)
        if (progress !== lastProgress) {
          lastProgress = progress
          feedAt.value = Date.now()
        }
      }
      if (plug.status === 'fulfilled') plugins.value = plug.value
      noteOutcome([snap, plug])
      lastUpdated.value = Date.now()
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
    } finally {
      loading.value = false
    }
  }

  /** Called by the shell once the operator has seen the expiry and logged in. */
  function acknowledgeSessionReset(): void {
    sessionExpired.value = false
  }

  return {
    snapshot, plugins, error, loading, lastUpdated,
    connected, strategyRows, tradeRows, feedAt, sessionExpired,
    navRequest, blueprintPreload,
    activeAccount, accounts, setAccounts, activeAccountVenue, activeAccountRow,
    refresh, acknowledgeSessionReset,
    requestNav, clearNavRequest, requestBlueprintPreload, takeBlueprintPreload,
  }
})
