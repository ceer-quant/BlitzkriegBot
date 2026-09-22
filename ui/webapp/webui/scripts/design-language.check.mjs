/**
 * E11 design-language self-audit — the「统一设计语言」验收, executable.
 *
 * The panel has a token vocabulary (gold primary, glass surfaces, label-micro
 * headers, stat-num figures, EmptyState/AlertBanner/Card primitives). This walk
 * over every page plus the shell pins three properties:
 *
 *   1. NO ad-hoc colors: a page may not hard-code a hex, rgb(), or Tailwind
 *      palette class (`text-red-500`) — color must come from the theme tokens
 *      (`--primary`, `text-up`, `bg-panel-2`, …), or the two themes diverge the
 *      moment one page is edited. Decorative `drop-shadow-…rgba(0,0,0,…)` is
 *      allowlisted: it is a shadow, not a surface color, and is identical in
 *      both themes.
 *   2. NO orphan styling: every page composes the shared kit (imports from
 *      `components/ui/`) and surfaces empty/error states through EmptyState /
 *      AlertBanner rather than hand-rolled prose.
 *   3. NO hand-rolled alert tint: an alert strip is `border-<tone>/N` +
 *      `bg-<tone>/N` + `text-<tone>` on one element — i.e. the AlertBanner
 *      recipe, copied into the page. Six such copies existed at different
 *      concentrations (`/35 /8` against the primitive's `/30 /10`), which is how
 *      the panel ended up with two reds for one meaning (#260). The kit owns the
 *      recipe; a page composes the primitive or a Badge variant instead.
 *   4. E8's layout vocabulary is still in use where it belongs (`glass`,
 *      `label-micro`, `rise-in`) — a page drifting off it breaks the 10-second
 *      information hierarchy the issue demands.
 *
 *   cd ui/webapp/webui && npm run check:design
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

/** Shell + login + all six pages = the 5 界面 of the issue (回放 included). */
const FILES = [
  ['App.vue', 'src', 'App.vue'],
  ['LoginView', 'src', 'components', 'LoginView.vue'],
  ['Overview', 'src', 'pages', 'Overview.vue'],
  ['行情面板', 'src', 'pages', 'HftPage.vue'],
  ['回放复盘', 'src', 'pages', 'BacktestPage.vue'],
  ['策略', 'src', 'pages', 'Strategies.vue'],
  ['插件', 'src', 'pages', 'Plugins.vue'],
  ['设置', 'src', 'pages', 'SettingsPage.vue'],
].map(([name, ...p]) => ({ name, src: read(...p) }))

// Tailwind's palette, with and without a shade suffix: `text-red-500`,
// `bg-blue-300`, but NOT `text-up` / `bg-panel-2` / `text-muted-fg`.
const PALETTE =
  /(?:text|bg|border|ring|fill|stroke|shadow|from|to|via|decoration|outline|divide|accent|caret)-(?:black|white|slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose)(?:-\d{2,3})?\b/

