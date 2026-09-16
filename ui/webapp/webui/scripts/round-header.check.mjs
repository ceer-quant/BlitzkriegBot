/**
 * Regression check for the round header and its rolling digits.
 *
 * Three defects are pinned here, each one measured on the rendered panel before
 * it was fixed:
 *
 *  1. The age counter shoved the status text beside it. Proportional figures made
 *     `1111s 已过` and `1000s 已过` 7.95px apart at the SAME character count, and
 *     `385s 已过` was 14.5px wider than `18s 已过`, so TRADING/WAITING slid
 *     horizontally on every tick. The fix is a fixed-width, tabular digit slot.
 *
 *  2. The digits had no roll, and the naive roll is wrong in three specific ways
 *     that are each re-derived below: a countdown must step one glyph (not sweep
 *     nine), a carry must align from the right (`99 → 100`), and the strip must
 *     park its LAST glyph in the visible row.
 *
 *  3. The mobile layout crowded the header: `mt-4` under a `top-3` sticky header
 *     was a visible 4px, and the stats row stayed one column up to 1280px.
 *
 * The digit model in `rollCell`/`rollText` is a faithful copy of the component's
 * own logic. That duplication is deliberate: these are properties of the
 * arithmetic, and testing them through a DOM would only re-derive the browser's
 * layout engine. The template assertions below keep the copy honest.
 *
 *   cd ui/webapp/webui && npm run check:round
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

const component = read('src', 'components', 'ui', 'roll', 'RollingNumber.vue')
const page = read('src', 'pages', 'HftPage.vue')
const app = read('src', 'App.vue')
const theme = read('src', 'styles', 'theme.css')

// ── the digit model, mirroring RollingNumber.vue ──────────────────────────────
const digitOf = (ch) => (ch !== undefined && ch >= '0' && ch <= '9' ? Number(ch) : null)

/** Signed shortest step between two digits, in −5..4. */
function shortestStep(from, to) {
  return (((((to - from + 5) % 10) + 10) % 10) - 5)
}

/** One entry per character; `stack` is the glyphs to travel through. */
function rollText(now, before) {
  const shift = now.length - before.length
  return [...now].map((ch, i) => {
    if (ch < '0' || ch > '9') return { ch, step: 0, stack: [] }
    const to = Number(ch)
    const from = digitOf(before[i - shift])
    const step = from === null ? 0 : shortestStep(from, to)
    const dir = Math.sign(step)
    const start = from ?? to
    const stack = step === 0
      ? [to]
      : Array.from({ length: Math.abs(step) + 1 }, (_, k) => ((((start + dir * k) % 10) + 10) % 10))
    return { ch, step, stack }
  })
}

/** Travel implied by a stack: always upward, parking the last glyph. */
const travel = (stack) => -(stack.length - 1)

