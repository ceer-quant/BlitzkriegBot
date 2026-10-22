/**
 * 显示账户 — the display-account identity gate (issue 559, amended issue 561).
 *
 * The contract (主会话钉死的合同，内核侧并行实现): the snapshot carries a
 * top-level `displayAccount` (account id) + `displayVenue` (that account's
 * venue, '' = unbound); a new gateway verb `display.account.set` with
 * payload `{accountId}` persists the accounts.toml top-level
 * `display_account` key and applies it at runtime.
 *
 * issue 561 amendment (operator ruling 2026-10-20, verbatim:
 * 「账户切换只保留唯一可信源 display account，行情 策略 风控 账户切换全部
 * 跟随 display account」): the display account is PROMOTED from the 行情页
 * identity fact to the PANEL-WIDE single trusted account source — the
 * switch action IS the display write, and every consumer (Header, 行情
 * 过滤, 策略归属, 流水卡) reads it. Two issue-559-era assertions are
 * REWRITTEN accordingly (each names the ruling inline):
 *   · the store's displayAccount/displayVenue are now a writable ref pair
 *     (snapshot mirror + optimistic write on switch) instead of a
 *     read-only computed;
 *   · the switcher's display write is no longer fire-and-forget-silent —
 *     a failed display write IS a failed switch, rolled back and reported
 *     inline (可见提示，不再静默 catch).
 *
 * Reverse acceptance — the following must be impossible on the UI:
 *   · a consumer reading the SESSION account (the switcher's private
 *     doc.active) while the display source is in force — the double-state
 *     double-clock defect returns;
 *   · a switch that mutates trading/strategy state because of the display
 *     write (切换不启停任何东西 still holds — see account-switch-purity);
 *   · a failed switch silently swallowed (the operator must SEE it);
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

// ── 2. the store: issue 561 可写唯一源（快照镜像 + 切户乐观写）─────────────
check('store 持有可写 displayAccount/displayVenue（快照镜像 + setDisplayAccount 乐观写）', () => {
  // issue 561 裁决改写 issue 559 的「只读 computed」断言：display 升格为
  // 面板唯一可信账户源后，store 必须能被切换动作乐观写（先写内核成功才
  // 写 store），轮询每拍镜像内核读数。
  assert.match(store, /const displayAccount = ref<string>\(''\)/, 'store 缺可写 displayAccount ref')
  assert.match(store, /const displayVenue = ref<string>\(''\)/, 'store 缺可写 displayVenue ref')
  assert.match(store, /function setDisplayAccount\(id: string\): void/, 'store 缺 setDisplayAccount 乐观写入口')
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

// ── 4. the switcher write (issue 561: 切换 = 写 display，先内核后乐观) ──────
check('switchTo 先写 display.account.set，成功才乐观写 store（失败回滚可见）', () => {
  const body = switcher.match(/async function switchTo\([\s\S]*?\n\}\)/)
  assert.ok(body, '缺 switchTo')
  // issue 561：切换动作就是 display 写 —— api.displayAccountSet 必须在
  // switchTo 主路径上 await（不再 fire-and-forget），成功才 setDisplayAccount。
  assert.match(body[0], /await api\s*\n?\s*\.displayAccountSet|await api\.displayAccountSet/, '切换主路径缺 await 的 display 写')
  assert.match(body[0], /store\.setDisplayAccount\(/, '切换成功路径缺乐观写 store.setDisplayAccount')
  // 失败可见：inline 错误 + 回滚，静默 catch 即 FAIL。
  assert.match(body[0], /switchErr\.value =/, '切换失败未落 inline 错误（静默吞掉 = 操作员看不见拒绝）')
  assert.doesNotMatch(body[0], /catch \(\) => \{[^}]*\}\s*\)\s*$/, '静默 catch 复活 —— issue 561 要求可见提示')
  // Header 与选中勾读 store 的 display 源，不再读私有 doc.active。
  assert.doesNotMatch(switcher, /const active = computed\(\(\) => doc\.value\?\.active/, 'Header 仍读私有 doc.active —— 双状态双节拍回归')
  assert.match(switcher, /const active = computed\(\(\) => store\.displayAccount/, 'Header 未接 store.displayAccount（唯一可信源）')
  // 旧 account <id> 文本动词不再被面板调用（内核保留该动词，CLI/门禁兼容面）。
  assert.doesNotMatch(body[0], /command\(`account \$\{id\}`\)|command\('account '/, 'switchTo 仍调用 account.switch 文本动词 —— issue 561 移除（会话格不再被面板触碰）')
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
