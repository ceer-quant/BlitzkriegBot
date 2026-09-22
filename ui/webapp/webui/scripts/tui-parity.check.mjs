/**
 * E11 — the「TUI/WebUI 对等清单逐项核对」验收, executable.
 *
 * The two faces must cover the same operational surface, but they are NOT
 * clones: the TUI is a tab cockpit driven by one command bar (four tabs today,
 * five once the Evolution tab lands with #154), while the WebUI adds richer
 * pages (进化, 策略账本, 回放复盘, 设置). This check parses BOTH sources — the
 * Rust tab enum and its renderers, the Vue nav segments — and asserts every
 * TUI face has a live WebUI home with the matching content, then pins the
 * deliberate surplus on the WebUI side with its off-panel equivalent (CLI verb
 * or TUI command), so the list stays honest instead of silently rotting.
 *
 * Both known layouts are accepted (four-tab pre-E13, five-tab with Evolution)
 * so the parity gate can gate the transition itself instead of flickering
 * between base and PR merge previews; an UNKNOWN tab set still fails loudly.
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

/** Known TUI layouts: the four-tab cockpit, and the five-tab one once the
 * Evolution face lands with the E13 trio (#154). Both are accepted so the
 * parity gate can gate the transition itself; an unknown tab set still fails. */
const TUI_TAB_SETS = [
  ['Overview', 'Positions', 'Trades', 'Plugins'],
  ['Overview', 'Positions', 'Trades', 'Plugins', 'Evolution'],
]
check('TUI tabs are a known layout (4-tab, or 5-tab with Evolution)', () => {
  assert.ok(
    TUI_TAB_SETS.some((s) => s.join(',') === tuiVariants.join(',')),
    `unknown TUI tab set: ${tuiVariants.join('/')} — extend TUI_TAB_SETS deliberately`,
  )
})

/** WebUI nav ids, read out of the segments array. */
const webuiIds = [...shell.matchAll(/id: '(\w+)', label: /g)].map((m) => m[1])
const WEBUI_TAB_SETS = [
  ['overview', 'hft', 'backtest', 'strategies', 'plugins', 'settings'],
  ['overview', 'hft', 'backtest', 'strategies', 'evolution', 'plugins', 'settings'],
]
check('WebUI nav is a known tab set (six, or seven with evolution)', () => {
  assert.ok(
    WEBUI_TAB_SETS.some((s) => s.join(',') === webuiIds.join(',')),
    `unknown WebUI tab set: ${webuiIds.join('/')} — extend WEBUI_TAB_SETS deliberately`,
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
// The Evolution face ships with the E13 trio (#154); its pairing is asserted
// only once the TUI actually carries the tab, so both sides of the transition
// stay gated instead of one side waiting on the other.
if (tuiVariants.includes('Evolution')) {
  TUI_TO_WEBUI.push(['Evolution', ['evolution'], 'EvolutionPage.vue', '拍板'])
}
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

check('TUI `n` 网络诊断浮层 ↔ WebUI 设置页网络诊断卡片', () => {
  assert.ok(new RegExp('fn render_net_check\\(').test(ui), 'TUI network overlay gone')
  assert.ok(app.includes('netcheck'), 'TUI command vocabulary lost the netcheck verb')
  const settings = read('src', 'pages', 'SettingsPage.vue')
  assert.ok(settings.includes('网络诊断'), 'WebUI network diagnosis card gone')
  // Both faces print the same reading rules — a status the core adds must not be
  // hidden on one side only (Rust `net_check::status_label` ↔ lib/net-check.ts).
  assert.ok(
    read('..', '..', 'ui_kit', 'src', 'core', 'net_check.rs').includes('fn status_label'),
    'the shared Rust status labels are gone',
  )
  assert.ok(read('src', 'lib', 'net-check.ts').includes('netStatusLabel'), 'the WebUI labels are gone')
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
