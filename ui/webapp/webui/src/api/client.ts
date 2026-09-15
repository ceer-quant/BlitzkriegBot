/**
 * Gateway API client for the Blitzkrieg panel.
 *
 * A single token must be provided via `?token=` URL fragment, `localStorage`,
 * or direct `setToken()` — the gateway (ui_kit_web) issues a one-time 40-hex
 * token printed at startup and every /api call must carry it (401 otherwise).
 */

const LS_KEY = 'blitzkrieg-panel-token'

let token = (() => {
  // URL query first (one-time links), then remembered localStorage.
  const q = new URLSearchParams(window.location.search).get('token')
  if (q) {
    localStorage.setItem(LS_KEY, q)
    // Scrub from the address bar so pastis/Copyscape style tooling doesn't log it.
    history.replaceState(null, '', window.location.pathname)
    return q
  }
  return localStorage.getItem(LS_KEY) ?? ''
})()

export function getToken(): string {
  return token
}

export function setToken(next: string): void {
  token = next.trim()
  if (token) localStorage.setItem(LS_KEY, token)
  else localStorage.removeItem(LS_KEY)
}

export function hasToken(): boolean {
  return token.length > 0
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message)
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    headers: {
      'X-Auth-Token': token,
      'Content-Type': 'application/json',
      ...(init?.headers ?? {}),
    },
  })
  if (res.status === 401) {
    throw new ApiError(401, '需要鉴权：请输入网关启动时打印的一次性 token。')
  }
  if (res.status === 403) {
    throw new ApiError(403, 'Origin 被拒（CORS）：请从面板地址访问。')
  }
  if (!res.ok) {
    throw new ApiError(res.status, `网关返回 ${res.status}`)
  }
  return (await res.json()) as T
}

export const api = {
  snapshot: () => request<Snapshot>('/snapshot'),
  plugins: () => request<PluginsDoc>('/plugins'),
}

// ── wire types (mirror ui_kit core/types.rs) ───────────────────────────────

export interface RejectionCauses {
  [bucket: string]: number
}

export interface StrategyStatsRow {
  name: string
  enabled: boolean
  source: string
  ordersPlaced: number
  ordersRejected: number
  limitRejected: number
  blockedTiming: number
  blockedMomentum: number
  gateExemptedTiming: number
  gateExemptedMomentum: number
  gateExemptions: number
  closedTrades: number
  wins: number
  losses: number
  netPnlUsd: string | number
  rejectionCauses: RejectionCauses | null
}

export interface EngineStats {
  dataTicks?: number
  engineTicks?: number
  signals?: number
  ordersPlaced?: number
  ordersRejected?: number
  placeRejected?: number
  strategyLimitRejected?: number
}

export interface Snapshot {
  connected: boolean
  lastError?: string | null
  stats?: EngineStats
  positions?: unknown[]
  strategies?: unknown[]
  extensions?: unknown[]
  marketPlugins?: unknown[]
}

export interface PluginRow {
  name: string
  kind?: string
  description?: string
  enabled?: boolean
  status?: string
}

export interface PluginsDoc {
  connected: boolean
  strategies: PluginRow[]
  extensions: PluginRow[]
  marketPlugins: PluginRow[]
  marketActive: string | null
  lastError: string | null
}
