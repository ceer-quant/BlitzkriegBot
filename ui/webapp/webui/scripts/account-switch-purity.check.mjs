/**
 * 切换不启停 — the account-switch purity gate.
 *
 * The user's ruling (2026-10, 账户↔venue 解耦裁决), verbatim anchors:
 *
 *   「策略的启停绑定在'账户+交易所'的级别……必须记录'账户 A 启用了策略 1'」
 *   「即使 UI 当前选中账户 B，账户 A 的策略 1 也必须在后台持续接收 Tick
 *     并交易」
 *
 * Reverse acceptance — the following must be impossible on the UI:
 *   · switching the selected account pauses, disables, freezes or flattens
 *     the OTHER account's running strategies (切换 = 只换视图，不是急停钮).
 *
 * What this gate pins (source-level, following the repo's .check.mjs teeth
 * pattern):
 *   1. AccountSwitcher's switchTo handler body is PURE view-context: the
 *      only kernel verb it speaks is `account <id>` (+ reads: accounts
 *      refresh, store refresh). No stop / disable / freeze / flatten /
 *      trading-off / strategies-set call may appear anywhere in it.
 *   2. The switch path contains no api.* command calls at all — commands
 *      other than the bare `account <id>` switch belong to explicit user
 *      actions on rows, never to the switch moment.
 *   3. The switcher's copy states the contract (切换不启停任何东西) so the
 *      operator reads the guarantee the code enforces.
 *
 * Note the granularity: the ban applies to the switchTo function body and
 * everything it alone calls — NOT to the file's per-row actions (askTrading /
 * doRemove are explicit operator clicks on a row, reachable without any
 * switch, and are not part of the switch moment).
 *
 *   cd ui/webapp/webui && npm run check:switch-purity
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

console.log('切换不启停 — the account-switch purity contract')

const switcher = read('src', 'components', 'AccountSwitcher.vue')

/** The switchTo body (balanced braces from the `async function switchTo` line). */
function switchToBody(src) {
  const start = src.indexOf('async function switchTo(')
  assert.ok(start >= 0, '缺 switchTo 处理器 —— 账户切换没有落点')
  let depth = 0
  let i = src.indexOf('{', start)
  for (; i < src.length; i++) {
    if (src[i] === '{') depth++
    else if (src[i] === '}') {
      depth--
      if (depth === 0) break
    }
  }
  return src.slice(start, i + 1)
}

check('switchTo 只切视图上下文：唯一内核命令是 account <id> 本身', () => {
  const body = switchToBody(switcher)
  const commands = [...body.matchAll(/command\(([^)]*)\)/g)].map((m) => m[1])
  assert.ok(
    commands.length >= 1 && commands.every((c) => c.includes('`account ${id}`') || c.includes("'account '")),
    `switchTo 里出现了 account 切换以外的内核命令：${commands.join(' | ')}`,
  )
})

check('switchTo 不含任何启停/冻结/平仓/分配调用（reverse acceptance 的牙）', () => {
  const body = switchToBody(switcher)
  const banned = [
    /stop/i, /disable/i, /freeze/i, /flatten/i,
    /trading\s+off|trading\s+false/i,
    /account\.trading/, /account\.strategies/, /strategies\.set/,
    /setStrategy/, /askTrading/, /doRemove/, /unified\.revoke/,
    /strategy\s+\w+\s+off/,
  ]
  for (const re of banned) {
    assert.doesNotMatch(body, re, `switchTo 内出现禁用调用（匹配 ${re}）—— 切换停掉了别的账户的策略`)
  }
})

check('switchTo 无 api.* 命令面调用（切换时刻只读 + 切账户，不碰任何写动词）', () => {
  const body = switchToBody(switcher)
  assert.doesNotMatch(
    body,
    /\bapi\.[A-Za-z]/,
    'switchTo 直接调用了 api.* —— 切换时刻不应触碰命令面（读数走 refresh/store.refresh；显示账户写走 setDisplayAccount helper，issue 559）',
  )
})

check('switchTo 的 display-account 写是纯视图事实（issue 559 裁决：写完不启停任何东西）', () => {
  // issue 559: a successful switch also fires `display.account.set`
  // (fire-and-forget) so the 行情页 identity follows the session account.
  // The exemption is SCOPED: the verb may appear only through the named
  // helper, and the helper must stay a pure display write — no trading /
  // freeze / strategy surface may ride along with it.
  const body = switchToBody(switcher)
  assert.match(body, /setDisplayAccount\(/, '切换成功路径缺显示账户写 —— 行情身份与切换器会错位（issue 559）')
  assert.doesNotMatch(
    body,
    /api\.displayAccountSet/,
    'switchTo 内联调 api.displayAccountSet —— 显示账户写必须走独立 helper（保持切换路径可审计）',
  )
  const helper = switcher.match(/function setDisplayAccount\(\)\s*: void \{[\s\S]*?\.catch[\s\S]*?\n\}/)
    ?? switcher.match(/function setDisplayAccount\([^)]*\)\s*(?:: void)? \{[\s\S]*?\n\}/)
  assert.ok(helper, '缺 setDisplayAccount helper')
  assert.match(helper[0], /displayAccountSet/, 'helper 未走专用代理方法（/api/display-account/set 的前端面）')
  assert.match(helper[0], /catch/, '显示账户写不是 fire-and-forget（失败会打断切换本身）')
  // The helper is display-only: the trading/strategy surfaces stay banned.
  for (const re of [/trading/i, /freeze/i, /flatten/i, /strategies/i, /setStrategy/]) {
    assert.doesNotMatch(helper[0], re, `setDisplayAccount 里混进了非显示写动词（匹配 ${re}）—— issue 559 的豁免被滥用`)
  }
})

check('switchTo 的写路径只有切账户命令；其余是读数（refresh + store.refresh）', () => {
  const body = switchToBody(switcher)
  assert.match(body, /await refresh\(\)/, '切换后未刷新账户读数')
  assert.match(body, /store\.refresh\(\)/, '切换后未刷新 store（视图上下文不完整）')
})

check('切换器的界面文案如实声明合同（切换不启停任何东西）', () => {
  assert.match(
    switcher,
    /切换不启停|持续运行|后台继续/,
    '切换器缺「切换不启停」文案 —— 操作员必须能读到这条保证',
  )
})

check('全文件无「切账户时顺手停策略」的隐藏入口（watch activeAccount 不得带写命令）', () => {
  // A watcher on the active account that fires writes would be the same
  // violation wearing a disguise. The switcher must not have one.
  const watches = [...switcher.matchAll(/watch\(\s*\(\)\s*=>\s*(?:props\.)?active[\s\S]*?\n\}\)/g)]
  assert.ok(watches.length >= 0, 'unreachable')
  for (const w of watches) {
    assert.doesNotMatch(
      w[0],
      /\bapi\.[A-Za-z]+|command\(/,
      'watch(active*) 里藏着命令调用 —— 切换事件的又一个停策略入口',
    )
  }
})

process.exit(failures ? 1 : 0)
