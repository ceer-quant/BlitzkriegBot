/**
 * #781 — the entry-chunk budget.
 *
 * The panel used to ship one 1.78 MB bundle (606 kB gzip) containing every
 * page and the full echarts build. Route pages are now `defineAsyncComponent`
 * and the vendor surfaces are split (see vite.config.ts), so the shell's entry
 * chunk is 92 kB raw / 32 kB gzip. This gate pins that split: a page that goes
 * back to a static import, or echarts drifting into the shell, must FAIL here
 * instead of quietly rebuilding the monolith.
 *
 * Budget: 128 kB raw / 44 kB gzip for the entry chunk (the module script
 * referenced by index.html). Measured after the split: 91.86 kB / 31.58 kB —
 * ~39% headroom for legitimate shell growth. The margin is deliberately below
 * the chunks it guards against: echarts (545 kB), SettingsPage (66 kB),
 * HftPage (64 kB), Strategies (44 kB).
 *
 * The script builds `dist` itself when it is missing, so `npm run check:all`
 * works on a fresh checkout; CI already runs `npm run build` before the gate
 * and the existing artifact is reused.
 *
 *   cd ui/webapp/webui && npm run check:bundle
 */
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, statSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { gzipSync } from 'node:zlib'

const here = dirname(fileURLToPath(import.meta.url))
const root = join(here, '..')

const ENTRY_BUDGET_RAW_KB = 128
const ENTRY_BUDGET_GZIP_KB = 44

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

const dist = join(root, 'dist')
if (!existsSync(join(dist, 'index.html'))) {
  console.log('dist/ missing — building before the budget check')
  execFileSync(process.execPath, [join(root, 'node_modules', 'vite', 'bin', 'vite.js'), 'build'], {
    cwd: root,
    stdio: 'inherit',
  })
}

const html = readFileSync(join(dist, 'index.html'), 'utf8')
const entrySrc = html.match(/<script[^>]*type="module"[^>]*src="([^"]+)"/)?.[1]

console.log('entry chunk — the shell must not swallow a page or a vendor')

check('index.html references one module entry script', () => {
  assert.ok(entrySrc, 'no <script type="module" src="..."> in dist/index.html')
})

const entryPath = entrySrc && join(dist, entrySrc.replace(/^\/?panel\//, ''))
const entryExists = entryPath && existsSync(entryPath)

check(`entry chunk exists (${entrySrc ?? 'missing'})`, () => {
  assert.ok(entryExists, `referenced entry script not found: ${entryPath}`)
})

if (entryExists) {
  const raw = statSync(entryPath).size
  const gzip = gzipSync(readFileSync(entryPath)).length
  const rawKb = raw / 1000
  const gzipKb = gzip / 1000
  console.log(`       entry: ${rawKb.toFixed(2)} kB raw / ${gzipKb.toFixed(2)} kB gzip`)

  check(`entry chunk ≤ ${ENTRY_BUDGET_RAW_KB} kB raw`, () => {
    assert.ok(
      rawKb <= ENTRY_BUDGET_RAW_KB,
      `${rawKb.toFixed(2)} kB > ${ENTRY_BUDGET_RAW_KB} kB — a page or a vendor library is back in the entry chunk`,
    )
  })

  check(`entry chunk ≤ ${ENTRY_BUDGET_GZIP_KB} kB gzip`, () => {
    assert.ok(
      gzipKb <= ENTRY_BUDGET_GZIP_KB,
      `${gzipKb.toFixed(2)} kB > ${ENTRY_BUDGET_GZIP_KB} kB — a page or a vendor library is back in the entry chunk`,
    )
  })
}

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
