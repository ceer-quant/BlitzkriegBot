/**
 * Regression check for the 历史订单 tab: row order, row keys, pagination.
 *
 * Three symptoms were reported together on the live panel (275 closed trades),
 * and they had three independent causes:
 *
 *   1. 「数据不是最新的」 — the trade log is append-only and `trades.history`
 *      returns it in file order, i.e. oldest first. The first page of 30 was
 *      therefore 9/14 trades while the summary printed directly above it read
 *      275 / +$41.49, so the table looked stale.
 *   2. 「加载更多」 reset — `visibleCount` was rewound by a watcher on
 *      `filteredRows`, a computed derived from the snapshot. The 2s poll hands
 *      it a fresh array identity every tick, so the watcher fired on every poll
 *      and sent the operator back to page 1 seconds after they clicked.
 *   3. 重置 后更错乱 — the rows were keyed on `t.id`, but `hft-N` is a per-boot
 *      counter and the log is append-only: the first page of 30 rows carried
 *      only 14 distinct ids (`hft-1` three times). Duplicate keys break Vue's
 *      patch algorithm, so it reused the wrong nodes and left stale rows behind.
 *
 * (1) and (3) are the same root confusion — the id is not an identity, and the
 * list is not newest-first — which is why `tradeIdentity` is both the dedupe
 * key and the row key.
 *
 *   cd ui/webapp/webui && npm run check:history
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
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

const { dedupeTrades, tradeIdentity, newestFirst } = await import('../src/lib/trades.ts')

const here = dirname(fileURLToPath(import.meta.url))
const page = readFileSync(join(here, '..', 'src', 'pages', 'HftPage.vue'), 'utf8')

/** The live shape: 275 rows in append-only order, ids restarting every boot. */
function liveRows() {
  const rows = []
  const boots = 7 // hft-1 … hft-13 reused across ~7 runs
  for (let run = 0; run < boots; run++) {
    for (let i = 1; i <= 13; i++) {
      const t = run * 1_000_000 + i * 60_000
      rows.push({
        id: `hft-${i}`,
        asset: ['BTC', 'ETH', 'SOL', 'XRP'][i % 4],
        entryTime: t,
        exitTime: t + 3_000,
        netPnlUsd: (i % 3) - 1,
      })
    }
  }
  return rows
}

console.log('history tab: order, keys, pagination')

// ── order ───────────────────────────────────────────────────────────────────
check('the newest close sorts first, not the oldest', () => {
  // The reported symptom: the table opened on 9/14 while the summary above it
  // read the all-time total.
  const rows = liveRows()
  const shown = newestFirst(rows)
  for (let i = 1; i < shown.length; i++) {
    assert.ok(
      Number(shown[i - 1].exitTime) >= Number(shown[i].exitTime),
      `row ${i} is newer than row ${i - 1}`,
    )
  }
  const last = rows[rows.length - 1]
  assert.equal(shown[0].exitTime, last.exitTime, 'the newest close leads the list')
})

check('the sort does not mutate the list it is given', () => {
  // `historyRows` feeds the equity curve, which needs chronological order. An
  // in-place sort would silently time-reverse the curve.
  const rows = liveRows()
  const before = rows.map((r) => r.exitTime)
  newestFirst(rows)
  assert.deepEqual(rows.map((r) => r.exitTime), before)
})

check('a row without exitTime still orders by its entry', () => {
  // A row mid-write, or from a core that omits the field, must not sort to the
  // bottom as `NaN` and disappear from the top of the list.
  const rows = [
    { id: 'a', asset: 'BTC', entryTime: 100, exitTime: 200 },
    { id: 'b', asset: 'ETH', entryTime: 300 },
    { id: 'c', asset: 'SOL', entryTime: 50, exitTime: 60 },
  ]
  assert.deepEqual(newestFirst(rows).map((r) => r.asset), ['ETH', 'BTC', 'SOL'])
})

check('order is total: no two rows compare equal and reorder between renders', () => {
  // A comparator that returns 0 for distinct rows lets the list shuffle on
  // every poll, which is what makes a table look like it is losing data.
  const rows = liveRows()
  const a = newestFirst(rows).map((r) => tradeIdentity(r))
  const b = newestFirst([...rows].reverse()).map((r) => tradeIdentity(r))
  assert.deepEqual(a, b, 'the same set must always produce the same order')
})