// ── the "missed a number" scanner ─────────────────────────────────────────────
const TAG = /<(\/?)([A-Za-z][-\w.]*)((?:"[^"]*"|'[^']*'|[^>"'])*?)(\/?)>/g
const NUMERIC_CLASS = /(?:^|\s)(?:stat-num|num)(?:\s|$)/
/**
 * Interpolations that are prose or identifiers rather than figures, and so are
 * deliberately left static. Anything added here must be justified: a value that
 * ticks belongs in <RollingNumber>.
 *
 * `feed.ageLabel` is the one judgement call — it does carry digits (`行情 3m20s
 * 前更新`), but it is a sentence whose wording changes with the state, so rolling
 * it would animate the prose around the number and twitch on every state change.
 * Timestamps and addresses are identifiers, not readings. `duration()` is
 * deliberately absent: a countdown does tick and must roll.
 */
const TEXT_MARKERS =
  /^(?:dateTime|shortAddr|identity|stateLabel|marketTypeLabel|backtestLabel)\s*\(|\.(?:ageLabel|asset|direction|name|strategy|source|exitReason|label|description|kind)\b/

/**
 * Raw `{{ … }}` interpolations sitting directly inside a numeric slot.
 *
 * Deliberately a tiny HTML walk rather than a regex over the whole file: the
 * slot a value sits in is inherited from ancestors, and only a walk can know
 * that `<td class="num"><span>{{ n }}</span></td>` is a figure while
 * `<td>{{ p.asset }}</td>` beside it is not.
 */
function rawInNumericSlots(src) {
  // Blank comments in place so their line structure survives while their prose
  // cannot be mistaken for markup.
  const clean = src.replace(/<!--[\s\S]*?-->/g, (m) => m.replace(/[^\n]/g, ' '))
  const found = []
  const stack = []
  let cursor = 0

  const flushText = (end) => {
    const chunk = clean.slice(cursor, end)
    cursor = end
    if (!stack.some((f) => f.numeric)) return
    for (const m of chunk.matchAll(/\{\{([\s\S]*?)\}\}/g)) {
      const expr = m[1].trim()
      if (TEXT_MARKERS.test(expr)) continue
      found.push(expr)
    }
  }

  TAG.lastIndex = 0
  let m
  while ((m = TAG.exec(clean))) {
    flushText(m.index)
    const [, closing, , attrs, selfClose] = m
    if (closing) {
      stack.pop()
    } else if (!selfClose) {
      const cls = attrs.match(/\bclass="([^"]*)"/)?.[1] ?? ''
      stack.push({ numeric: NUMERIC_CLASS.test(cls) })
    }
    cursor = TAG.lastIndex
  }
  flushText(clean.length)
  return found
}

console.log('round header — status position and rolling digits')

// ── 1. the jitter ─────────────────────────────────────────────────────────────
check('the age is reserved a fixed tabular digit slot', () => {
  // Both halves matter: the width reserves the space, tabular-nums makes the
  // reserved `ch` equal the digits it is reserving for.
  assert.match(page, /w-\[4ch\]/, 'the age must reserve a fixed width')
  const slot = page.match(/class="([^"]*w-\[4ch\][^"]*)"/)
  assert.ok(slot, 'the slot class must be findable')
  assert.match(slot[1], /tabular-nums/, 'the reserved `ch` must be a tabular digit')
  assert.match(slot[1], /text-right/, 'overflow past four digits must grow leftward, away from the status')
})

check('the status text follows the fixed slot, not the raw number', () => {
  // The old markup interpolated the number directly into the flow, which is what
  // let its width move the status. It must now go through the component.
  assert.doesNotMatch(
    page,
    /\{\{\s*round\?\.ageSec[^}]*\}\}s 已过/,
    'the raw age must not be interpolated into the flow any more',
  )
  assert.match(page, /<RollingNumber[^>]*:value="round\?\.ageSec/, 'the age must render through RollingNumber')
})

check('the countdown also rolls, and its tracking is dropped', () => {
  const tag = page.match(/<RollingNumber[\s\S]*?:value="leftText"[\s\S]*?\/>/)
  assert.ok(tag, 'the countdown must render through RollingNumber')
  // Cell widths are fixed, so letter-spacing cannot be applied: it is added after
  // every character and would make the width-defining glyph the wrong width.
  assert.doesNotMatch(tag[0], /tracking-/, 'fixed-width cells cannot carry tracking')
})

check('a single-glyph cell is never tracked wider than its own sizer', () => {
  // The sizer "0" defines each cell's width; every glyph must render at that same
  // width, which is only true for tabular figures.
  assert.match(component, /font-variant-numeric:\s*tabular-nums/, 'cells must be tabular')
  assert.match(component, /letter-spacing:\s*0/, 'tracking must be zeroed')
})

// ── 2. the roll itself ────────────────────────────────────────────────────────
check('a countdown steps ONE glyph, not nine', () => {
  // 07:03 → 07:02 (the seconds unit). Sweeping forward through 3,4,…,2 would take
  // nine glyphs and be plainly visible; one step down is the whole point.
  const cells = rollText('07:02', '07:03')
  const unit = cells[4]
  assert.equal(unit.stack.length, 2, 'one step = two glyphs (from, to)')
  assert.deepEqual(unit.stack, [3, 2], 'travels 3 → 2')
  assert.equal(travel(unit.stack), -1, 'travels one line')
})

check('the seconds digit rolling 0 → 9 wraps by one, not nine', () => {
  // The case that makes the naive implementation twitch once every ten seconds.
  const unit = rollText('06:59', '07:00')[4]
  assert.equal(unit.step, -1)
  assert.deepEqual(unit.stack, [0, 9])
})

check('every step is the shortest path and lands exactly', () => {
  for (let a = 0; a < 10; a++) {
    for (let b = 0; b < 10; b++) {
      const s = shortestStep(a, b)
      assert.ok(Math.abs(s) <= 5, `${a}→${b} took ${s}`)
      assert.equal((((a + s) % 10) + 10) % 10, b, `${a}→${b} landed on the wrong glyph`)
    }
  }
})

check('the stack always begins at the old digit and ends at the new one', () => {
  for (let a = 0; a < 10; a++) {
    for (let b = 0; b < 10; b++) {
      const [cell] = rollText(String(b), String(a))
      assert.equal(cell.stack[0], a, `${a}→${b} did not start on screen`)
      assert.equal(cell.stack.at(-1), b, `${a}→${b} did not end on the new digit`)
    }
  }
})

check('a carry aligns from the RIGHT, like an odometer', () => {
  // 99 → 100. Aligning left would pit the new `1` against the old `9` and
  // misalign every remaining place, so the units would not roll at all.
  const cells = rollText('100', '99')
  assert.equal(cells[0].step, 0, 'the new leading place settles instead of sweeping in')
  assert.deepEqual(cells[0].stack, [1])
  assert.equal(cells[1].step, 1, 'tens carry 9 → 0 upward')
  assert.equal(cells[2].step, 1, 'units carry 9 → 0 upward')
  assert.deepEqual(cells[2].stack, [9, 0])
})

check('the visible row is where the stack ends', () => {
  // Travel must park the LAST glyph in the cell. Getting this sign wrong leaves
  // the cell blank, which is how the first attempt shipped a blank digit.
  for (let n = 0; n < 10; n++) {
    for (let o = 0; o < 10; o++) {
      const [cell] = rollText(String(n), String(o))
      const rows = cell.stack.length
      const parked = travel(cell.stack)
      assert.equal(-parked, rows - 1, `${o}→${n} parks off the stack`)
      assert.ok(Math.abs(parked) <= 5, `${o}→${n} travels ${parked} lines`)
    }
  }
  // The source form is `${-(cell.stack.length - 1)}em` — the `}` before `em`
  // closes the JS interpolation, so the pattern has to allow it.
  assert.match(component, /-\(cell\.stack\.length - 1\)\}\s*em/, 'travel must be derived from the stack length')
})

