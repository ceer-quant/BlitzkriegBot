/**
 * Regression check for the panel's session-recovery path.
 *
 * The defect this pins: a token in `localStorage` was trusted by *existence*
 * alone — `authed = hasToken() && !!getToken()`. The gateway keeps sessions in
 * memory, so restarting it (or letting a session expire) invalidated every
 * token while the browser kept handing the dead one back. The panel then showed
 * the 网关连接异常 alert over an empty layout, with no route to the login form
 * short of finding the 退出登录 button. Reproduced live during acceptance with a
 * leftover `blitzkrieg-panel-token`.
 *
 * The fix has two halves, both asserted here:
 *
 *   1. A 401 is not an error to report — it is a state to act on. It clears the
 *      dead token and raises `sessionExpired`, a *different* signal from
 *      `error`, so the shell can answer with the login form while still
 *      reporting genuine connection trouble as an error.
 *   2. The token is not spent blindly on boot: `ping()` distinguishes "gateway
 *      unreachable" (null — a real error; the alert is accurate) from "gateway
 *      alive, session dead" (a reply, after which the ordinary call 401s).
 *
 *   cd ui/webapp/webui && npm run check:session
 */
import assert from 'node:assert/strict'

let failures = 0
const check = async (label, fn) => {
  try {
    await fn()
    console.log(`  ok   ${label}`)
  } catch (e) {
    failures++
    console.log(`  FAIL ${label}\n       ${e.message}`)
  }
}

// ── A fake browser, installed before the modules are imported ───────────────
//
// `src/api/client.ts` reads localStorage and `window.location.search` at module
// scope, so the globals have to exist first. This also lets the check drive the
// token lifecycle directly rather than inferring it from outside.
const store = new Map()
globalThis.localStorage = {
  getItem: (k) => (store.has(k) ? store.get(k) : null),
  setItem: (k, v) => store.set(k, String(v)),
  removeItem: (k) => store.delete(k),
  clear: () => store.clear(),
}
globalThis.window = {
  location: { search: '', pathname: '/panel' },
  history: { replaceState: () => {} },
}

/** Queue of canned fetch responses, consumed in order. */
let scripted = []
let calls = []
globalThis.fetch = async (url, init = {}) => {
  calls.push({ url: String(url), method: init.method ?? 'GET', headers: init.headers ?? {} })
  const next = scripted.shift()
  if (!next) throw new Error(`unscripted fetch: ${url}`)
  if (next instanceof Error) throw next
  return {
    ok: next.status >= 200 && next.status < 300,
    status: next.status,
    json: async () => next.body ?? {},
    text: async () => JSON.stringify(next.body ?? {}),
  }
}

const client = await import('../src/api/client.ts')
const { classifyOutcome, SESSION_EXPIRED_REASON } = await import('../src/lib/session.ts')
const LS_KEY = 'blitzkrieg-panel-token'

console.log('client — token storage')

await check('a token round-trips through localStorage', () => {
  client.setToken('  abc123  ') // trimmed on write, so a pasted token with a newline works
  assert.equal(client.getToken(), 'abc123')
  assert.equal(store.get(LS_KEY), 'abc123')
  assert.equal(client.hasToken(), true)
})

await check('clearing a token removes it from storage, not just memory', () => {
  client.clearToken()
  assert.equal(client.hasToken(), false)
  assert.equal(client.getToken(), '')
  assert.equal(store.has(LS_KEY), false, 'a stale key must not survive in localStorage')
})

console.log('client — 401 vs unreachable')

await check('a 401 is an ApiError carrying its status', async () => {
  // The classification keys off `status === 401`; a bare Error would be
  // indistinguishable from a network failure and would be reported as one.
  client.setToken('dead-token')
  scripted = [{ status: 401, body: {} }]
  let caught = null
  try {
    await client.api.snapshot()
  } catch (e) {
    caught = e
  }
  assert.ok(caught instanceof client.ApiError, 'must be an ApiError')
  assert.equal(caught.status, 401)
})

