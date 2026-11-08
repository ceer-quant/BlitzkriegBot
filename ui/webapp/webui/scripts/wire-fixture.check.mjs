/**
 * Issue #842 lane B+A3b — the webUI side of the shared-fixture contract:
 * the mock server's golden family fixtures (`ui/ui_kit/tests/fixtures/mock/`)
 * are the ONE artifact both the TUI-side Rust contract tests and the webUI
 * check scripts read. This script pins the webUI's TS wire types to those
 * same bytes — a schema drift anywhere fails BOTH suites.
 *
 * This is the A3b validator of docs/rust-core/ISSUE-842-ACCEPTANCE.md: the
 * TS surface is not hand-copied and trusted — it is checked against the
 * frozen golden bytes on every run. (The alternative A3b path — generating
 * TS from schema — is deliberately not taken: the surface is small, the
 * gateway speaks HTTP rather than raw JSON-RPC, and a checker needs no
 * generator toolchain in the CI lane.)
 *
 * What is asserted (per family, on the families the web UI actually consumes):
 *   1. the fixture file exists and is valid JSON;
 *   2. the webUI's hand-written TS types still carry every camelCase key the
 *      frozen golden bytes speak (a key dropped from a TS interface = the
 *      panel silently stops rendering that field — this check makes that a
 *      red gate, not a silent drift);
 *   3. the FULL frozen-schema sweep: all 23 families × every key must be
 *      either declared somewhere in webui src or explicitly listed in the
 *      per-family not-consumed allowlist below (each entry with a reason);
 *   4. the allowlist itself cannot rot: an exempted key that disappears
 *      from its family fixture (kernel rename) goes stale → red → forces
 *      re-judgment instead of exempting a ghost forever;
 *   5. family 23 (issue #991's additive unverified-exposure family) carries
 *      the `source` field the panel's banner reads.
 *
 * The TS types are read as source text and checked for the member names —
 * this is a drift gate on the DECLARED surface, deliberately not a
 * type-system run (no ts-node dependency in the check lane).
 *
 *   cd ui/webapp/webui && npm run check:wire
 */
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
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

/** Does the TS source under src/ declare this identifier? The declared wire
 * surface = every file in src/api/*.ts (each domain module owns its slice)
 * plus stores/panel.ts (PanelAccountRow, the rendered account row). */
const declaredSurface = () => {
  let text = ''
  let entries
  try { entries = readdirSync(API, { withFileTypes: true }) } catch { entries = [] }
  for (const e of entries) {
    if (e.isFile() && e.name.endsWith('.ts')) {
      text += readFileSync(join(API, e.name), 'utf8') + '\n'
    }
  }
  try {
    text += readFileSync(join(WEBUI, 'src', 'stores', 'panel.ts'), 'utf8')
  } catch { /* optional */ }
  return text
}

const declares = (name) => declaredSurface().includes(name)

/**
 * Full-sweep allowlist (A3b): families whose keys the webUI does NOT consume
 * today, with the judgment recorded inline so the exemption is reviewable.
 * A stale entry (key renamed/removed kernel-side) fails check #4 below, so
 * this list can only shrink honestly — it cannot accrete dead exemptions.
 *
 * family 01 — transport envelope: the gateway (ui_kit web routes) speaks
 *   HTTP; the webUI never parses raw JSON-RPC envelopes, `jsonrpc` stays a
 *   TUI-side concern (ipc_client decodes it via bk_contract).
 * family 04 — kline.subscribe params: the webUI reads kline.history rows;
 *   the subscribe request is composed by the TUI/feed side.
 * family 05 — orders.place params: the webUI places orders through gateway
 *   POST bodies; `internalKey`/`makerTimeoutMs` are kernel-side idempotency
 *   and maker-window knobs the browser never supplies.
 * family 14 — core.feeQuote result: the fee model readout is consumed by
 *   backtest/exit tooling, not rendered by any web page today.
 * family 16 — orders.reconcile params: kernel-side reconciliation input
 *   (openOrderIds/venueOrderId/tradeId); the webUI never composes it.
 * family 18 — dayRealizedUsd: read-only wire fact no page renders today
 *   (updatedAtMs IS declared in accounts.ts, so it needs no exemption).
 * family 23 — positions.unverified_exposure report rows: the report method
 *   exists on the wire (#991) but the UI presentation is a separate ticket
 *   (INTERFACES §2.2: "UI 展示另单"); until that page exists no TS type
 *   mirrors these rows.
 */
const NOT_CONSUMED = {
  1: ['jsonrpc'],
  4: ['symbols'],
  5: ['internalKey', 'makerTimeoutMs'],
  14: ['feePerShare', 'feePctOfPrice', 'makerFeePerShare', 'modelMatches', 'exponent'],
  16: ['openOrderIds', 'venueOrderId', 'tradeId'],
  18: ['dayRealizedUsd'],
  23: ['costUsd', 'fallbackPrice', 'originalFallbackPrice', 'exitedAtMs',
    'percentageOfTotalCost', 'totalPositions', 'totalShares', 'totalCostUsd',
    'totalPercentageOfClosedCost'],
}

console.log('B4 — shared fixture directory is readable from the web side')
check('the shared mock fixture directory exists with all 23 families', () => {
  for (let n = 1; n <= 23; n++) fixture(n)
})

console.log('\nA3b — full 23-family sweep: every golden key is declared or exempted with a reason')
const allTs = declaredSurface()

for (let n = 1; n <= 23; n++) {
  check(`family ${String(n).padStart(2, '0')}: keys declared or exempted`, () => {
    const { json } = fixture(n)
    const present = [...new Set([...keys(json)])]
    const exempted = NOT_CONSUMED[n] ?? []
    const missing = present.filter((k) => !exempted.includes(k) && !allTs.includes(k))
    assert.deepEqual(missing, [], `TS types lost (add to TS or to NOT_CONSUMED with a reason): ${missing.join(', ')}`)
    // Allowlist rot gate: an exempted key the fixture no longer carries is a
    // stale judgment (kernel renamed/removed it) — re-judge, don't carry it.
    const stale = exempted.filter((k) => !present.includes(k))
    assert.deepEqual(stale, [], `stale NOT_CONSUMED entries for family ${n} (fixture no longer has them): ${stale.join(', ')}`)
  })
}

console.log('\nB4 — family 23 (#991 unverified exposure) carries the additive surface')
check('the exposure report row carries source + fallback price', () => {
  const { json } = fixture(23)
  const row = json.entries[0]
  assert.equal(row.source, 'reconciled')
  assert.equal(typeof row.fallbackPrice, 'number')
  assert.equal(typeof row.netPnlUsd, 'number')
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