/** Bare hex/rgb color, anywhere outside a decorative drop-shadow. */
const RAW_COLOR = /(?:#[0-9a-fA-F]{3,8}\b|(?<!drop-shadow-\[[^\]]{0,80})\brgba?\()/

console.log('ad-hoc colors — token vocabulary only')

for (const f of FILES) {
  check(`${f.name}: no palette classes or raw colors`, () => {
    const palette = f.src.match(new RegExp(PALETTE, 'g'))
    assert.equal(palette, null, `palette classes: ${palette?.join(', ')}`)
    // Walk line by line so the allowlist can see the drop-shadow context.
    const offenders = f.src.split('\n').filter((line) => {
      if (!RAW_COLOR.test(line)) return false
      // A shadow is identical in both themes and reads as depth, not color.
      return !/drop-shadow-\[[^\]]*rgba?\(/.test(line)
    })
    assert.deepEqual(offenders, [], `raw color lines:\n${offenders.join('\n')}`)
  })
}

console.log('shared kit — no orphan styling')

for (const f of FILES) {
  const kits = [...f.src.matchAll(/from '(?:@\/|\.\.?\/)components\/(ui|charts)\//g)].map((m) => m[1])
  check(`${f.name}: composes the shared kit (≥2 ui imports)`, () => {
    assert.ok(kits.length >= 2, `only ${kits.length} kit imports`)
  })
}

for (const f of FILES) {
  check(`${f.name}: states empty/loading via EmptyState or is exempt`, () => {
    // Exempt: the shell, the login gate, and the settings desk — surfaces with
    // no data-empty state to render. Every data page must route its empty and
    // loading shapes through EmptyState.
    const exempt = ['App.vue', 'LoginView', '设置'].includes(f.name)
    const hasEmpty = /EmptyState/.test(f.src)
    assert.ok(hasEmpty || exempt, 'page renders data but has no EmptyState usage')
  })
}

console.log('alert tint — the recipe lives in the kit, not in the page')

/**
 * A hand-rolled alert: ONE element whose class string carries a tone border, a
 * tone background and a tone text — `border-down/35 bg-down/8 text-down`. That
 * is AlertBanner's recipe copied into a page, and it is what let one meaning
 * acquire two concentrations (`/35 /8` beside the primitive's `/30 /10`, #260).
 *
 * The three parts must appear in a single quoted class literal, so the rule
 * cannot fire on the two shapes that legitimately live in pages:
 *   * a directional stat tile (`border-down/25 bg-down/8`) — a surface, no
 *     tone text on the box itself, and not an alert;
 *   * a token-coloured span (`text-down`) — text, not a surface.
 * Both are asserted below so the rule cannot silently grow teeth it did not have.
 */
const ALERT_TINT =
  /border-(up|down|primary|info|gold)\/\d+[^"']*bg-\1\/\d+[^"']*text-\1(?![\w-])/g

for (const f of FILES) {
  check(`${f.name}: no hand-rolled alert tint (use AlertBanner / Badge)`, () => {
    const offenders = f.src
      .split('\n')
      .map((line, i) => [i + 1, line.match(ALERT_TINT)])
      .filter(([, m]) => m)
      .map(([n, m]) => `${f.name}:${n} ${m.join(', ')}`)
    assert.deepEqual(
      offenders,
      [],
      `hand-rolled alert tint — compose AlertBanner or a Badge variant instead:\n${offenders.join('\n')}`,
    )
  })
}

// The rule above must not be the only reason a page has no alert tint: the
// primitive has to actually be the thing that replaced it.
for (const f of FILES.filter((f) => !['App.vue', 'LoginView'].includes(f.name))) {
  check(`${f.name}: surfaces its alerts through the AlertBanner primitive`, () => {
    assert.ok(/<AlertBanner/.test(f.src), 'no AlertBanner usage')
  })
}

// Guard the rule's own edges: a directional tile and a tone-coloured span are
// not alerts, and a rule that started failing on them would be a false positive
// that gets allowlisted instead of fixed.
check('the alert-tint rule does not fire on a tile or a bare tone span', () => {
  const tile = `<div class="rounded-md border border-down/25 bg-down/8 px-2.5 py-2">`
  const span = `<span class="stat-num text-[15px] text-down">`
  const alert = `<div class="rounded-md border border-down/35 bg-down/8 text-down">`
  assert.equal(tile.match(ALERT_TINT), null, 'a directional tile is not an alert')
  assert.equal(span.match(ALERT_TINT), null, 'a tone span is not an alert')
  assert.notEqual(alert.match(ALERT_TINT), null, 'the rule must still catch a real alert')
})

console.log('E8 vocabulary still in use')

// Per-page, the vocabulary every data page shares.
for (const f of FILES.filter((f) => !['LoginView', 'App.vue'].includes(f.name))) {
  check(`${f.name}: surfaces via glass or the Card primitive`, () => {
    assert.ok(/glass|<Card/.test(f.src), 'neither glass nor Card')
  })
  check(`${f.name}: uses the micro-label header style`, () => {
    assert.ok(/label-micro/.test(f.src), 'no label-micro')
  })
  check(`${f.name}: figures use stat-num / RollingNumber`, () => {
    assert.ok(/stat-num|RollingNumber/.test(f.src), 'no figure vocabulary')
  })
}

// Panel-wide: the entrance motion is part of the identity, not a per-page duty.
check('entrance motion (rise-in) is alive across the panel', () => {
  const users = FILES.filter((f) => /rise-in/.test(f.src)).length
  assert.ok(users >= 4, `only ${users} surfaces use rise-in`)
})

console.log('global tokens — the theme contract is real')

check('styles carry the token palette the pages consume', () => {
  const styles = read('src', 'styles', 'theme.css')
  for (const token of ['--primary', '--panel', '--line', '--up', '--down']) {
    assert.ok(styles.includes(token), `missing ${token} in styles/theme.css`)
  }
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
