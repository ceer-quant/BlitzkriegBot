/**
 * E11 — the「TUI/WebUI 对等清单逐项核对」验收, executable.
 *
 * The two faces must cover the same operational surface, but they are NOT
 * clones: the TUI is a four-tab cockpit driven by one command bar, while the
 * WebUI adds richer pages (策略账本, 回放复盘, 设置). This check parses BOTH
 * sources — the Rust tab enum and its renderers, the Vue nav segments — and
 * asserts every TUI face has a live WebUI home with the matching content, then
 * pins the deliberate surplus on the WebUI side with its off-panel equivalent
 * (CLI verb or TUI command), so the list stays honest instead of silently
 * rotting.
 *
 *   cd ui/webapp/webui && npm run check:parity
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

const app = read('..', '..', 'ui_kit_panel', 'src', 'app.rs')
const ui = read('..', '..', 'ui_kit_panel', 'src', 'ui.rs')
const shell = read('src', 'App.vue')

console.log('the two nav surfaces, parsed from source')

/** TUI tabs, read out of the enum so a new Rust tab forces this list to move. */
const enumBody =
  [...app.matchAll(/^pub enum Tab \{([\s\S]*?)^\}/gm)][0]?.[1] ?? ''
const tuiVariants = [...enumBody.matchAll(/^\s{4}([A-Z]\w+),$/gm)].map((m) => m[1])

check('TUI exposes exactly four tabs (Overview/Positions/Trades/Plugins)', () => {
  assert.deepEqual(tuiVariants, ['Overview', 'Positions', 'Trades', 'Plugins'])
})

/** WebUI nav ids, read out of the segments array. */
const webuiIds = [...shell.matchAll(/id: '(\w+)', label: /g)].map((m) => m[1])
check('WebUI nav has the six known tabs', () => {
  assert.deepEqual(
    webuiIds,
    ['overview', 'hft', 'backtest', 'strategies', 'plugins', 'settings'],
  )
})

console.log('TUI face → WebUI home (every TUI tab must live somewhere)')

/** Each pair: TUI tab → WebUI ids, and the marker proving the content is live. */
const TUI_TO_WEBUI = [
  ['Overview', ['overview'], 'Overview.vue', '权益曲线'],
  ['Positions', ['hft', 'overview'], 'HftPage.vue', '强平'],
  ['Trades', ['hft'], 'HftPage.vue', '成交'],
  ['Plugins', ['plugins'], 'Plugins.vue', '行情插件'],
]
for (const [tab, targets, pageFile, marker] of TUI_TO_WEBUI) {
  check(`TUI ${tab} → WebUI ${targets.join(' + ')}（${marker} 在页上）`, () => {
    for (const t of targets) assert.ok(webuiIds.includes(t), `WebUI has no '${t}' tab`)
    const page = read('src', 'pages', pageFile)
    assert.ok(page.includes(marker), `${marker} not found in ${pageFile}`)
  })
  // The face must actually render in the TUI, not merely exist as a variant.
  check(`TUI ${tab} has a render fn`, () => {
    assert.ok(
      new RegExp(`fn render_${tab.toLowerCase()}\\(`).test(ui),
      `no render_${tab.toLowerCase()} in ui.rs`,
    )
  })
}

console.log('shared control surfaces (not tabs, but faces)')

check('TUI command bar ↔ WebUI 设置页指令台', () => {
  assert.ok(new RegExp('fn render_command_bar\\(').test(ui), 'TUI command bar gone')
  const settings = read('src', 'pages', 'SettingsPage.vue')
  assert.ok(settings.includes('指令台'), 'WebUI command desk gone')
})

check('TUI `?` help overlay ↔ WebUI 首次引导 + 设置页会话说明', () => {
  assert.ok(new RegExp('fn render_help\\(').test(ui), 'TUI help overlay gone')
  assert.ok(
    read('src', 'components', 'OnboardingTour.vue').includes('首次引导'),
    'WebUI onboarding gone',
  )
})

check('TUI flatten confirm dialog ↔ WebUI 强平按钮', () => {
  assert.ok(new RegExp('fn render_confirm\\(').test(ui), 'TUI confirm dialog gone')
  assert.ok(read('src', 'pages', 'HftPage.vue').includes('强平'), 'WebUI flatten gone')
})

console.log('WebUI surplus — declared, each with where it lives off-panel')

/** Every WebUI tab with no TUI twin must name its off-panel equivalent. */
const WEBUI_SURPLUS = [
  ['backtest', 'CLI: blitzkrieg-core --backtest --backtest-report（回放复盘是 WebUI 增强面）'],
  ['strategies', 'TUI 指令台 strategy <name> on|off 同动词；账本表格是 WebUI 增强面'],
  ['settings', 'TUI header 遥测（在线/模式）；主题/声音/token 管理是 WebUI 增强面'],
]
check('every WebUI-only tab has a declared off-panel equivalent', () => {
  const surplus = webuiIds.filter((id) => !TUI_TO_WEBUI.some(([, ts]) => ts.includes(id)))
  assert.deepEqual(
    surplus,
    ['backtest', 'strategies', 'settings'],
    'new WebUI tab without a parity note — add it to WEBUI_SURPLUS and this list',
  )
})
for (const [id, note] of WEBUI_SURPLUS) {
  check(`${id}: ${note.slice(0, 24)}…`, () => {
    // The note lives in this file on purpose; assert its shape so it cannot
    // degrade into a silent TODO.
    assert.ok(note.includes('WebUI') || note.includes('CLI') || note.includes('TUI'))
  })
}

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
