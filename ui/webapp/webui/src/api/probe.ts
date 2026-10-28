/**
 * Session adjudication on top of the snapshot endpoint: the one authenticated
 * probe the shell uses to tell a valid session from an expired one.
 */
import { api } from './client.ts'
import { ApiError } from './core.ts'

/**
 * Adjudicate the CURRENT token with one authenticated call: `200` proves the
 * session works, `401` proves it does not, anything else means no verdict was
 * reachable. `ping` cannot answer this question — it is deliberately
 * sessionless, so its `authRequired` describes the gateway, not the token
 * (mapping the two was what made the 设置 badge claim 会话已过期 forever on
 * gateways that require login).
 */
export async function probeSession(): Promise<'valid' | 'expired' | 'unreachable'> {
  try {
    await api.snapshot()
    return 'valid'
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) return 'expired'
    return 'unreachable'
  }
}
