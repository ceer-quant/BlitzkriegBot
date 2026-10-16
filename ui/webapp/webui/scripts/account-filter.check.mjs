/**
 * Regression check for the per-account row filter (`src/lib/account-filter.ts`,
 * E28 §9.5). The properties pinned here:
 *
 *   - rows attributed to the active account stay; rows of other accounts go;
 *   - an UNATTRIBUTED row (accountId empty/absent — a pre-E28 core or a
 *     pre-E28 record in the append-only trade log) always stays: hiding it
 *     would silently falsify the book;
 *   - no active account (switcher not mounted / book not fetched) = no
 *     filtering at all;
 *   - a core that attributes NOTHING (every accountId empty) is treated as
 *     pre-E28: no filtering, and `attributed: false` so pages label the view
 *     范围 instead of 已过滤;
 *   - `attributed` is true exactly when filtering is in effect — the honest
 *     label the pages render;
 *   - order and references are preserved (pages key rows on identity).
 *
 *   cd ui/webapp/webui && npm run check:account
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

const { filterByAccount } = await import('../src/lib/account-filter.ts')

const row = (id, accountId) => ({ id, accountId })

check('rows of the active account stay, others go', () => {
  const rows = [row('a', 'default'), row('b', 'paper'), row('c', 'default')]
  const out = filterByAccount(rows, 'default')
  assert.deepEqual(out.rows.map((r) => r.id), ['a', 'c'])
  assert.equal(out.attributed, true)
})

check('unattributed rows (pre-E28) always stay', () => {
  const rows = [row('a', 'default'), row('old', ''), row('older', undefined)]
  const out = filterByAccount(rows, 'paper')
  // Only the attributed `paper` row and the two unattributed ones survive.
  assert.deepEqual(out.rows.map((r) => r.id), ['old', 'older'])
  assert.equal(out.attributed, true)
})

check('no active account = no filtering, nothing attributed', () => {
  const rows = [row('a', 'default'), row('b', 'paper')]
  const out = filterByAccount(rows, null)
  assert.deepEqual(out.rows.map((r) => r.id), ['a', 'b'])
  assert.equal(out.attributed, false)
})

check('a fully unattributed list reads as pre-E28: nothing filtered', () => {
  const rows = [row('a', ''), row('b', undefined)]
  const out = filterByAccount(rows, 'default')
  assert.deepEqual(out.rows.map((r) => r.id), ['a', 'b'])
  assert.equal(out.attributed, false, 'pages must label this 范围, not 已过滤')
})

check('order is preserved and the input is not mutated', () => {
  const rows = [row('a', 'paper'), row('b', 'default'), row('c', 'paper')]
  const snapshot = JSON.stringify(rows)
  const out = filterByAccount(rows, 'paper')
  assert.deepEqual(out.rows.map((r) => r.id), ['a', 'c'])
  assert.equal(JSON.stringify(rows), snapshot, 'input array must be untouched')
})

check('real wire shapes: trade rows with accountId from the gateway', () => {
  const rows = [
    { id: 'hft-1', accountId: 'default', asset: 'BTC_UP', netPnlUsd: 1.2 },
    { id: 'hft-0', accountId: '', asset: 'BTC_UP', netPnlUsd: -0.3 },
  ]
  const out = filterByAccount(rows, 'default')
  assert.deepEqual(out.rows.map((r) => r.id), ['hft-1', 'hft-0'])
  assert.equal(out.attributed, true)
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
