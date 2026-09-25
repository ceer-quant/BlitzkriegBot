/**
 * VERSIONING.md V6-5 — the WebUI half of the version/update contract, executable.
 *
 * The Settings card renders facts the KERNEL owns, and the two ways this could
 * rot silently are exactly the two the contract names:
 *   1. the THREE-STATE badge collapsing: "not checked" (null) drawn as
 *      "up to date" — the INV-3 lie, on the UI side (N16);
 *   2. an old core's refusal being papered over with a fabricated version
 *      instead of the honest "this kernel does not speak system.version" (§5.6).
 *
 * Static like its sibling checks: it parses the sources, not the pixels. The
 * behavioural half (a real core answering, three states over the wire) is the
 * provenance gate's job (`scripts/core-provenance-check.mjs`).
 *
 *   cd ui/webapp/webui && npm run check:version-panel
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

const versionLib = read('src', 'lib', 'version.ts')
const settingsPage = read('src', 'pages', 'SettingsPage.vue')
const client = read('src', 'api', 'client.ts')

console.log('三态：null ≠ false，徽章不许撒谎（N16）')

check('versionBadge 是唯一的徽章实现，且三个状态都可分辨', () => {
  assert.ok(versionLib.includes('export function versionBadge'), 'versionBadge 缺失')
  // The three states, spelled out and distinguishable — in THIS order the null
  // check must come before any boolean reading, or `=== false` semantics leak.
  const body = versionLib.slice(versionLib.indexOf('export function versionBadge'))
  assert.match(body, /updateAvailable === null/, 'null（未检查）必须有专属分支')
  assert.match(body, /updateAvailable === false/, 'false（已是最新）必须有专属分支')
  assert.match(body, /未检查/, '未检查的文案缺失')
  assert.match(body, /已是最新/, '已是最新的文案缺失')
  assert.match(body, /可更新/, '可更新的文案缺失')
})

check('设置页渲染徽章但不自己解三态（判据同源，不各写一份）', () => {
  assert.ok(settingsPage.includes('versionBadge('), '设置页没有走 versionBadge')
  // A second, hand-rolled ternary over updateAvailable in the page would be the
  // copy that drifts; the badge text may only come from the shared helper.
  const pageBadge = settingsPage.slice(settingsPage.indexOf('versionBadge('))
  assert.doesNotMatch(
    pageBadge,
    /updateAvailable\s*===/,
    '设置页内联了三态判断 —— 回到 lib/version.ts',
  )
})

console.log('旧内核容错：宁说「不认识」，不编版本号（§5.6）')

check('读不到 systemVersion 时画「不可用」，不画空版本', () => {
  assert.ok(
    settingsPage.includes('不可用'),
    'systemVersion 缺失时的「不可用」状态缺失',
  )
  assert.ok(
    settingsPage.includes('不认识'),
    '旧内核（不认识 system.version）的明确文案缺失',
  )
  // The card must not render `version.version` when there is no version doc —
  // the empty state guards the whole detail block.
  assert.match(settingsPage, /v-else-if="!version"/, '缺 version 文档时没有整块兜底')
})

console.log('开关与默认：关闭要可见，动作不许绕过内核')

check('「检查更新」按钮在 checkEnabled=false 时禁用并说明', () => {
  assert.match(
    settingsPage,
    /checkEnabled/,
    '按钮没有读到内核的 checkEnabled',
  )
  assert.ok(settingsPage.includes('检查已关闭'), '「检查已关闭」的说明文案缺失')
})

check('自动更新开关的值来自内核，写盘失败要报错', () => {
  // The switch binds the KERNEL's autoUpdate (snapshot), not a local ref.
  assert.match(settingsPage, /:model-value="version\.autoUpdate"/, '开关没有绑定内核的 autoUpdate')
  assert.ok(client.includes("'/version/configure'"), 'configure 路由缺失')
  assert.ok(client.includes("'/version/check'"), 'check 路由缺失')
  // configure 的调用方必须把 error 展示出来 —— 静默回退的开关比没有更糟（§7.4）。
  const toggle = settingsPage.slice(settingsPage.indexOf('toggleAutoUpdate'))
  assert.ok(toggle.includes('versionErr.value'), 'configure 失败没有落到可见的错误条')
})

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`)
process.exit(failures === 0 ? 0 : 1)
