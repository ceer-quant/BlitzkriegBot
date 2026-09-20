/**
 * Session-failure classification.
 *
 * Lives on its own — and deliberately imports nothing — so the rule can be
 * tested without a gateway, a browser or a store. The rule is small but
 * load-bearing: **a gateway that refused our credentials and a gateway that never
 * answered are different problems, and the panel must not confuse them.**
 *
 *   * `401` — the session is gone. Retrying cannot help; the operator has to log
 *     in again. Leaving them on a "网关连接异常" alert here is a dead end, because
 *     nothing on that screen can fix it.
 *   * anything else (transport error, 5xx, timeout) — the gateway is unreachable
 *     or broken. Offering a login form would be a lie, since logging in cannot
 *     succeed. This is genuinely an error to report, and the token should be
 *     kept: it may be perfectly valid once the gateway is back.
 *
 * Sessions live in the gateway's memory (a `BTreeMap` in `web/mod.rs`), so a
 * restart invalidates every token — which makes the 401 path a routine
 * occurrence rather than an edge case, and is worth telling the operator.
 */

export type SessionVerdict =
  /** Keep going; anything already loaded stays. */
  | { kind: 'ok' }
  /** The session is dead: drop the token, show the login form. */
  | { kind: 'expired' }
  /** The gateway could not be reached: report it, keep the token. */
  | { kind: 'unreachable'; message: string }

/**
 * Does this rejection carry an HTTP status?
 *
 * Structural rather than `instanceof ApiError`: it keeps this module free of
 * imports (so it stays loadable under Node's strip-only TypeScript mode, which
 * the panel's `check:*` gates rely on), and it does not break if the bundle ever
 * ends up holding two copies of the client.
 */
function httpStatus(reason: unknown): number | null {
  if (typeof reason !== 'object' || reason === null) return null
  const status = (reason as { status?: unknown }).status
  return typeof status === 'number' ? status : null
}

/**
 * Judge one poll's outcome.
 *
 * A 401 anywhere in the batch wins over a rejection elsewhere: the batch is
 * `Promise.allSettled([snapshot, plugins])` and both calls go through the same
 * gate, so one 401 means the session is dead regardless of what the other did.
 */
export function classifyOutcome(
  results: readonly PromiseSettledResult<unknown>[],
): SessionVerdict {
  const rejections = results.filter(
    (r): r is PromiseRejectedResult => r.status === 'rejected',
  )
  if (rejections.length === 0) return { kind: 'ok' }

  if (rejections.some((r) => httpStatus(r.reason) === 401)) {
    return { kind: 'expired' }
  }

  const reason = rejections[0].reason
  return {
    kind: 'unreachable',
    message: reason instanceof Error ? reason.message : String(reason),
  }
}

/**
 * Text for the login form when it appeared because a session died, rather than
 * because this is a first visit. Naming the likely cause matters: a gateway
 * restart is the common cause and is invisible from the operator's side.
 */
export const SESSION_EXPIRED_REASON =
  '会话已失效（过期、被登出，或网关重启过）。请重新登录。'

/**
 * The 设置 badge's session adjudication.
 *
 * `ping()` is deliberately sessionless (see `client.ts`): its `authRequired`
 * flag describes the GATEWAY, never our token — a gateway that requires login
 * reports `authRequired: true` even while the session is perfectly fine.
 * Mapping that flag to "expired" is what made the badge claim 会话已过期
 * forever on any credentialed gateway; re-logging in could never change it,
 * because the probe never looked at the token.
 *
 * So reachability (a null ping) and the token's fate are two different
 * questions, and only the second one needs credentials: `probe` is an
 * authenticated call through the client whose `200`/`401` is the token's
 * actual verdict. A newer gateway could report a session state on `ping`
 * itself, but the probe stays the single arbiter — one code path that is
 * correct against any gateway generation, with no field trusted to speak for
 * the token except a real 200/401.
 */
export async function adjudicateSession(
  ping: { ok: boolean; authRequired: boolean; session?: string } | null,
  probe: () => Promise<'valid' | 'expired' | 'unreachable'>,
): Promise<'valid' | 'expired' | 'unreachable'> {
  if (ping === null) return 'unreachable'
  return probe()
}
