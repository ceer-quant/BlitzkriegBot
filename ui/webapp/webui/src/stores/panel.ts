/**
 * Panel store — snapshot polling + strategy/plugin data, shared by all pages.
 */
import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import {
  api, clearToken, hasToken,
  type Snapshot, type PluginsDoc, type StrategyStatsRow, type TradeRow,
  type CumulativeTradeSummary,
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
  /**
   * 该账户的策略启用集（启停绑定在「账户+策略」级别 —— 账户↔venue 解耦
   * 裁决）。非空 = 逐字集合；空数组 = 显式不跑（INHERIT 已废除，空集不跟
   * 随全局开关 —— issue 552 裁决）；undefined = 老内核缺字段（启用按钮降
   * 级为全局语义）。可见性与它无关：策略列表恒显全部。
   */
  strategies?: string[]
  /**
   * 该账户的轮长族声明（轮次多族 B1d；秒）。空数组 = 未声明限制（许可轴
   * 空集放行）；undefined = 老内核缺字段。面板周期集卡（PR-D）读它。
   */
  rounds?: number[]
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
   * issue 561（2026-10-20 操作者裁决）：display account 升格为面板唯一
   * 账户事实源 —— 切换动作 = 写 display；一切消费点（Header、行情过滤、
   * 策略归属、流水卡）读 display。内核侧缺省解析链同步改为
   * session → display → 进程 default（ipc/server.rs 的
   * effective_default_account），因此账户级聚合读数（trades.summary /
   * engine.stats 带 accountId）也按 display 走。
   *
   * 事实来源按优先级：快照顶层的 displayAccount/displayVenue（内核的
   * display_account 独立事实，2s/15s 轮询每拍带回）> refreshAccounts 镜像
   * （account.list 回执的 displayAccount，快照未到前先有一拍）> null
   * （两者都没到，渲染「未知」而不是猜 `default`）。
   */
  const displayAccount = ref<string>('')
  const displayVenue = ref<string>('')
  const accounts = ref<PanelAccountRow[]>([])

  /**
   * setAccounts：AccountSwitcher 与 refreshAccounts 的镜像入口。行数据
   * （账户簿）始终镜像更新；display id 只在快照尚未说话时兜底采纳
   * （快照在场 = 内核说了话，镜像不得降格覆盖它）。写动作本身只经
   * `setDisplayAccount`。
   */
  function setAccounts(active: string, rows: PanelAccountRow[], mirrorDisplay?: string): void {
    const snapshotInForce = snapshot.value?.displayAccount !== undefined
    if (!snapshotInForce) {
      if (mirrorDisplay) displayAccount.value = mirrorDisplay
      else if (active && !displayAccount.value) displayAccount.value = active
    }
    accounts.value = rows
  }

  /**
   * issue 561：切户的乐观写（唯一写方 AccountSwitcher.switchTo，且只在
   * display.account.set 成功后调用）。venue 由 account.list 行派生（行未
   * 到时空串占位，下一拍快照带回内核读数）。
   */
  function setDisplayAccount(id: string): void {
    displayAccount.value = id
    displayVenue.value = accounts.value.find((a) => a.id === id)?.venue ?? ''
  }

  /**
   * issue 561：display 账户的绑定 venue（''/undefined = 未绑定，可交易任
   * 意交易所）。行情页的 venue 过滤是这里的纯派生 —— 账户切换器就是交易
   * 所切换器，页面不另设 venue 控件（账户↔venue 配对卡的下游跟随者）。
   * 源：快照 displayVenue（内核读数，优先）→ account.list 行派生。
   */
  const activeAccountVenue = computed(() => {
    if (snapshot.value?.displayVenue !== undefined) {
      return snapshot.value?.displayVenue ?? ''
    }
    const id = displayAccount.value
    if (!id) return ''
    return accounts.value.find((a) => a.id === id)?.venue ?? ''
  })

  /** display 账户的行（账户级余额瓦片读它的 balance/available/reserved）。 */
  const activeAccountRow = computed(() => {
    const id = displayAccount.value
    if (!id) return null
    return accounts.value.find((a) => a.id === id) ?? null
  })

  // ── 兼容读数（issue 561 迁移期）──────────────────────────────────────────
  // activeAccount/displayAccount 曾是两套事实（会话格 vs 显示指针）；
  // display 升格唯一可信源后二者同指一格，统一从 displayAccount 出。保留
  // activeAccount 名字让既有消费点逐字不动（页面渐进迁移，不留双源双节拍）。
  const activeAccount = computed<string | null>(() => displayAccount.value || null)

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

  /**
   * PR H (账户筛选修复): the account-scoped aggregates, refreshed beside the
   * snapshot on every poll. `activeAccount` is this panel's session default
   * (AccountSwitcher mirrors it), so the reads and the snapshot describe the
   * SAME account; `null`/'' active = global headline and both stay empty so
   * pages fall back to the snapshot values (老面板行为不变).
   */
  const accountTradeSummary = ref<CumulativeTradeSummary | null>(null)
  const accountStrategyStats = ref<StrategyStatsRow[]>([])

  /**
   * 切户残留窗口（issue 552 审计）：成功切换默认账户后、下一次 refresh 落
   * 地前，账户级读数（trade summary / strategy stats 切片）还是旧账户的。
   * 置空让页面回落全局面读数（单账户部署与老面板的既有行为），而不是把
   * 旧账户的数字渲染成新账户的。snapshot 全局块不清 —— 它本来就无归属。
   */
  function clearAccountScoped(): void {
    accountTradeSummary.value = null
    accountStrategyStats.value = []
  }

  // Prefer engine snapshot strategyStats (richer); fall back to /api/plugins.
  // PR H (账户筛选修复): when an account is active, the counters come from
  // the account-scoped engine.stats read (`accountStrategyStats`) — the same
  // per-NAME rows with THIS account's counter slice. The row SET never
  // shrinks (the kernel folds the full registry either way), and pages must
  // not filter it again (账户↔venue 解耦裁决: 策略列表恒显全部已安装).
  // The merge keeps the mode/compat verdicts from the registry either way.
  const strategyRows = computed<StrategyStatsRow[]>(() => {
    const acc = accountStrategyStats.value
    const st = acc.length ? acc : snapshot.value?.strategyStats
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
            // 启停归属（账户↔venue 解耦合同）：registry 行带 accounts
            // 时把它带过 —— 策略页的「N 个账户启用中」徽标与账户级启停
            // 降级判定都读它。engine.stats 的 fold 行自己也带 accounts，
            // registry 缺席时保留 stats 行的。
            accounts: (match as { accounts?: string[] }).accounts ?? row.accounts,
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

  /**
   * 重拉账户列表（`accounts` 命令，与 AccountSwitcher.refresh 同一面）并把
   * 行镜像进 store。**启用按钮的首选读数就在这里**（account.list 行的
   * `strategies` 集合）：启停写回（strategy.enable / account.strategies.set）
   * 只动内核与这行镜像 —— 不重拉它，按钮翻转后读的仍是内存旧集，要等
   * 手动刷新才见变化（stale-readout 缺陷，2026-10 操作者报告）。
   *
   * 失败不抛：账户列表读不到时保留现有行（比清空好 —— 旧集只是陈旧，
   * 清空会把按钮读数打到降级面）。AccountSwitcher 的 setAccounts 调用方
   * 语义不变（active id 由该组件的切换流写）。
   */
  async function refreshAccounts(): Promise<void> {
    try {
      const out = await api.command('accounts')
      if (!out.ok || !out.data) return
      const doc = out.data as {
        active?: string
        displayAccount?: string
        accounts?: Array<{
          id: string
          name: string
          marketType: string
          status: string
          balance: string | number
          available: string | number
          venue?: string
          strategies?: unknown
          rounds?: unknown
          reserved?: string
        }>
      }
      if (!Array.isArray(doc.accounts) || !doc.accounts.length) return
      const rows: PanelAccountRow[] = doc.accounts.map((a) => ({
        id: a.id,
        name: a.name,
        marketType: a.marketType,
        status: a.status,
        balance: a.balance,
        available: a.available,
        venue: a.venue ?? '',
        strategies: Array.isArray(a.strategies) ? [...(a.strategies as string[])] : undefined,
        rounds: Array.isArray(a.rounds) ? [...(a.rounds as number[])] : undefined,
        reserved: a.reserved ?? '',
      }))
      // issue 561：镜像只供行数据与显示兜底 —— display 账户优先读快照
      // （内核事实），报文的 displayAccount 在场也采纳（快照未到的一拍）；
      // 旧报文的 `active`（会话格语义）不再作为显示源。
      const mirrorDisplay =
        typeof doc.displayAccount === 'string' && doc.displayAccount
          ? doc.displayAccount
          : displayAccount.value
      setAccounts(mirrorDisplay ?? '', rows, mirrorDisplay || undefined)
    } catch {
      // 静默：与 noteOutcome 的降级纪律一致 —— 读数失败不退登录态、不清行。
    }
  }

  /**
   * 轮次周期集写（`account.rounds.set`，轮次多族 B2）：把该账户的轮长族
   * 声明交给内核（空数组 = 不限制）。成功后 refreshAccounts 重拉账户簿
   * （AccountView.rounds 随行带回，卡面读数即新值）；失败抛出 —— 内核的
   * 拒绝（如移除有敞口的族，消息点名族列表）与老内核的 unknown-method 都
   * 由调用方原样透出，这里不吞、不预判敞口。
   */
  async function setAccountRounds(id: string, rounds: number[]): Promise<void> {
    const out = await api.accountRoundsSet({ accountId: id, rounds, reason: 'panel rounds card' })
    if (out.error) throw new Error(out.error)
    await refreshAccounts()
  }

  async function refresh(): Promise<void> {
    if (!hasToken()) return
    loading.value = true
    try {
      // PR H: the account-scoped aggregates ride their own proxy routes
      // beside the snapshot. `acct` absent = 全局面板 —— 两条读数置空，页面
      // 回落 snapshot 值（单账户部署与老面板逐字节一致）。
      // issue 561: `acct` 跟随 display 账户（唯一可信源）——聚合读数与页
      // 面过滤同源，不再有「Header 一样、数字两套」的双节拍。
      const acct = displayAccount.value || undefined
      const [snap, plug, accSummary, accStats, acctList] = await Promise.allSettled([
        api.snapshot(),
        api.plugins(),
        api.tradesSummary({ accountId: acct }),
        api.engineStatsFor({ accountId: acct }),
        refreshAccounts(),
      ])
      if (snap.status === 'fulfilled') {
        snapshot.value = snap.value
        // issue 561: 快照顶层的 displayAccount/displayVenue 是内核的账户
        // 事实 —— 轮询每拍镜像（乐观写在两拍之间由 setDisplayAccount 维
        // 持，快照到达后以内核读数为准）。缺字段（老内核）保留现值。
        const snapDisplay = snap.value.displayAccount
        if (typeof snapDisplay === 'string' && snapDisplay) {
          displayAccount.value = snapDisplay
          displayVenue.value =
            snap.value.displayVenue ??
            accounts.value.find((a) => a.id === snapDisplay)?.venue ??
            ''
        }
        const progress = feedProgress(snap.value.stats?.books, snap.value.stats?.tops)
        if (progress !== lastProgress) {
          lastProgress = progress
          feedAt.value = Date.now()
        }
      }
      if (plug.status === 'fulfilled') plugins.value = plug.value
      accountTradeSummary.value =
        acct && accSummary.status === 'fulfilled' ? accSummary.value.summary ?? null : null
      accountStrategyStats.value =
        acct && accStats.status === 'fulfilled' ? accStats.value.strategies ?? [] : []
      noteOutcome([snap, plug, accSummary, accStats])
      lastUpdated.value = Date.now()
      void acctList
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
    activeAccount, accounts, setAccounts, setDisplayAccount, clearAccountScoped, refreshAccounts, activeAccountVenue, activeAccountRow, setAccountRounds,
    displayAccount, displayVenue,
    accountTradeSummary, accountStrategyStats,
    refresh, acknowledgeSessionReset,
    requestNav, clearNavRequest, requestBlueprintPreload, takeBlueprintPreload,
  }
})
