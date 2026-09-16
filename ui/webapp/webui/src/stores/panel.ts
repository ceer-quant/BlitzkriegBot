/**
 * Panel store — snapshot polling + strategy/plugin data, shared by all pages.
 */
import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import {
  api, hasToken, getToken,
  type Snapshot, type PluginsDoc, type StrategyStatsRow, type TradeRow,
} from '../api/client'
import { dedupeTrades } from '../lib/trades'

export const usePanelStore = defineStore('panel', () => {
  const snapshot = ref<Snapshot | null>(null)
  const plugins = ref<PluginsDoc | null>(null)
  const error = ref<string | null>(null)
  const loading = ref(false)
  const lastUpdated = ref<number | null>(null)

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

  async function refresh(): Promise<void> {
    if (!hasToken()) return
    loading.value = true
    try {
      const [snap, plug] = await Promise.allSettled([
        api.snapshot(),
        api.plugins(),
      ])
      if (snap.status === 'fulfilled') snapshot.value = snap.value
      if (plug.status === 'fulfilled') plugins.value = plug.value
      if (plug.status === 'rejected' && plug.reason instanceof Error) {
        error.value = plug.reason.message
      } else {
        error.value = null
      }
      lastUpdated.value = Date.now()
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
    } finally {
      loading.value = false
    }
  }

  return {
    snapshot, plugins, error, loading, lastUpdated,
    connected, strategyRows, tradeRows,
    refresh,
  }
})
