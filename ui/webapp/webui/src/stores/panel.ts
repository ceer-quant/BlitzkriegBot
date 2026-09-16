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
import { feedProgress } from '../lib/feed'
import { classifyOutcome } from '../lib/session'

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
   * market data is still arriving (see `lib/feed.ts` for why nothing else in the
   * snapshot can tell us). Tracked here rather than per-page so every page judges
   * liveness from the same clock, and so it survives navigation.
   */
  const feedAt = ref<number | null>(null)
  let lastProgress = -1

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
    if (st && st.length) return st
    const fromStats = (snapshot.value as unknown as { stats?: { strategies?: StrategyStatsRow[] } } | undefined)?.stats?.strategies
    if (fromStats && fromStats.length) return fromStats
    const plug = plugins.value?.strategies
    if (plug) {
      return plug.map((s) => ({
        name: s.name,
        enabled: s.enabled ?? false,
        source: s.kind ?? 'builtin',
        ordersPlaced: 0, ordersRejected: 0, limitRejected: 0,
        blockedTiming: 0, blockedMomentum: 0,
        gateExemptedTiming: 0, gateExemptedMomentum: 0, gateExemptions: [],
        closedTrades: 0, wins: 0, losses: 0, netPnlUsd: 0,
        rejectionCauses: null,
      }))
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
    refresh, acknowledgeSessionReset,
  }
})