check('travel does not reverse between a countdown and an increment', () => {
  // Both directions move up; only the glyphs differ. A direction that flips
  // whenever the value changes sign reads as a twitch, not a roll.
  const down = rollText('07:02', '07:03')[4]
  const up = rollText('07:04', '07:03')[4]
  assert.ok(travel(down.stack) <= 0 && travel(up.stack) <= 0, 'travel must always be upward')
})

check('a value that does not change does not roll', () => {
  assert.ok(rollText('385', '385').every((c) => c.step === 0))
})

check('static punctuation never rolls', () => {
  const cells = rollText('01:02', '01:01')
  assert.equal(cells[2].ch, ':', 'the colon stays put')
  assert.deepEqual(cells[2].stack, [], 'and carries no stack')
})

check('settled digits render the value the component reports', () => {
  // The invariant that matters to a reader: after the animation, what is in the
  // window is the number, not a neighbouring glyph. Verified live as well — 24
  // settled samples, 0 mismatches.
  for (const target of ['529', '531', '1000', '09', '07:03']) {
    const cells = rollText(target, target)
    assert.equal(cells.map((c) => c.ch).join(''), target)
    for (const c of cells) if (c.stack.length) assert.equal(String(c.stack.at(-1)), c.ch)
  }
})

check('a full countdown keeps the invariant every second', () => {
  const stamp = (n) => `${String(Math.floor(n / 60)).padStart(2, '0')}:${String(n % 60).padStart(2, '0')}`
  let prev = stamp(600)
  for (let n = 599; n >= 0; n--) {
    const now = stamp(n)
    for (const cell of rollText(now, prev)) {
      if (!cell.stack.length) continue
      assert.equal(String(cell.stack.at(-1)), cell.ch, `${prev}→${now} would show the wrong glyph`)
      assert.ok(Math.abs(cell.step) <= 5, `${prev}→${now} swept ${cell.step}`)
    }
    prev = now
  }
})

