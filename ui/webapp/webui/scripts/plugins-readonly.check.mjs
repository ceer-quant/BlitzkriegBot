/**
 * 绑定即启用 — the plugins page is a STATUS VIEW, executable reverse-acceptance.
 *
 * 2026-10-16 裁决：插件启用由账户绑定派生（绑定即启用），内核侧的派生逻辑在
 * 并行分支。一旦启用来源变成账户绑定，插件页作为「启用开关面板」的旧语义就
 * 错了 —— 操作者在插件页关掉的开关会被派生逻辑翻回来。所以本页必须诚实：
 * 只渲染会话事实（注册状态、模式声明），启用控制移交账户切换器。
 *
 * 钉住的三条（回归即红）：
 *
 *   1. 页面不再出现 `market.plugin.set` 写路径（extension.enable/disable 亦然）
 *      —— 页面没有 enable/disable 写动词，派生逻辑翻不动不存在的开关。
 *   2. 「绑定即启用」的移交文案在场 —— 操作员能从页面上读到开关去了哪里。
 *   3. 卡片仍渲染 modes（模式声明）与注册状态（registered/active）—— 状态
 *      视图的信息量不能因为去掉开关而缩水。
 *
 *   cd ui/webapp/webui && npm run check:plugins-readonly
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

const page = read('src', 'pages', 'Plugins.vue')
const client = read('src', 'api', 'client.ts')

console.log('绑定即启用 — the plugins page is a status view, not a switch panel')

// ① The write verbs are gone: no market.plugin.set anywhere on the page, and
//    the client no longer carries the plugin enable/set writers at all.
check('Plugins.vue has no market.plugin.set write path', () => {
  assert.ok(!page.includes('marketPluginSet'), 'marketPluginSet still called on the page')
  assert.ok(!page.includes('/market-plugin/set'), 'market.plugin.set endpoint still on the page')
  assert.ok(!page.includes('extensionSet'), 'extensionSet still called on the page')
})

check('client.ts carries no plugin enable/set writer', () => {
  assert.ok(!client.includes('marketPluginSet'), 'marketPluginSet writer still in client.ts')
  assert.ok(!client.includes('/market-plugin/set'), 'market.plugin.set route still in client.ts')
  assert.ok(!client.includes('extensionSet'), 'extensionSet writer still in client.ts')
  // The read path survives — the page still renders the session facts.
  assert.match(client, /marketPlugin:\s*\(\)\s*=>\s*request/, 'market.plugin.get read path missing')
})

// ② The handover copy: the page states where enable control went.
check('绑定即启用 handover copy is on the page', () => {
  assert.match(page, /插件启用由账户绑定派生/, 'derivation sentence missing')
  assert.match(page, /绑定即启用/, 'the four-character rule must appear verbatim')
  assert.match(page, /账户切换器/, 'must point at the AccountSwitcher')
})

// ③ The status view still carries the facts: mode declarations and the
//    registered/active state per plugin card.
check('plugin cards still render modes and registered/active state', () => {
  assert.match(page, /modesList\(/, 'modes badges no longer rendered')
  assert.match(page, /r\.modes/, 'PluginRow.modes no longer read')
  assert.match(page, /注册状态/, '注册状态 column header missing')
  assert.match(page, /p\.active/, 'registered[].active no longer rendered')
  assert.match(page, /marketDoc\.activePlugin/, 'active market plugin no longer rendered')
})

console.log(`\nRESULT: ${failures > 0 ? `FAIL — ${failures} check(s) red` : 'PASS — plugins page is a status view: no enable/set writer, handover copy present, modes + registered state still rendered'}`)
if (failures > 0) process.exit(1)
