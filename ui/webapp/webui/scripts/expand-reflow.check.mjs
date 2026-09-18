/**
 * Regression check for the strategies table's expand/collapse reflow.
 *
 * Measured on the rendered panel before the fix (Chrome Layout Instability API,
 * viewport 1280x720, four strategies):
 *
 *   expanding a row's 拒单原因 detail row moved the header row sideways —
 *   「来源」 left by 11.7px and 24.9px wider, 「下单被拒」 right by 34px,
 *   「原因」 left by 5.9px — and pushed the body rows down 15.8px.
 *
 * Two independent causes, each pinned below:
 *
 *  1. The table used automatic layout, so the widths of all 13 columns were
 *     derived from cell content. Inserting the detail row (`<td colspan="13">`
 *     holding the cause pills and the long 计数口径 paragraph) made the browser
 *     recompute every column from that row's content — a colspan cell influences
 *     the columns under it — which is what slid the headers.
 *
 *  2. The page had no reserved scrollbar gutter, so the 3px of extra document
 *     height the expanded row introduced summoned a scrollbar where none had
 *     been. The 10px it took narrowed the viewport and shifted the KPI grid.
 *
 *   cd ui/webapp/webui && npm run check:expand
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

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

const here = dirname(fileURLToPath(import.meta.url))
const read = (...p) => readFileSync(join(here, '..', ...p), 'utf8')

const page = read('src', 'pages', 'Strategies.vue')
const theme = read('src', 'styles', 'theme.css')

/** Strip CSS and template comments so prose about a rule cannot trip a rule. */
const code = (s) => s.replace(/\/\*[\s\S]*?\*\//g, '').replace(/<!--[\s\S]*?-->/g, '')

const template = code(page)

console.log('strategies table — expand/collapse reflow')

// ── 1. the columns must not depend on cell content ───────────────────────────
check('the performance table is fixed-layout', () => {
  // Automatic layout is the defect: it derives column widths from the widest
  // cell, and an inserted colspan cell is one of the inputs.
  const table = template.match(/<table class="([^"]*)"/)
  assert.ok(table, 'the table must exist')
  assert.match(table[1], /table-fixed/, 'the columns must not be content-derived')
})

check('every column is pinned to an explicit width', () => {
  const cols = template.match(/<colgroup>([\s\S]*?)<\/colgroup>/)
  assert.ok(cols, 'a colgroup must pin the columns')
  const widths = [...cols[1].matchAll(/<col class="([^"]*)"/g)]
  assert.equal(widths.length, 13, `the table has 13 columns, colgroup pins ${widths.length}`)
  for (const [, cls] of widths) {
    assert.match(cls, /w-\[[0-9.]+%\]/, `every col needs a width, got "${cls}"`)
  }
})

check('the pinned widths match the header count', () => {
  // A mismatch is silently tolerated by the browser (missing cols fall back to
  // auto) and would reintroduce the reflow for the unpinned tail.
  const thead = template.match(/<thead>([\s\S]*?)<\/thead>/)
  assert.ok(thead, 'the thead must exist')
  const ths = [...thead[1].matchAll(/<th\b/g)].length
  const cols = [...template.matchAll(/<col class="w-\[/g)].length
  assert.equal(ths, cols, `${ths} headers vs ${cols} pinned columns`)
})

check('a name that outgrows its column truncates instead of pushing', () => {
  // Fixed columns mean the name cell can no longer widen its own column; without
  // this it would either overflow the cell or wrap, and both read as a jump.
  const nameCell = template.match(/<span class="truncate" :title="r\.name">/)
  assert.ok(nameCell, 'the strategy name must truncate and carry a title')
})

// ── 2. the viewport must not change width when content appears ───────────────
check('the page reserves its scrollbar gutter', () => {
  // Without this the scrollbar appears when the expanded row makes the document
  // taller, narrowing the viewport by its width and reflowing everything.
  const html = theme.match(/html\s*\{([^}]*)\}/)
  assert.ok(html, 'the html rule must exist')
  assert.match(html[1], /scrollbar-gutter:\s*stable/, 'the gutter must be reserved in both directions')
})

check('the two fixes are independent', () => {
  // Each guards a different axis: fixed columns stop the horizontal slide, the
  // gutter stops the vertical one. If either were dropped the other would still
  // report green, so assert both together here.
  assert.match(template, /table-fixed/, 'the horizontal fix must be present')
  assert.match(theme, /scrollbar-gutter:\s*stable/, 'the vertical fix must be present')
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
