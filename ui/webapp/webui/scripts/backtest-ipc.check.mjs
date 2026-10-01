/**
 * #353 backtest-surface audit — the spec's three red lines, executable.
 *
 * The 回测 page must be a dumb glass over the kernel: every dataset pull, run,
 * result and export travels the gateway's `backtest.*` IPC proxy, and the kernel
 * is the only thing that touches disk. Three defects would break that contract,
 * so this check holds each one red:
 *
 *   1. WebUI reads local files directly (file inputs / FileReader / fs imports).
 *      The pre-#353 viewer loaded report JSON through an <input type="file"> —
 *      exactly the绕过-IPC shape the spec forbids. The kernel now serves the
 *      same bytes over IPC; the panel never touches the filesystem again.
 *   2. The old 「回放复盘」 naming survives anywhere in src/ or the check
 *      scripts. The rename (issue 禁止事项 #11) is page, menu, route and
 *      comments; one straggler means the next reader re-introduces the old
 *      surface's vocabulary.
 *   3. An arbitrary-script path exists (eval / new Function / v-html /
 *      innerHTML / document.write). The backtest form takes strategy *names*;
 *      anything that could execute user- or dataset-supplied text in the panel
 *      would turn a report viewer into a code-execution surface.
 *
 *   cd ui/webapp/webui && npm run check:backtest
 */
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { dirname, join, extname } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const srcDir = join(here, '..', 'src')

/** Every .vue/.ts file under src/, recursively. */
function walk(dir) {
  const out = []
  for (const name of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, name.name)
    if (name.isDirectory()) out.push(...walk(p))
    else if (['.vue', '.ts'].includes(extname(name.name))) out.push(p)
  }
  return out
}

const srcFiles = walk(srcDir).map((p) => ({
  path: p.slice(p.indexOf('src')),
  src: readFileSync(p, 'utf8'),
}))

// The page + panel sources are the enforcement surface; the check scripts are
// only scanned for the renamed vocabulary (they quote UI copy, not access it).
const scriptFiles = readdirSync(here)
  .filter((n) => n.endsWith('.mjs'))
  .map((n) => ({ path: `scripts/${n}`, src: readFileSync(join(here, n), 'utf8') }))

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

console.log('red line 1 — the panel never reads local files (kernel IPC only)')

// One pattern list, one meaning: any of these in src/ is the panel reaching
// around the gateway to the user's disk.
const FS_PATTERNS = [
  [/<input[^>]*type=["']file["']/, '<input type="file">'],
  [/new\s+FileReader\b/, 'new FileReader'],
  [/showOpenFilePicker|showDirectoryPicker/, 'File System Access API'],
  [/from\s+['"]node:fs|require\(\s*['"]fs['"]\s*\)/, 'node:fs import'],
  [/fetch\(\s*['"`]file:/, 'fetch file:// URL'],
]

check('src/ carries no file-read surface', () => {
  const offenders = []
  for (const f of srcFiles) {
    for (const [re, name] of FS_PATTERNS) {
      if (re.test(f.src)) offenders.push(`${f.path}: ${name}`)
    }
  }
  assert.deepEqual(offenders, [], `panel reads local files directly:\n${offenders.join('\n')}`)
})

check('the backtest page reaches everything through the backtest.* proxy', () => {
  const page = srcFiles.find((f) => f.path.endsWith('pages/BacktestPage.vue'))
  assert.ok(page, 'BacktestPage.vue missing')
  // All four verbs of the flow, each an api.* call — the page has no other
  // data source.
  for (const fn of ['backtestOnchainList', 'backtestOnchainPull', 'backtestRun', 'backtestStatus', 'backtestResult', 'backtestExport']) {
    assert.ok(page.src.includes(`api.${fn}`), `page does not call api.${fn}`)
  }
})

console.log('red line 2 — the 「回放复盘」 rename is total')

check('no 回放 naming survives in src/ or the check scripts', () => {
  // The audit script itself must name the forbidden word to describe the rule —
  // it is the one exempt file, exactly like a linter quoting its own error text.
  const offenders = []
  for (const f of [...srcFiles, ...scriptFiles.filter((f) => !f.path.endsWith('backtest-ipc.check.mjs'))]) {
    const lines = f.src.split('\n')
    lines.forEach((line, i) => {
      if (/回放/.test(line)) offenders.push(`${f.path}:${i + 1}`)
    })
  }
  assert.deepEqual(offenders, [], `「回放」 residue:\n${offenders.join('\n')}`)
})

console.log('red line 3 — no arbitrary-script execution surface')

const EXEC_PATTERNS = [
  [/\beval\s*\(/, 'eval()'],
  [/new\s+Function\s*\(/, 'new Function()'],
  [/\bv-html\b/, 'v-html'],
  [/\.innerHTML\s*=/, 'innerHTML assignment'],
  [/document\.write\s*\(/, 'document.write'],
]

check('src/ carries no script-execution surface', () => {
  const offenders = []
  for (const f of srcFiles) {
    for (const [re, name] of EXEC_PATTERNS) {
      if (re.test(f.src)) offenders.push(`${f.path}: ${name}`)
    }
  }
  assert.deepEqual(offenders, [], `panel can execute input text:\n${offenders.join('\n')}`)
})

// The kernel enforces the name-only contract server-side (start_backtest refuses
// anything that is not a known strategy name); here we pin the page's side of
// the deal: the strategy field is a plain text Input, never a code area.
check('the strategy selector takes names, not code', () => {
  const page = srcFiles.find((f) => f.path.endsWith('pages/BacktestPage.vue'))
  assert.ok(page, 'BacktestPage.vue missing')
  assert.ok(/strategiesRaw/.test(page.src), 'strategy filter input missing')
  assert.ok(!/\btextarea\b|\bcodeEditor\b|\bace\b|\bcodemirror\b/i.test(page.src), 'a code-editing surface appeared on the backtest page')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
