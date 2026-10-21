/**
 * 显示账户 — the display-account identity gate (issue 559).
 *
 * The contract (主会话钉死的合同，内核侧并行实现): the snapshot carries a
 * top-level `displayAccount` (account id) + `displayVenue` (that account's
 * venue, '' = unbound); a new gateway verb `display.account.set` with
 * payload `{accountId}` persists the accounts.toml top-level
 * `display_account` key and applies it at runtime.
 *
 * What changed on the panel side and why: the 行情页 countdown card used to
 * derive its identity (badge/name/slot/age) from the SESSION account's
 * bound venue. With the display account becoming an independent kernel
 * fact, that derivation is wrong at exactly one moment — the operator
 * switches the session account while the kernel's display face still runs
 * the old venue: the badge would jump to the new venue while the rows and
 * the round clock below still speak the old one. The identity line now has
 * a UNIQUE source: displayVenue first, account-binding fallback only when
 * the snapshot lacks the field (old cores).
 *
 * Reverse acceptance — the following must be impossible on the UI:
 *   · the countdown card's identity line reading the account-binding chain
 *     while the snapshot speaks displayVenue (the misalignment returns);
 *   · a switch that mutates trading/strategy state because of the display
 *     write (切换不启停任何东西 still holds — see account-switch-purity);
 *   · a display write that blocks or fails the account switch itself
 *     (fire-and-forget: the next snapshot brings the new displayVenue, the
 *     fallback chain covers old cores);
 *   · a fallback decided on an EMPTY displayVenue string — the kernel
 *     speaking (fields present) must always be followed verbatim; only
 *     field ABSENCE (old core) may fall back.
 *
 *   cd ui/webapp/webui && npm run check:display
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

console.log('显示账户 — the display-account identity contract (issue 559)')

const client = read('src', 'api', 'client.ts')
const store = read('src', 'stores', 'panel.ts')
const page = read('src', 'pages', 'HftPage.vue')
const switcher = read('src', 'components', 'AccountSwitcher.vue')
const gateway = read('../../..', 'ui', 'ui_kit', 'src', 'web', 'mod.rs')

// ── 1. the wire contract ─────────────────────────────────────────────────────
check('Snapshot 合同声明 displayAccount/displayVenue（可选字段，老内核缺省）', () => {
  assert.match(client, /displayAccount\?: string \| null/, '快照缺 displayAccount 字段')
  assert.match(client, /displayVenue\?: string \| null/, '快照缺 displayVenue 字段')
})

check('display.account.set 有专用代理方法（/display-account/set，body 逐字转发）', () => {
  assert.match(client, /displayAccountSet:\s*\(body:\s*\{ accountId: string/, 'client.ts 缺 displayAccountSet 方法')
  assert.match(
    client,
    /'\/display-account\/set'/,
    'displayAccountSet 未走 /display-account/set 代理路由',
  )
  const m = client.match(/displayAccountSet:\s*\(body[^`]*?\/display-account\/set',\s*\{[\s\S]*?\n\s*\}\)/)
  assert.ok(m, 'displayAccountSet 方法体残缺')
  assert.match(m[0], /JSON\.stringify\(body\)/, '方法未把 body 逐字 JSON 转发 —— 网关侧零加工的前提')
})

// ── 2. the store: read-only view, no extra fetch ─────────────────────────────
check('store 暴露 displayAccount/displayVenue 纯派生（无写方、无额外拉取）', () => {
  assert.match(store, /const displayAccount = computed\(/, 'store 缺 displayAccount 派生')
  const dv = store.match(/const displayVenue = computed\(\(\) => snapshot\.value\?\.displayVenue \?\? ''\)/)
  assert.ok(dv, 'store 缺 displayVenue 派生（或未直接读快照）')
  // The exports carry both.
  assert.match(store, /displayAccount, displayVenue,/, 'store 导出缺显示账户字段')
})

// ── 3. the identity single source on the 行情页 ──────────────────────────────
check('identityVenue：displayVenue 优先，字段缺省才回落账户绑定链', () => {
  const m = page.match(/const identityVenue = computed<string>\(\(\) => \{[\s\S]*?\n\}\)/)
  assert.ok(m, '缺 identityVenue computed')
  assert.match(m[0], /s\?\.displayVenue \?\? ''/, 'identityVenue 未读快照 displayVenue')
  assert.match(m[0], /store\.activeAccountVenue/, 'identityVenue 缺账户绑定回落 —— 老内核身份行悬空')
  assert.match(
    m[0],
    /displayAccount !== undefined \|\| s\?\.displayVenue !== undefined/,
    '回落判定必须按字段缺省 —— 内核显式空串是「未绑定」，不是「缺字段」',
  )
})

check('倒计时卡身份行（徽章/名/slot 链）只读 identityVenue，旁路即 FAIL', () => {
  assert.match(page, /const marketName = computed\(\(\) => \{[\s\S]*?identityVenue\.value/, 'marketName 未接 identityVenue')
  assert.match(page, /const marketTypeText = computed\(\(\) => \{[\s\S]*?identityVenue\.value/, 'marketTypeText 未接 identityVenue')
  assert.match(page, /const venueRound = computed\(\(\) => \{[\s\S]*?identityVenue\.value/, 'venueRound（钟行选行）未接 identityVenue')
  // No bypass: the three identity consumers must not read activeVenue.
  for (const name of ['marketName', 'marketTypeText', 'venueRound']) {
    const body = page.match(new RegExp(`const ${name} = computed\\(\\(\\) => \\{[\\s\\S]*?\\n\\}\\)`))
    assert.ok(body, `缺 ${name} computed`)
    assert.doesNotMatch(body[0], /activeVenue\.value/, `${name} 仍读账户绑定的 activeVenue —— 唯一源被旁路`)
  }
})

check('重绑扳机挂 identityVenue（身份变化 = 清残值，两源合一）', () => {
  assert.match(page, /watch\(identityVenue,/, '倒计时/Loading 重绑未挂 identityVenue')
})

// ── 4. the switcher write ────────────────────────────────────────────────────
check('切换器成功路径写 display.account.set（fire-and-forget，失败静默）', () => {
  const body = switcher.match(/async function switchTo\([\s\S]*?\n\}\)/)
  assert.ok(body, '缺 switchTo')
  assert.match(body[0], /setDisplayAccount\(/, '切换成功路径缺显示账户写')
  const helper = switcher.match(/function setDisplayAccount\([^)]*\)[\s\S]*?\n\}/)
  assert.ok(helper, '缺 setDisplayAccount helper')
  assert.match(helper[0], /api\s*\n?\s*\.displayAccountSet/, 'helper 未走专用代理方法')
  assert.match(helper[0], /\.catch\(\(\) =>/, '显示账户写失败未静默 —— 会打断切换本身')
})

// ── 5. the gateway proxy route ───────────────────────────────────────────────
check('网关注册 /api/display-account/set 代理路由（display.account.set 逐字转发）', () => {
  assert.match(
    gateway,
    /\("POST", "\/api\/display-account\/set"\)/,
    'ui_kit 网关缺 /api/display-account/set 路由 —— 面板写动词无处落',
  )
  const m = gateway.match(/\("POST", "\/api\/display-account\/set"\) => \{[\s\S]*?\n\s*\}/)
  assert.ok(m, 'display-account/set 路由体残缺')
  assert.match(m[0], /proxy_backtest_body\("display\.account\.set"/, '路由未转发 display.account.set 动词')
  assert.doesNotMatch(m[0], /serde_json::json!|insert\(/, '路由加工了 body —— 代理纪律是逐字转发')
})

// ── 6. comment hygiene (RAW_COLOR / venue-name bans) ─────────────────────────
check('webui 注释用「issue 559」格式（禁 #559 —— RAW_COLOR 门禁误判 hex 色）', () => {
  for (const [file, src] of [['client.ts', client], ['panel.ts', store], ['HftPage.vue', page], ['AccountSwitcher.vue', switcher]]) {
    // Only the comment lines matter; the wire keys are camelCase and can
    // never collide, but scan comments anyway for the tooth to be simple.
    const comments = src.match(/\/\*[\s\S]*?\*\/|\/\/[^\n]*/g) ?? []
    for (const c of comments) {
      assert.doesNotMatch(
        c,
        /#559\b/,
        `${file} 注释出现 #559 形式 —— 会撞 RAW_COLOR 门禁，写「issue 559」`,
      )
    }
  }
})

check('HftPage 注释无具体交易所名（venue 是数据不是逻辑）', () => {
  const banned = ['polymarket', 'kalshi', 'binance', 'okx']
  for (const name of banned) {
    assert.doesNotMatch(page, new RegExp(name, 'i'), `HftPage 出现交易所名 ${name}`)
  }
})

process.exit(failures ? 1 : 0)
