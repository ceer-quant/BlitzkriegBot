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
/** The gateway's single command table — where the bar's vocabulary lives. */
const gatewayCommands = read('..', '..', 'ui_kit', 'src', 'gateway', 'command.rs')
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
  ['Overview', 'Positions', 'Trades', 'Plugins', 'Evolution', 'Settings'],
  // E25 (#331): the arbitration audit gets its own face on BOTH sides — the
  // audit is the kernel's answer to "why did this strategy stop placing", so
  // it is a paired face, not WebUI surplus.
  [
    'Overview',
    'Positions',
    'Trades',
    'Plugins',
    'Evolution',
    'Decisions',
    'Settings',
  ],
]
check(
  'TUI tabs are a known layout (4-tab, 5-tab with Evolution, or 6-tab with Settings, 7-tab with Decisions)',
  () => {
    assert.ok(
      TUI_TAB_SETS.some((s) => s.join(',') === tuiVariants.join(',')),
      `unknown TUI tab set: ${tuiVariants.join('/')} — extend TUI_TAB_SETS deliberately`,
    )
  },
)

/** WebUI nav ids, read out of the segments array. */
const webuiIds = [...shell.matchAll(/id: '(\w+)', label: /g)].map((m) => m[1])
const WEBUI_TAB_SETS = [
  ['overview', 'hft', 'backtest', 'strategies', 'plugins', 'settings'],
  ['overview', 'hft', 'backtest', 'strategies', 'evolution', 'plugins', 'settings'],
  [
    'overview',
    'hft',
    'backtest',
    'strategies',
    'decisions',
    'evolution',
    'plugins',
    'settings',
  ],
]
check('WebUI nav is a known tab set (six, seven with evolution, eight with decisions)', () => {
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
// The Settings face ships with the version card (VERSIONING.md §6.4); its
// pairing is asserted once the TUI carries the tab, same rule as Evolution.
if (tuiVariants.includes('Settings')) {
  TUI_TO_WEBUI.push(['Settings', ['settings'], 'SettingsPage.vue', '版本与更新'])
}
// The Decisions face ships with the E25 arbitration audit (#331); paired the
// same way — both sides render the kernel's GateTrace.detail verbatim.
if (tuiVariants.includes('Decisions')) {
  TUI_TO_WEBUI.push(['Decisions', ['decisions'], 'Decisions.vue', '裁决'])
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

check('TUI 设置页 ↔ WebUI 设置页版本卡片同源（VERSIONING.md §6.4）', () => {
  assert.ok(/fn render_settings\(/.test(ui), 'TUI render_settings 缺失')
  const settingsPage = read('src', 'pages', 'SettingsPage.vue')
  assert.ok(settingsPage.includes('版本与更新'), 'WebUI 版本卡片缺失')
  // 同一事实、同一条三态规则：两侧都要能从内核问版本（system.version）。
  assert.ok(
    read('..', '..', 'ui_kit', 'src', 'core', 'ipc_client.rs').includes('fn system_version'),
    'ui_kit 的 system_version 客户端方法缺失',
  )
  // TUI 侧也必须经同一接口取值，不自己 parse 版本字符串。
  assert.ok(ui.includes('git_hash'), 'TUI 不再从 system.version 读修订号')
})

check('TUI `n` 网络诊断浮层 ↔ WebUI 设置页网络诊断卡片', () => {
  assert.ok(new RegExp('fn render_net_check\\(').test(ui), 'TUI network overlay gone')
  // The bar's vocabulary is the gateway's own table now — app.rs used to hold a
  // hand-copied list, which had already drifted (a phantom `risk`, four real
  // verbs missing). So pin both halves: the TUI reads the table, and the table
  // still carries the verb.
  assert.ok(app.includes('command_verbs('), 'the command bar stopped reading the gateway command table')
  assert.ok(gatewayCommands.includes('"netcheck"'), 'the gateway command table lost the netcheck verb')
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

/** The body of a rustfmt'd top-level `fn` — its closing brace is the only `}` in
 * column 0, so this needs no brace matching over `format!` placeholders. */
const topLevelFn = (src, name) => {
  const start = src.indexOf(`fn ${name}(`)
  assert.notEqual(start, -1, `no fn ${name} in ui.rs`)
  const end = src.indexOf('\n}\n', start)
  assert.notEqual(end, -1, `unterminated fn ${name} in ui.rs`)
  return src.slice(start, end)
}

const netOverlay = topLevelFn(ui, 'render_net_check')
const netCheckRs = read('..', '..', 'ui_kit', 'src', 'core', 'net_check.rs')
const settings = read('src', 'pages', 'SettingsPage.vue')

// The two faces render the same two FACTS, and they have to say them at the
// same volume. They did not: the TUI painted a fake-IP resolution and the proxy
// variable names in its attention colour while the panel printed both in
// `text-faint-fg`, the faintest tier on the card — the same evidence, so quiet
// on one side that the reader would not connect it to the failure above (#260).
console.log('网络自检：同一事实，两侧同一语气')

/** The panel's faintest tier and the TUI's, which no shared fact may sit in. */
const WEAKEST = { web: 'text-faint-fg', tui: 'DIM' }

check('fake-IP 标记在两侧都出现，且都不在最弱一档', () => {
  assert.ok(netOverlay.includes('[fake-ip]'), 'TUI overlay no longer marks a fake-IP resolution')
  assert.ok(settings.includes('fake-IP'), 'WebUI no longer marks a fake-IP resolution')
  // TUI: the span that carries the marker, and the colour it was given.
  const tui = netOverlay.match(/if item\.fake_ip[\s\S]{0,120}?Style::default\(\)\.fg\((\w+)\)/)
  assert.notEqual(tui, null, 'the fake-IP span is no longer recognisable in ui.rs')
  assert.equal(tui[1], 'WARN', `the TUI paints fake-IP as ${tui[1]}, not its WARN tier`)
  assert.notEqual(tui[1], WEAKEST.tui, 'fake-IP fell to the TUI\u2019s faintest tier')
  // WebUI: the class on the element whose text is the marker.
  const web = settings.match(/class="([^"]*)"\s*>fake-IP</)
  assert.notEqual(web, null, 'the WebUI fake-IP marker lost its own element')
  assert.doesNotMatch(web[1], new RegExp(WEAKEST.web), 'fake-IP fell to the WebUI\u2019s faintest tier')
  assert.match(web[1], /text-primary/, `the WebUI paints fake-IP as "${web[1]}"`)
})

check('proxy 变量名在两侧都出现，且都不在最弱一档', () => {
  assert.ok(netOverlay.includes('proxy variables set:'), 'TUI overlay stopped naming the proxy variables')
  assert.ok(settings.includes('netView.proxyNote'), 'WebUI stopped rendering the proxy note')
  const tui = netOverlay.match(/proxy variables set:[\s\S]{0,200}?Style::default\(\)\.fg\((\w+)\)/)
  assert.notEqual(tui, null, 'the proxy line is no longer recognisable in ui.rs')
  assert.equal(tui[1], 'WARN', `the TUI paints the proxy line as ${tui[1]}, not its WARN tier`)
  assert.notEqual(tui[1], WEAKEST.tui, 'the proxy line fell to the TUI\u2019s faintest tier')
  const web = settings.match(/class="([^"]*)"[^>]*>\{\{\s*netView\.proxyNote/)
  assert.notEqual(web, null, 'the WebUI proxy note lost its own element')
  assert.doesNotMatch(web[1], new RegExp(WEAKEST.web), 'the proxy note fell to the WebUI\u2019s faintest tier')
  assert.match(web[1], /text-primary/, `the WebUI paints the proxy note as "${web[1]}"`)
  // The names (never the values) are what the WebUI note is built from.
  assert.ok(read('src', 'lib', 'net-check.ts').includes('proxyEnv.join'), 'the WebUI note stopped naming the variables')
})

// The table columns and the truncation rule have ONE source, in `net_check.rs`,
// because the two renderers had a copy each and no assertion held them equal: a
// width change in one silently mis-aligned the log pane against the overlay.
console.log('网络自检：表格列宽与截断只有一份实现')

check('列宽与 truncate 由 ui_kit 提供，TUI 不再自带一份', () => {
  for (const shared of ['pub const COL_NAME', 'pub const COL_TARGET', 'pub const COL_MS', 'pub fn row_columns', 'pub fn truncate']) {
    assert.ok(netCheckRs.includes(shared), `net_check.rs no longer exports \`${shared}\``)
  }
  assert.ok(netOverlay.includes('row_columns('), 'the TUI row is not built from the shared columns')
  assert.ok(netOverlay.includes('use blitzkrieg_ui_kit::core::net_check::'), 'the TUI stopped importing the shared helpers')
  // No second width format: a `{:<11}` / `{:>7}` in the overlay means someone
  // re-derived the layout locally, which is exactly the drift this pins.
  const localWidth = netOverlay.match(/:<\d+|:>\d+/g)
  assert.equal(localWidth, null, `the overlay hard-codes a column width again: ${localWidth?.join(', ')}`)
  // And no second truncation: `truncate_cell` was the copy that drifted.
  assert.doesNotMatch(ui, /\nfn (?:truncate|truncate_cell)\b/, 'ui.rs defines its own truncation again')
})

// The overlay sized itself from a literal 20 rows, so a report that grew past it
// lost its tail silently (#260). The height has to come from the content — and
// from the WRAPPED content, since the paragraph wraps.
check('浮层高度由内容算出来，不再写死', () => {
  assert.ok(ui.includes('fn wrapped_rows('), 'the row counter is gone')
  assert.ok(netOverlay.includes('wrapped_rows('), 'the overlay stopped counting its rows')
  assert.match(
    netOverlay,
    /centered_rect\(\s*area,\s*NET_OVERLAY_PCT_X,\s*\(body_rows as u16\)/,
    'the overlay height is not derived from the counted rows',
  )
  assert.doesNotMatch(
    netOverlay,
    /centered_rect\(area,\s*\d+\s*,\s*\d+\s*\)/,
    'the overlay height is a literal again — a longer report would be cropped in silence',
  )
  assert.ok(
    netOverlay.includes('more rows below the fold'),
    'an over-long report is cropped in silence instead of saying so',
  )
})

console.log('WebUI surplus — declared, each with where it lives off-panel')

/** Every WebUI tab with no TUI twin must name its off-panel equivalent.
 * `settings` stopped being surplus when the TUI gained its own Settings tab
 * (VERSIONING.md §6.4) — the version card is now a PAIRED face asserted in
 * TUI_TO_WEBUI; only the theme/sound/token extras remain WebUI enhancements. */
const WEBUI_SURPLUS = [
  ['backtest', 'CLI: blitzkrieg-core --backtest --backtest-report（回放复盘是 WebUI 增强面）'],
  ['strategies', 'TUI 指令台 strategy <name> on|off 同动词；账本表格是 WebUI 增强面'],
]
check('every WebUI-only tab has a declared off-panel equivalent', () => {
  const surplus = webuiIds.filter((id) => !TUI_TO_WEBUI.some(([, ts]) => ts.includes(id)))
  assert.deepEqual(
    surplus,
    ['backtest', 'strategies'],
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
