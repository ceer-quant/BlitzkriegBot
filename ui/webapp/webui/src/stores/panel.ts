/**
 * Panel store — snapshot polling + strategy/plugin data, shared by all pages.
 */
import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import { api, hasToken, setToken, type Snapshot, type PluginsDoc, type StrategyStatsRow } from '../api/client'

export const usePanelStore = defineStore('panel', () => {
  const snapshot = ref<Snapshot | null>(null)
  const plugins = ref<PluginsDoc | null>(null)
  const error = ref<string | null>(null)
  const loading = ref(false)
  const lastUpdated = ref<number | null>(null)

  const connected = computed(() => snapshot.value?.connected ?? plugins.value?.connected ?? false)
  const strategyRows = ref<StrategyStatsRow[]>([])

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
      // Prefer engine.stats rows (richer) when present; fall back to /api/plugins.
      const stats = snapshot.value?.stats
      if (stats && (stats as unknown as { strategies?: unknown }).strategies) {
        strategyRows.value = (stats as unknown as { strategies: StrategyStatsRow[] }).strategies
      } else if (plugins.value?.strategies) {
        strategyRows.value = plugins.value.strategies.map((s) => ({
          name: s.name,
          enabled: s.enabled ?? false,
          source: s.kind ?? 'builtin',
          ordersPlaced: 0, ordersRejected: 0, limitRejected: 0,
          blockedTiming: 0, blockedMomentum: 0,
          gateExemptedTiming: 0, gateExemptedMomentum: 0, gateExemptions: 0,
          closedTrades: 0, wins: 0, losses: 0, netPnlUsd: 0,
          rejectionCauses: null,
        }))
      }
      error.value = null
      lastUpdated.value = Date.now()
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
    } finally {
      loading.value = false
    }
  }

  function applyToken(token: string): void {
    setToken(token)
    void refresh()
  }

  return {
    snapshot, plugins, error, loading, lastUpdated,
    connected, strategyRows,
    refresh, applyToken,
  }
})
