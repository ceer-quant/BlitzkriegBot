/**
 * #427 跨平台呈现 — the「三平台在面板上可见」反向验收, executable.
 *
 * The venue is DATA that reaches the operator's eye: every position and every
 * closed trade renders WHICH platform its market was listed on (the Venue /
 * 场地 column), so a cross-venue pair shows one leg per venue. The reverse
 * acceptance this gate pins: a WebUI that cannot show a Kalshi or predict.fun
 * leg goes red — because the columns below are fed by the kernel's
 * `positions.list` / `trades` payloads (`PositionView.venue` /
 * `TradeView.venue`), a regression that drops the field anywhere on the chain
 * (kernel schema → kit types → web adapter → column) breaks a pinned reading.
 *
 *   cd ui/webapp/webui && npm run check:venues
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const read = (...p) => readFileSync(join(here, '..', ...p), 'utf8')

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

console.log('venue is visible — the three-platform presentation contract')

// The wire contract: both row types carry the venue field, typed optional so
// an older core degrades to the dash instead of a crash.
check('client.ts types carry venue on Position and TradeRow', () => {
  const client = read('src/api/client.ts')
  const pos = client.match(/export interface Position \{[\s\S]*?\n\}/)?.[0] ?? ''
  assert.match(pos, /venue\?\s*:\s*string/, 'Position.venue missing')
  const trade = client.match(/export interface TradeRow \{[\s\S]*?\n\}/)?.[0] ?? ''
  assert.match(trade, /venue\?\s*:\s*string/, 'TradeRow.venue missing')
})

// The presentation: the positions table AND the history table render it.
check('HftPage renders the 场地 column on positions and history tables', () => {
  const page = read('src/pages/HftPage.vue')
  assert.match(page, /<th[^>]*>场地<\/th>/, 'no 场地 header found')
  // Both row loops read the field; two `p.venue || '—'`-shaped bindings exist.
  const bindings = page.match(/\b[pt]\.venue \|\| '—'/g) ?? []
  assert.ok(
    bindings.length >= 2,
    `expected venue bindings in BOTH tables, found ${bindings.length}`,
  )
})

// The reverse-acceptance shape: a per-venue page/branch is FORBIDDEN — venue
// is a column, never a page switch. Any `if venue === 'kalshi'`-shaped
// decision in the UI is the static-venue kernel written in Vue.
check('no UI branch on a concrete venue name (venue is data, not a page)', () => {
  for (const file of ['src/pages/HftPage.vue', 'src/api/client.ts', 'src/lib/onboarding.ts']) {
    const text = read(file)
    const branch = text.match(
      /(==|!=|~=)\s*['"]?(polymarket|kalshi|predictfun|predict_fun)/i,
    )
    assert.equal(branch, null, `${file} branches on a concrete venue`)
  }
})

// The naming: the examples a new operator sees name all three platforms —
// the tour's step ① presents the plugin choice as three-venue.
check('onboarding step ① names all three platforms', () => {
  const lib = read('src/lib/onboarding.ts')
  for (const name of ['Polymarket', 'Kalshi', 'predict.fun']) {
    assert.ok(lib.includes(name), `onboarding does not name ${name}`)
  }
})

if (failures > 0) {
  console.log(`\nRESULT: FAIL — ${failures} check(s) red`)
  process.exit(1)
}
console.log('\nRESULT: PASS — venue reaches both tables, no venue branch, examples name all three')