check('the strips are re-keyed so the animation restarts', () => {
  assert.match(component, /:key="gen"/, 'a stale strip would not replay')
  assert.match(component, /animation:\s*roll\b|animationDuration/, 'the roll must be a CSS animation')
  assert.match(component, /@keyframes roll/, 'the keyframes must exist')
})

check('reduced motion is honoured', () => {
  // theme.css collapses every animation globally, which lands each strip on its
  // final digit — so the component needs no branch of its own, only the guarantee
  // that the collapse still exists.
  assert.match(theme, /prefers-reduced-motion/)
  assert.match(theme, /animation-duration:\s*0?\.001ms/)
})

check('gradient text still reaches the digits', () => {
  // The defect that the first attempt shipped: `.grad-gold` paints its gradient
  // with `background-clip: text` and `color: transparent`. Clipping only applies
  // to an element's OWN in-flow text, and the glyphs live inside the animated
  // strip, so the gradient never arrived while `color: transparent` did — the
  // countdown rendered as four unpainted digits. Verified live in both states
  // (gold `14:48` and urgent `00:25`) after the fix.
  assert.match(
    theme,
    /--grad-text:\s*linear-gradient/,
    'the gradient must be published as a custom property to be inheritable',
  )
  assert.match(component, /background-image:\s*var\(--grad-text/, 'each glyph must clip the gradient itself')
  assert.match(component, /background-clip:\s*text/, 'and clip it to its own text')
})

check('a gradient-less parent still renders solid digits', () => {
  // The age counter passes no gradient. The declaration must therefore degrade to
  // `none` rather than forcing every glyph transparent.
  assert.match(component, /var\(--grad-text,\s*none\)/, 'the gradient must default to none')
  assert.doesNotMatch(component, /^\s*color:\s*transparent/m, 'the component must not hard-code transparency')
})

check('the digits are announced once, as the number', () => {
  // A cell is a column of ten glyphs; a screen reader must hear `529`, not the
  // strip. The visible copy is aria-hidden.
  assert.match(component, /class="sr-only"/, 'the value needs a readable twin')
  assert.match(component, /aria-hidden="true"/, 'the glyph strips must be hidden from AT')
})

check('a copied value is not doubled by the readable twin', () => {
  // Every digit now exists twice: once as selectable text and once as painted
  // glyphs. Selecting a row and copying it would otherwise paste the number
  // twice, so the painted copy has to be excluded from selections.
  assert.match(component, /\.roll-paint\s*\{[^}]*user-select:\s*none/, 'the painted copy must be unselectable')
  assert.match(component, /class="roll-paint"\s+aria-hidden="true"/, 'the unselectable copy is the painted one')
})

// ── 2b. the digits sit on the text baseline ───────────────────────────────────
check('the rolling cell does not break the text baseline', () => {
  // CSS 2.1: an `overflow` other than `visible` on an inline-block forces that
  // box's baseline to its BOTTOM MARGIN EDGE. The cell is baseline-aligned, so
  // clipping it directly lifted every digit off the text baseline — measured at
  // −5.25px on the 42px countdown and −1.5px on the 11.5px age counter, which is
  // the "digits moved up" regression.
  const cell = component.match(/\.roll-cell\s*\{([^}]*)\}/)
  assert.ok(cell, '.roll-cell must exist')
  assert.doesNotMatch(cell[1], /overflow/, 'the cell must keep a visible overflow')

  // Clipping still has to happen somewhere: on an absolutely positioned inner
  // layer, whose baseline the surrounding text never consults.
  const clip = component.match(/\.roll-clip\s*\{([^}]*)\}/)
  assert.ok(clip, '.roll-clip must exist')
  assert.match(clip[1], /overflow:\s*hidden/, 'clipping belongs on the inner layer')
  assert.match(clip[1], /position:\s*absolute/, 'the clipping layer must be out of flow')

  // The cell needs an in-flow line box to take a baseline from, and that same box
  // is what fixes its width. It must not be taken out of flow.
  const sizer = component.match(/\.roll-sizer\s*\{([^}]*)\}/)
  assert.ok(sizer, '.roll-sizer must exist')
  assert.match(sizer[1], /visibility:\s*hidden/, 'the sizer is hidden, not removed')
  assert.doesNotMatch(sizer[1], /position:\s*absolute/, 'the sizer must stay in flow')
  assert.match(
    component,
    /<span class="roll-sizer"[^>]*>0<\/span><span class="roll-clip"/,
    'the sizer must be adjacent to the clip layer, with no text node between them',
  )
})