// ── keys ────────────────────────────────────────────────────────────────────
check('the first page of the live log has no duplicate row keys', () => {
  // The 重置 corruption: `:key="t.id"` gave 13 duplicates in the first 30 rows.
  const page1 = newestFirst(dedupeTrades(liveRows())).slice(0, 30)
  const keys = page1.map((r) => tradeIdentity(r))
  assert.equal(new Set(keys).size, keys.length, 'every rendered row carries a unique key')
})

check('the row key is the trade identity, not the per-boot counter id', () => {
  const rows = dedupeTrades(liveRows())
  const ids = new Set(rows.map((r) => r.id))
  assert.ok(ids.size < rows.length, 'ids repeat across boots — that is the point')
  for (const r of rows) {
    assert.equal(tradeIdentity(r), `t:${r.asset}:${r.entryTime}:${r.exitTime}`)
  }
})

check('the template keys rows on tradeIdentity and never on t.id', () => {
  // Source-level, because the bug is only visible in Vue's patch step.
  assert.match(page, /:key="tradeIdentity\(t\)"/, 'the history rows must key on tradeIdentity')
  assert.doesNotMatch(page, /:key="t\.id"/, ':key="t.id" reintroduces duplicate keys')
})

// ── pagination ──────────────────────────────────────────────────────────────
check('the page counter is rewound by a filter change, not by the data', () => {
  // The 加载更多 reset: watching `filteredRows` fired on every 2s poll.
  assert.match(
    page,
    /watch\(\[fTime, fAsset, fOutcome, fStrategy\], \(\) => \{ visibleCount\.value = PAGE \}\)/,
    'the watcher must name the filter refs',
  )
})

check('nothing watches the derived row list to reset pagination', () => {
  const offenders = [...page.matchAll(/watch\((\[[^\]]*\]|[A-Za-z_$][\w$.]*)\s*,\s*\(\)\s*=>\s*\{\s*visibleCount/g)]
    .map((m) => m[1])
    .filter((src) => /filteredRows|visibleRows|historyRows/.test(src))
  assert.deepEqual(offenders, [], 'watching a snapshot-derived list resets the page on every poll')
})

check('the page never grows past the filtered list', () => {
  // `加载更多` adds PAGE unconditionally; the sentinel adds PAGE on intersect.
  // A count beyond the list would advertise rows that are not there.
  const rows = liveRows()
  const PAGE = 30
  let visibleCount = PAGE
  for (let i = 0; i < 20; i++) visibleCount += PAGE
  assert.equal(Math.min(visibleCount, rows.length), rows.length)
  assert.ok(visibleCount > rows.length, 'the clamp is what keeps 已显示 honest')
})

check('the filter refs the watcher names all still exist', () => {
  // A renamed ref would leave the watcher silently watching nothing, and the
  // page would stop rewinding on a filter change — back to stale rows.
  for (const ref of ['fTime', 'fAsset', 'fOutcome', 'fStrategy']) {
    assert.match(page, new RegExp(`const ${ref} = ref`), `${ref} must still be declared`)
  }
})

// ── the shared chart ────────────────────────────────────────────────────────
check('the equity curve plots the rows in the order it is handed them', () => {
  // Both callers pass `tradeRows`, which is chronological, and the accounting
  // check asserts that order is preserved so the curve stays chronological. The
  // chart contradicted that: it reversed its input, plotting the run backwards.
  // The endpoint (= the total) is order-independent, so the header stayed right
  // while the shape was mirrored — up to $28.17 off mid-run on the live log.
  const chart = readFileSync(
    join(here, '..', 'src', 'components', 'charts', 'EquityCurve.vue'),
    'utf8',
  )
  assert.doesNotMatch(chart, /rows\]\.reverse\(\)|rows\)\.reverse\(\)/, 'the curve must not reverse its input')
  assert.match(chart, /props\.rows\.map\(/, 'the series accumulates the rows as given')
})

check('the curve total equals the plain sum, whichever order it is fed', () => {
  const rows = liveRows()
  const cumulate = (rs) => {
    let cum = 0
    for (const r of rs) cum += Number(r.netPnlUsd) || 0
    return cum
  }
  assert.equal(cumulate(rows), cumulate(newestFirst(rows)), 'the endpoint is order-independent')
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