await check('a transport failure is NOT an ApiError', async () => {
  scripted = [new Error('connect ECONNREFUSED')]
  let caught = null
  try {
    await client.api.snapshot()
  } catch (e) {
    caught = e
  }
  assert.ok(caught instanceof Error)
  assert.ok(!(caught instanceof client.ApiError), 'a dead gateway must not look like a dead session')
})

await check('the token travels as X-Auth-Token on every call', async () => {
  client.setToken('live-token')
  calls = []
  scripted = [{ status: 200, body: {} }, { status: 200, body: {} }]
  await client.api.snapshot()
  await client.api.plugins()
  assert.equal(calls.length, 2)
  for (const c of calls) {
    assert.equal(c.headers['X-Auth-Token'], 'live-token', `${c.url} must carry the session`)
  }
})

console.log('client — ping distinguishes the three states')

await check('ping reports the gateway is alive and locked', async () => {
  scripted = [{ status: 200, body: { ok: true, authRequired: true } }]
  assert.deepEqual(await client.ping(), { ok: true, authRequired: true })
})

await check('ping returns null when the gateway does not answer', async () => {
  // The point of the probe: a killed gateway must not be mistaken for a stale
  // session, or the panel would drop to a login form when nothing is listening.
  scripted = [new Error('ECONNREFUSED')]
  assert.equal(await client.ping(), null)
})

await check('ping returns null on a non-200 too', async () => {
  scripted = [{ status: 500, body: {} }]
  assert.equal(await client.ping(), null)
})

await check('ping carries no session, so a dead token cannot block it', async () => {
  client.setToken('dead-token')
  calls = []
  scripted = [{ status: 200, body: { ok: true, authRequired: true } }]
  await client.ping()
  assert.equal(calls.length, 1)
  assert.equal(calls[0].url, '/api/ping')
  assert.equal(calls[0].headers['X-Auth-Token'], undefined, 'ping must not depend on a session')
})

console.log('classification — the rule the shell acts on')

const fulfilled = (v) => ({ status: 'fulfilled', value: v })
const rejected = (r) => ({ status: 'rejected', reason: r })

await check('a clean batch is ok', () => {
  assert.deepEqual(classifyOutcome([fulfilled({}), fulfilled({})]), { kind: 'ok' })
})

await check('a 401 in the batch means the session expired', () => {
  assert.deepEqual(
    classifyOutcome([fulfilled({}), rejected(new client.ApiError(401, 'nope'))]),
    { kind: 'expired' },
  )
  // Either call in the batch is enough — both go through the same gate.
  assert.deepEqual(
    classifyOutcome([rejected(new client.ApiError(401, 'nope')), fulfilled({})]),
    { kind: 'expired' },
  )
})

await check('a 403 is NOT an expired session', () => {
  // 403 is the CORS gate refusing a foreign origin. The token is fine; the
  // request came from the wrong place. Sending the operator to a login form
  // would not fix it — the same request would be refused again.
  const verdict = classifyOutcome([rejected(new client.ApiError(403, 'Origin 被拒')), fulfilled({})])
  assert.equal(verdict.kind, 'unreachable')
  assert.match(verdict.message, /Origin 被拒/)
})

await check('a transport failure is unreachable, not expired', () => {
  const verdict = classifyOutcome([rejected(new Error('connect ECONNREFUSED')), fulfilled({})])
  assert.equal(verdict.kind, 'unreachable', 'a dead gateway must not look like a dead session')
  assert.match(verdict.message, /ECONNREFUSED/)
})

await check('a 5xx is unreachable, not expired', () => {
  assert.equal(classifyOutcome([rejected(new client.ApiError(500, 'boom'))]).kind, 'unreachable')
})

await check('a non-Error rejection still yields a message', () => {
  // Defensive: `throw 'string'` is legal and would otherwise reach the template
  // as `undefined`.
  const verdict = classifyOutcome([rejected('plain string failure')])
  assert.equal(verdict.kind, 'unreachable')
  assert.equal(verdict.message, 'plain string failure')
})

await check('the expired-session wording names the likely cause', () => {
  assert.ok(SESSION_EXPIRED_REASON.length > 0)
  assert.match(SESSION_EXPIRED_REASON, /网关重启/, 'a gateway restart is the common cause and is invisible to the operator')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