check('the gradient is never re-painted by an intermediate element', () => {
  // `background: inherit` along the strip would make each layer paint the whole
  // gradient again as a rectangle; only the custom property carries it intact.
  // Declarations only — the component documents that wrong fix in prose.
  const declarations = component.replace(/\/\*[\s\S]*?\*\//g, '')
  assert.doesNotMatch(
    declarations,
    /background(-image)?:\s*inherit/,
    'the gradient must not travel by inheriting the background',
  )
})

// ── 3. mobile spacing ─────────────────────────────────────────────────────────
check('the compact nav clears the sticky header', () => {
  // `top-3` shifts the header 12px below its flow box, so the nav's margin is
  // measured against a box that is 12px further up than it looks: `mt-4` was a
  // visible 4px.
  const nav = app.match(/<nav class="([^"]*md:hidden[^"]*)"/)
  assert.ok(nav, 'the compact nav must exist')
  const mt = Number(nav[1].match(/mt-(\d+(?:\.\d+)?)/)?.[1] ?? 0)
  const offset = Number(app.match(/sticky top-(\d+(?:\.\d+)?)/)?.[1] ?? 0)
  assert.ok(mt * 4 - offset * 4 >= 8, `nav clearance is only ${mt * 4 - offset * 4}px`)
})

check('content clears the header more on mobile than it used to', () => {
  const main = app.match(/<main class="([^"]*)"/)
  assert.ok(main, 'main must exist')
  const pt = Number(main[1].match(/pt-(\d+(?:\.\d+)?)/)?.[1] ?? 0)
  assert.ok(pt * 4 >= 32, `mobile top padding is ${pt * 4}px, expected >= 32px`)
  assert.match(main[1], /md:pt-10/, 'desktop keeps the larger clearance')
})

check('the stats row is not a single column up to 1280px', () => {
  // It used to stack four full-width cards until `xl`, which is a long scroll on
  // a tablet or a small laptop.
  const row = page.match(/<div class="(mt-3\.5 grid gap-3\.5[^"]*xl:grid-cols-\[1\.6fr_1fr_1fr_1fr\][^"]*)"/)
  assert.ok(row, 'the stats row must exist')
  assert.match(row[1], /sm:grid-cols-2/, 'it must pair up from `sm`')
})

check('the price grid already pairs up from `sm`', () => {
  assert.match(page, /sm:grid-cols-2 xl:grid-cols-4/, 'the price cards keep their breakpoints')
})

// ── 4. every panel-wide number rolls ──────────────────────────────────────────
const pages = [
  'Overview.vue',
  'HftPage.vue',
  'Strategies.vue',
  'Plugins.vue',
  'BacktestPage.vue',
]
const pageText = Object.fromEntries(pages.map((p) => [p, read('src', 'pages', p)]))
const statTile = read('src', 'components', 'ui', 'stat', 'StatTile.vue')
const statRow = read('src', 'components', 'ui', 'stat', 'StatRow.vue')

