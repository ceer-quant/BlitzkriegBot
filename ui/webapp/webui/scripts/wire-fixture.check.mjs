/**
 * Issue #842 lane C, gate B4 — the webUI side of the shared-fixture contract:
 * the mock server's golden family fixtures (`ui/ui_kit/tests/fixtures/mock/`)
 * are the ONE artifact both the TUI-side Rust contract tests and the webUI
 * check scripts read. This script pins the webUI's TS wire types to those
 * same bytes — a schema drift anywhere fails BOTH suites.
 *
 * What is asserted (per family, on the families the web UI actually consumes):
 *   1. the fixture file exists and is valid JSON;
 *   2. the webUI's hand-written TS types still carry every camelCase key the
 *      frozen golden bytes speak (a key dropped from a TS interface = the
 *      panel silently stops rendering that field — this check makes that a
 *      red gate, not a silent drift);
 *   3. family 23 (issue #991's additive unverified-exposure family) carries
 *      the `source` field the panel's banner reads.
 *
 * The TS types are read as source text and checked for the member names —
 * this is a drift gate on the DECLARED surface, deliberately not a
 * type-system run (no ts-node dependency in the check lane).
 *
 *   cd ui/webapp/webui && npm run check:wire
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = dirname(fileURLToPath(import.meta.url))
const WEBUI = join(HERE, '..')
const FIXTURES = join(HERE, '..', '..', '..', 'ui_kit', 'tests', 'fixtures', 'mock')
const API = join(WEBUI, 'src', 'api')

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

const fixture = (n) => {
  const raw = readFileSync(join(FIXTURES, `family_${String(n).padStart(2, '0')}.json`), 'utf8')
  return { raw, json: JSON.parse(raw) }
}

/** Every camelCase key the JSON carries, recursively. */
function* keys(v) {
  if (Array.isArray(v)) {
    for (const item of v) yield* keys(item)
    return
  }
  if (v && typeof v === 'object') {
    for (const [k, sub] of Object.entries(v)) {
      yield k
      yield* keys(sub)
    }
  }
}

/** Does the TS source under src/ declare this identifier? The account row
 * the pages render lives in stores/panel.ts (PanelAccountRow); the wire
 * docs live in src/api/*. */
const declares = (name) => {
  const files = ['unified.ts', 'orders.ts', 'accounts.ts', 'risk.ts', 'snapshot.ts',
    'core.ts', 'market.ts', 'audit.ts']
  for (const f of files) {
    try {
      if (readFileSync(join(API, f), 'utf8').includes(name)) return true
    } catch { /* file may not exist */ }
  }
  try {
    return readFileSync(join(WEBUI, 'src', 'stores', 'panel.ts'), 'utf8').includes(name)
  } catch {
    return false
  }
}

console.log('B4 — shared fixture directory is readable from the web side')
check('the shared mock fixture directory exists with all 23 families', () => {
  for (let n = 1; n <= 23; n++) fixture(n)
})

console.log('\nB4 — family 18 (AccountView): every key the pages render is still declared')
check('PanelAccountRow covers the family-18 wire keys the UI consumes', () => {
  const { json } = fixture(18)
  // The panel's row type (stores/panel.ts) renders a subset by design —
  // `dayRealizedUsd`/`updatedAtMs` are read-only wire facts no page renders
  // today. Everything ELSE the golden bytes carry must exist in the TS
  // surface, or a panel field has silently stopped rendering.
  const notRendered = new Set(['dayRealizedUsd', 'updatedAtMs'])
  const missing = [...keys(json)].filter(
    (k) => !notRendered.has(k) && !declares(k),
  )
  assert.deepEqual(missing, [], `TS types lost: ${missing.join(', ')}`)
})

console.log('\nB4 — family 23 (#991 unverified exposure) carries the additive surface')
check('the exposure report row carries source + fallback price', () => {
  const { json } = fixture(23)
  const row = json.entries[0]
  assert.equal(row.source, 'reconciled')
  assert.equal(typeof row.fallbackPrice, 'number')
  assert.equal(typeof row.netPnlUsd, 'number')
})

console.log('\nB4 — family 03 (RiskLimitsResult) keys survive in the web UI types')
check('risk view fields are still declared', () => {
  const { json } = fixture(3)
  const missing = [...keys(json)].filter((k) => !declares(k))
  assert.deepEqual(missing, [], `TS types lost: ${missing.join(', ')}`)
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
