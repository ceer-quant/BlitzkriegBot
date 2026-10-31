/**
 * Regression check for the position expiry judgment (issue 733): the 清除
 * button and the force-close verb routing must share ONE predicate, and that
 * predicate must multiply a row's round slot by the ROW's OWN family duration
 * (the kernel resolves the three-state fallback and sends it on the wire) —
 * never by a page-level duration, which belongs to the identity venue's
 * family and is the wrong clock for every other family.
 *
 *   cd ui/webapp/webui && npm run check:position-expiry
 */
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

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

const { positionExpired } = await import('../src/lib/trades.ts')

const here = dirname(fileURLToPath(import.meta.url))
const page = readFileSync(join(here, '..', 'src', 'pages', 'HftPage.vue'), 'utf8')
const tables = readFileSync(join(here, '..', 'src', 'components', 'hft', 'TradesTables.vue'), 'utf8')

console.log('position expiry: one predicate, per-row family clock')

check('a 3600s-family slot is NOT expired at the 900s grid arithmetic', () => {
  // slot 2 of a 3600s family: round ends at (2+1)*3600. A page-level 900
  // clock would call (2+1)*900 long past — the issue 733 bug shape.
  const now = ((2 + 1) * 3600 - 100) * 1000
  const real = Date.now
  Date.now = () => now
  try {
    const p = { roundSlot: 2, roundDurationSec: 3600, remainingSec: 100 }
    assert.equal(positionExpired(p, 900), false, 'own clock says live')
  } finally {
    Date.now = real
  }
})

check('a 3600s-family slot IS expired past its own round end', () => {
  const real = Date.now
  Date.now = () => ((2 + 1) * 3600 + 1) * 1000
  try {
    const p = { roundSlot: 2, roundDurationSec: 3600, remainingSec: -1 }
    assert.equal(positionExpired(p, 900), true)
  } finally {
    Date.now = real
  }
})

check('a missing row duration falls back to the page-level hint', () => {
  const real = Date.now
  // 900 fallback: slot 1 ends at 1800 → 1801 is past; 3600 would not be.
  Date.now = () => 1801_000
  try {
    const p = { roundSlot: 1, remainingSec: 1799 }
    assert.equal(positionExpired(p, 900), true, 'fallback clock decides')
  } finally {
    Date.now = real
  }
})

check('a legacy row without a slot falls back to the countdown sign', () => {
  assert.equal(positionExpired({ remainingSec: 5 }, 900), false)
  assert.equal(positionExpired({ remainingSec: -1 }, 900), true)
  assert.equal(positionExpired({}, 900), false, 'no facts = not expired')
})

check('the button and the verb routing share the one predicate', () => {
  // TradesTables button state calls the local `expired(p)` wrapper...
  assert.match(tables, /function expired\(p: Position\): boolean \{/)
  assert.match(tables, /positionExpired\(p, props\.identityDurSec \?\? 900\)/)
  // ...and HftPage routes the verb through the same imported predicate.
  assert.match(page, /import \{ newestFirst, positionExpired \} from '@\/lib\/trades'/)
  assert.match(page, /positionExpired\(position, identityDurSec\.value \?\? 900\)/)
  // The old divergent form (remainingSec-only routing) must stay dead.
  assert.doesNotMatch(page, /const expired = position\.remainingSec !== undefined/)
})

check('the row carries its own family duration on the wire', () => {
  // gateway projection keeps roundDurationSec (ui_kit 转译层) — the web module
  // is split into domain children, so read the whole `web/` Rust subtree
  // instead of one filename (the #803/#816 gate rule).
  const rustTree = (dir) =>
    readdirSync(dir, { withFileTypes: true })
      .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))
      .flatMap((e) => {
        const p = join(dir, e.name)
        return e.isDirectory()
          ? rustTree(p)
          : e.name.endsWith('.rs')
            ? [readFileSync(p, 'utf8')]
            : []
      })
  const kit = rustTree(join(here, '..', '..', '..', '..', 'ui', 'ui_kit', 'src', 'web')).join('\n')
  assert.match(kit, /"roundDurationSec": p\.round_duration_sec/)
  // the panel interface declares it
  const snap = readFileSync(join(here, '..', 'src', 'api', 'snapshot.ts'), 'utf8')
  assert.match(snap, /roundDurationSec\?: number/)
  // and the predicate prefers it over the fallback
  const lib = readFileSync(join(here, '..', 'src', 'lib', 'trades.ts'), 'utf8')
  assert.match(lib, /p\.roundDurationSec && p\.roundDurationSec > 0/)
})

process.exit(failures ? 1 : 0)
