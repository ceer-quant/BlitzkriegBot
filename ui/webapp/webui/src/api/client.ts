/**
 * Gateway API client for the Blitzkrieg panel.
 *
 * Auth: user/password login (`POST /api/login`, set via
 * `BLITZKRIEG_PANEL_USER`/`BLITZKRIEG_PANEL_PASSWORD` on the server) issues a
 * session token kept in localStorage; every /api call carries it as
 * `X-Auth-Token`. A `?token=…` link is also accepted (session hand-off).
 */

const LS_KEY = 'blitzkrieg-panel-token'

let token = (() => {
  const q = new URLSearchParams(window.location.search).get('token')
  if (q) {
    localStorage.setItem(LS_KEY, q)
    history.replaceState(null, '', window.location.pathname)
    return q
  }
  return localStorage.getItem(LS_KEY) ?? ''
})()

export function getToken(): string {
  return token
}

export function clearToken(): void {
  token = ''
  localStorage.removeItem(LS_KEY)
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

export function setToken(next: string): void {
  token = next.trim()
  if (token) localStorage.setItem(LS_KEY, token)
  else localStorage.removeItem(LS_KEY)
}

/** Exchange user/password for a session token. Throws ApiError on failure. */
export async function login(user: string, password: string): Promise<void> {
  const res = await fetch('/api/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ user, password }),
  })
  if (!res.ok) {
    let msg = '用户名或密码错误'
    try {
      const doc = (await res.json()) as { error?: string }
      if (doc?.error) msg = doc.error
    } catch {
      /* server returned no body; keep default */
    }
    throw new ApiError(res.status, msg)
  }
  const doc = (await res.json()) as { ok: boolean; token?: string; error?: string }
  if (!doc.ok || !doc.token) throw new ApiError(res.status, doc.error ?? '登录失败')
  setToken(doc.token)
}

export async function logout(): Promise<void> {
  try {
    await fetch('/api/logout', { headers: { 'X-Auth-Token': token } })
  } catch {
    /* network errors during logout are non-fatal */
  }
  clearToken()
  window.location.reload()
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
    throw new ApiError(401, '未登录或会话已失效，请重新登录。')
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
  books?: number
  tops?: number
  spots?: number
  rounds?: number
  evaluations?: number
  signals?: number
  placeRejected?: number
  confirmed?: string[]
  strategies?: StrategyStatsRow[]
}

export interface Position {
  asset: string
  direction: string
  entryPrice: number
  currentPrice: number
  unrealizedPct: number
  remainingSec?: number
}

export interface TradeSummary {
  count: number
  net: number
  winRate: number
}

export interface Round {
  slot: number
  ageSec: number
  timeLeftSec: number
  canTrade: boolean
}

export interface Snapshot {
  connected: boolean
  mode?: string
  lastError?: string | null
  stats?: EngineStats
  balance?: { balance: number; reserved: number; available: number } | null
  round?: Round | null
  positions?: Position[]
  trades?: TradeSummary
  strategies?: unknown[]
  extensions?: unknown[]
  marketPlugins?: PluginRow[]
  strategyStats?: StrategyStatsRow[]
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
