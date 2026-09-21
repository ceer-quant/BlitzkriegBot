/**
 * Regression check for the panel's trading-safety banners (#236, #232).
 *
 * The defect #236 pins is a *silent* one, so this check is written against the
 * reading rules rather than against a screenshot: the kernel froze trading, the
 * panel drew nothing, and no layer reported a problem — the UI Kit's
 * `EngineStatsView` dropped the keys between the kernel and the page. The Rust
 * side now has a seam test (`core/blitzkrieg_core/tests/engine_stats_view_seam.rs`)
 * that hands the kernel's real payload to that view type; this check covers what
 * the page does with it once it arrives.
 *
 * It also holds the two TITLE fixes in place, because a title is a claim about
 * the data (#232):
 *
 *   - the trading-error banner may not name a source. The slot it reads carries
 *     a venue refusal, a safety-net failure and the kernel's own leg refusals,
 *     so "（venue）" sent operators to the exchange for a local problem.
 *   - the connection banner may not call the gateway's own "core not reachable"
 *     string an engine error. That one sent operators to the engine log when the
 *     panel could not reach the core at all.
 *
 *   cd ui/webapp/webui && npm run check:safety
 */
import assert from 'node:assert/strict'

let failures = 0
const check = (label, fn) => {
  try {
    fn()
    console.log(`  ok   ${label}`)
  } catch (e) {
    failures++
    console.log(`  FAIL ${label}\n       ${e.message}`)
  }
}

const {
  connectionBanner, freezeBanner, lastErrorBanner, selfCheckBanner,
} = await import('../src/lib/safety.ts')

console.log('trading-safety banners — the three states that used to be invisible')

check('a frozen kernel raises the freeze banner with the kernel\'s reason', () => {
  const b = freezeBanner({
    tradingFrozen: { active: true, reason: '5 consecutive venue rejections; last: auth failed' },
  })
  assert.ok(b, 'the freeze banner is the one thing that must never be silent')
  assert.equal(b.tone, 'error', 'a freeze is red, not a warning')
  assert.match(b.body, /consecutive venue rejections/)
})

check('a live kernel raises no freeze banner', () => {
  assert.equal(freezeBanner({ tradingFrozen: { active: false } }), null)
  // An older core omits the key entirely — absence means "unknown", and the
  // panel shows nothing rather than inventing a freeze.
  assert.equal(freezeBanner({}), null)
  assert.equal(freezeBanner(null), null)
})

check('the refusal banner reads the structured record and shows its code', () => {
  const b = lastErrorBanner({
    lastError: { tsMs: 1_000_000_000_000, code: 'NOT_AUTHENTICATED', message: 'auth failed' },
  })
  assert.ok(b)
  assert.equal(b.body, 'auth failed')
  assert.equal(b.code, 'NOT_AUTHENTICATED', 'the kernel\'s code is DATA on screen')
})

check('the refusal banner title claims no source (#232)', () => {
  const structured = lastErrorBanner({
    lastError: { tsMs: 1, code: 'RISK_REJECTED', message: 'position cap reached' },
  })
  const legacy = lastErrorBanner({ lastVenueError: { tsMs: 1, message: 'RiskRejected: position cap' } })
  for (const b of [structured, legacy]) {
    assert.ok(b)
    assert.doesNotMatch(b.title, /venue/i, `title must not pin a source: ${b.title}`)
    assert.doesNotMatch(b.body, /^引擎/, 'and the body is the kernel\'s message, not a claim')
  }
  // The same slot carries non-venue causes; the title must survive all of them.
  assert.equal(structured.title, legacy.title, 'one slot, one title, whatever wrote it')
})

check('an old core falls back to lastVenueError, and only then', () => {
  const b = lastErrorBanner({ lastVenueError: { tsMs: 1, message: 'NotAuthenticated: auth failed' } })
  assert.ok(b, 'a core that predates lastError still has something to show')
  assert.equal(b.code, undefined, 'the legacy string already carries its code as text')
  assert.equal(b.body, 'NotAuthenticated: auth failed')
  // The structured record wins when both are present — it is the same record,
  // and the structured one is the one with the code split out.
  const both = lastErrorBanner({
    lastError: { tsMs: 2, code: 'VENUE_ERROR', message: 'structured' },
    lastVenueError: { tsMs: 2, message: 'VenueError: structured' },
  })
  assert.equal(both.body, 'structured')
  assert.equal(both.code, 'VENUE_ERROR')
})

check('the freeze banner replaces the refusal banner rather than stacking', () => {
  const stats = {
    tradingFrozen: { active: true, reason: 'audit halted' },
    lastError: { tsMs: 1, code: 'VENUE_ERROR', message: 'auth failed' },
  }
  assert.ok(freezeBanner(stats))
  assert.equal(lastErrorBanner(stats, true), null, 'the freeze is the thing to act on')
  assert.ok(lastErrorBanner(stats, false), 'the record itself is not lost when unfrozen')
})

check('a failed self-check lists exactly the failing probes', () => {
  const b = selfCheckBanner({
    selfCheck: {
      ok: false,
      tsMs: 1,
      items: [
        { name: 'venue_balance', ok: false, detail: 'HTTP 401 unauthorized' },
        { name: 'reconcile_sweep', ok: true, detail: 'ok' },
      ],
    },
  })
  assert.ok(b)
  assert.equal(b.tone, 'error')
  assert.deepEqual(b.items.map((i) => i.name), ['venue_balance'], 'the passing probe is not a problem')
  assert.match(b.body, /1 项/)
  // A passing report is not a banner, and neither is a core that never ran one.
  assert.equal(selfCheckBanner({ selfCheck: { ok: true, tsMs: 1, items: [] } }), null)
  assert.equal(selfCheckBanner({}), null)
})

check('the connection banner says what it is: the panel cannot reach the core', () => {
  const b = connectionBanner({ lastError: 'core not reachable on /tmp/blitzkrieg-core.sock' })
  assert.ok(b)
  assert.doesNotMatch(b.title, /引擎/, 'snapshot.lastError is the GATEWAY\'s string, not the engine\'s')
  assert.match(b.title, /连接|内核/)
  assert.equal(connectionBanner({ lastError: null }), null)
})

check('a healthy snapshot raises nothing at all', () => {
  const stats = {
    venueRejected: 0,
    lastError: null,
    lastVenueError: null,
    selfCheck: null,
    tradingFrozen: { active: false },
    reconcile: { consecutiveSweepFailures: 0, freezeThreshold: 3 },
  }
  const snap = { lastError: null, stats }
  assert.equal(connectionBanner(snap), null)
  assert.equal(freezeBanner(stats), null)
  assert.equal(lastErrorBanner(stats), null)
  assert.equal(selfCheckBanner(stats), null)
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