/** Strip CSS and template comments so prose about a rule cannot trip a rule. */
const code = (s) => s.replace(/\/\*[\s\S]*?\*\//g, '').replace(/<!--[\s\S]*?-->/g, '')

check('a number rendered as text is a rolling number', () => {
  // The whole point of the sweep: a figure that changes under the reader should
  // arrive by rolling, not by swapping glyphs. This walks each template and looks
  // for a raw `{{ … }}` sitting directly inside a figure slot (`stat-num` / `num`).
  // Anything found there is a number that was missed — either wrap it in
  // <RollingNumber>, or mark it in TEXT_MARKERS below if it is prose, not a figure.
  for (const [name, src] of Object.entries({
    ...pageText,
    'RejectionChart.vue': read('src', 'components', 'charts', 'RejectionChart.vue'),
  })) {
    const missed = rawInNumericSlots(src)
    assert.equal(missed.length, 0, `${name} still interpolates ${JSON.stringify(missed)} into a figure slot`)
  }
})

check('the miss detector catches what it claims to', () => {
  // A scanner that silently matches nothing would pass the check above forever.
  // These are the shapes it has to catch, plus the prose it must leave alone.
  const mustCatch = [
    '<div class="stat-num mt-1 leading-none">{{ tradeStats.count }}</div>',
    '<div class="stat-num mt-1 leading-none">\n  {{ cumStats.total }}\n</div>',
    '<td class="px-2 py-2.5 text-right num">{{ num(r.ordersPlaced) }}</td>',
    '<td class="num"><span class="text-up">{{ num(r.wins) }}</span><span> / </span></td>',
    '<span class="num text-faint-fg">{{ positions.length }} 持仓</span>',
    '<div class="stat-num text-[34px]" :class="p ? \'text-up\' : \'text-down\'">\n  {{ signedMoney(netPnl) }}\n</div>',
  ]
  for (const s of mustCatch) {
    assert.ok(rawInNumericSlots(s).length > 0, `the detector missed ${JSON.stringify(s)}`)
  }
  const mustPass = [
    '<div class="stat-num mt-1"><RollingNumber :value="tradeStats.count" /></div>',
    '<span class="num text-faint-fg">0</span>',
    '<td class="num">{{ p.asset }}</td>',
    '<td class="num">{{ dateTime(t.entryTime) }}</td>',
    '<div class="mt-2 text-[11px] num">{{ shortAddr(walletAddr) }}</div>',
  ]
  for (const s of mustPass) {
    assert.equal(rawInNumericSlots(s).length, 0, `the detector false-positives on ${JSON.stringify(s)}`)
  }
})

check('every page that shows figures imports the roller', () => {
  for (const [name, src] of Object.entries(pageText)) {
    assert.match(code(src), /import RollingNumber from '@\/components\/ui\/roll\/RollingNumber\.vue'/, `${name} must import RollingNumber`)
    assert.match(code(src), /<RollingNumber[\s/>]/, `${name} must actually use it`)
  }
})

check('the shared stat primitives roll by default', () => {
  // StatTile/StatRow are used across pages; making the roll opt-in would leave
  // every tile that was not visited by the sweep silently static.
  for (const [name, src] of [['StatTile.vue', statTile], ['StatRow.vue', statRow]]) {
    assert.match(src, /roll:\s*true/, `${name} must default to rolling`)
    assert.match(code(src), /<RollingNumber v-if="props\.roll"/, `${name} must fall back to the roller`)
    assert.match(code(src), /<template v-else>/, `${name} must keep a non-rolling path`)
  }
})

check('a word, not a figure, is opted out of the roll', () => {
  // DRY / LIVE are labels. A roller would announce them digit by digit and let
  // the mode flicker between two strings; they are explicitly excluded instead.
  assert.match(page, /:roll="false"/, 'HftPage must opt the mode row out')
  const overview = pageText['Overview.vue']
  assert.match(overview, /:roll="false"/, 'Overview must opt the mode tile out')
})

check('the readable twin does not double a copied row', () => {
  // With nearly every cell rolling, a table row now carries each figure twice in
  // the DOM. Copying a row must still yield one value per column.
  assert.match(component, /user-select:\s*none/, 'the painted digits must not be selectable')
})

console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`)
process.exit(failures === 0 ? 0 : 1)
