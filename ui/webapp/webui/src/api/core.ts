/**
 * Gateway client core — session/token plumbing and the one `request` helper
 * every endpoint module shares. The domain modules (snapshot/accounts/orders/)
 * … each own their wire types plus their slice of the endpoint surface; the
 * `client.ts` facade re-exports everything and composes the `api` object, so
 * `from '@/api/client'` imports keep working unchanged.
 *
 * Auth: user/password login (`POST /api/login`, set via
 * `BLITZKRIEG_PANEL_USER`/`BLITZKRIEG_PANEL_PASSWORD` on the server) issues a
 * session token kept in localStorage; every /api call carries it as
 * `X-Auth-Token`. (#460: there is no `?token=` hand-off — the query channel is
 * gone server-side because it leaks into logs/history/Referer.)
 *
 * Written for Node's strip-only TypeScript mode too (the panel's `check:*`
 * gates import `src/` directly rather than through Vite): no parameter
 * properties, no enums — plain assignments and unions only.
 */
const LS_KEY = 'blitzkrieg-panel-token'
/** When the current session token was issued (login click), for the 设置 page. */
export const LOGIN_AT_KEY = 'blitzkrieg-panel-login-at'

/** When the current session was issued, or null when unknown/pre-restart. */
export function loginAt(): number | null {
  const raw = localStorage.getItem(LOGIN_AT_KEY)
  const n = raw ? Number(raw) : 0
  return Number.isFinite(n) && n > 0 ? n : null
}

let token = (() => localStorage.getItem(LS_KEY) ?? '')()

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

/**
 * A non-2xx gateway response.
 *
 * Written without a TypeScript parameter property (`constructor(readonly
 * status: …)`) so the class stays loadable under Node's strip-only TypeScript
 * mode — the panel's `check:*` gates import `src/` directly rather than through
 * Vite, and parameter properties are syntax that strip-only refuses to
 * transform. One assignment is a small price for keeping the modules testable
 * as-is.
 */
export class ApiError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.status = status
    this.name = 'ApiError'
  }
}

/**
 * Gateway liveness probe (`GET /api/ping`).
 *
 * The one endpoint that answers without a session — deliberately, and with
 * nothing in the reply but this. Its purpose is to let the panel tell three
 * situations apart that otherwise look identical:
 *
 *   * gateway unreachable → this resolves `null`;
 *   * gateway alive, session gone → resolves with `authRequired: true`;
 *   * gateway alive, session valid → the ordinary `/api/*` calls just work.
 *
 * Without it, a rejected token and a crashed gateway both surface as "network
 * error", which is how a stale `localStorage` token used to strand the panel on
 * an alert with no way back to the login form.
 */
export async function ping(): Promise<{ ok: boolean; authRequired: boolean } | null> {
  try {
    const res = await fetch('/api/ping', { headers: { Accept: 'application/json' } })
    if (!res.ok) return null
    const doc = (await res.json()) as { ok?: boolean; authRequired?: boolean }
    return { ok: doc.ok ?? false, authRequired: doc.authRequired ?? false }
  } catch {
    return null
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
  localStorage.setItem(LOGIN_AT_KEY, String(Date.now()))
}

export async function logout(): Promise<void> {
  try {
    await fetch('/api/logout', { headers: { 'X-Auth-Token': token } })
  } catch {
    /* network errors during logout are non-fatal */
  }
  clearToken()
  localStorage.removeItem(LOGIN_AT_KEY)
  window.location.reload()
}

/**
 * Optional query params are encoded HERE (URLSearchParams), never spliced into
 * `path` by callers: the path stays a static literal, so a stray value can
 * never turn the request into something it was not written to be (the
 * request-forgery shape the push gate refuses).
 */
export async function request<T>(
  path: string,
  init?: RequestInit,
  query?: Record<string, string | number | undefined>,
): Promise<T> {
  const qs = query
    ? new URLSearchParams(
        Object.entries(query)
          .filter(([, v]) => v !== undefined && v !== '')
          .map(([k, v]) => [k, String(v)] as [string, string]),
      ).toString()
    : ''
  const res = await fetch(`/api${path}${qs ? `?${qs}` : ''}`, {
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

