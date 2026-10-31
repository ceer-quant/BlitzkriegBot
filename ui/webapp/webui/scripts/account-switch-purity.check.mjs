/**
 * 切换不启停 — the account-switch purity gate.
 *
 * The user's ruling (2026-10, 账户↔venue 解耦裁决), verbatim anchors:
 *
 *   「策略的启停绑定在'账户+交易所'的级别……必须记录'账户 A 启用了策略 1'」
 *   「即使 UI 当前选中账户 B，账户 A 的策略 1 也必须在后台持续接收 Tick
 *     并交易」
 *
 * issue 561 amendment (operator ruling 2026-10-20, verbatim:
 * 「账户切换只保留唯一可信源 display account，行情 策略 风控 账户切换全部
 * 跟随 display account」): the switch action IS the display write. The
 * purity contract is UNCHANGED in spirit — the switch moment still touches
 * NO trading/strategy surface — but its grammar moved: the one kernel write
 * the switch path may speak is now `display.account.set` (via
 * /api/display-account/set), replacing the old `account <id>` session-verb
 * (the session cell is no longer touched by the panel at all; the kernel
 * keeps the verb for CLI/gate compatibility). The old assertions pinning
 * `account <id>`-as-the-only-verb and api.*-free are REWRITTEN to pin the
 * new grammar; the purity teeth (no stop/disable/freeze/flatten/trading/
 * strategies writes) survive verbatim.
 *
 * Reverse acceptance — the following must be impossible on the UI:
 *   · switching the selected account pauses, disables, freezes or flattens
 *     the OTHER account's running strategies (切换 = 只换视图，不是急停钮).
 *
 * What this gate pins (source-level, following the repo's .check.mjs teeth
 * pattern):
 *   1. AccountSwitcher's switchTo handler body is PURE view-context: the
 *      only kernel write it speaks is the display write (+ reads: accounts
 *      refresh, store refresh). No stop / disable / freeze / flatten /
 *      trading-off / strategies-set call may appear anywhere in it.
 *   2. No `account <id>` session verb on the switch path (issue 561: the
 *      session cell is not the panel's business any more).
 *   3. The switcher's copy states the contract (切换不启停任何东西) so the
 *      operator reads the guarantee the code enforces.
 *   4. issue 781: the account READ is the store's shared-client read, not a
 *      second hand-rolled fetch/projection; and the two polling surfaces
 *      (KlineChart + the panel store's snapshot batch) carry a monotonic
 *      request id plus an account/venue(/symbol) identity capture, so a
 *      late response fetched for the OLD identity can never overwrite the
 *      new account's readings after a switch.
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
const store = read('src', 'stores', 'panel.ts')
const chart = read('src', 'components', 'charts', 'KlineChart.vue')

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

check('switchTo 只切视图上下文：唯一内核写是 display 写（issue 561 语法）', () => {
  // issue 561 改写旧断言（旧语法钉 `account <id>` 会话动词）：切换动作 =
  // 写 display（api.displayAccountSet），不再触碰会话格。本断言继续挡
  // /api/command 文本命令混进切换路径（trading/freeze 等都走 command）。
  const body = switchToBody(switcher)
  const commands = [...body.matchAll(/command\(([^)]*)\)/g)].map((m) => m[1])
  assert.equal(
    commands.length, 0,
    `switchTo 里出现了文本命令调用（切换只写 display，不走 /api/command）：${commands.join(' | ')}`,
  )
  // The one legal kernel write: the display write, awaited (a failed write
  // is a failed switch — see display-account.check.mjs).
  assert.match(
    body,
    /await api\.displayAccountSet/,
    'switchTo 缺 display 写 —— issue 561 下切换动作就是写 display',
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

check('switchTo 不碰会话格（account <id> 会话动词已被 issue 561 移除）', () => {
  const body = switchToBody(switcher)
  assert.doesNotMatch(
    body,
    /command\(\s*[`'"]account[\s`'"]/,
    'switchTo 仍调用 account.switch 文本动词 —— 会话格不再是面板的账户事实源（issue 561）',
  )
})

check('switchTo 的 display 写是纯视图事实（issue 561：写成功才乐观写 store）', () => {
  // issue 561: the display write IS the switch. It stays scoped to the
  // dedicated proxy method and remains display-only — no trading /
  // freeze / strategy surface may ride along with it.
  const body = switchToBody(switcher)
  assert.match(body, /await api\.displayAccountSet/, '切换主路径缺 display 写')
  assert.doesNotMatch(
    body,
    /api\.displayAccountSet\s*\(\s*\{[\s\S]*?\}\s*\)\s*;?\s*\n\s*void/,
    'display 写退回 fire-and-forget —— 失败必须可见（issue 561）',
  )
  assert.match(body, /store\.setDisplayAccount\(/, '切换成功路径缺乐观写 store.setDisplayAccount')
  assert.match(body, /switchErr\.value\s*=/, '切换失败未落 inline 错误（静默吞掉）')
  // display 写是纯显示事实：交易/冻结/策略面不得顺路。
  for (const re of [/trading/i, /freeze/i, /flatten/i, /strategies/i, /setStrategy/]) {
    const line = body.split('\n').find((l) => l.includes('displayAccountSet'))
    assert.ok(line, '缺 display 写行')
    assert.doesNotMatch(line, re, `display 写行混进了非显示写动词（匹配 ${re}）—— 纯视图合同被滥用`)
  }
})

check('switchTo 的写路径只有 display 写；其余是读数（refresh + store.refresh）', () => {
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

// ── issue 781: one client, one projection, no stale write-back ──────────────
check('账户读数走共享 API 客户端：无裸 fetch(/api/command)、无手抄 X-Auth-Token', () => {
  // The shared client (api.command → core.request) is the ONLY place that
  // speaks /api/command or attaches the token. A hand-rolled copy here
  // bypasses the 401 → sessionExpired path and mints a second projection.
  assert.doesNotMatch(
    switcher,
    /fetch\(\s*['"`]\/api\/command/,
    'AccountSwitcher 又自建 fetch(/api/command) —— 绕过共享客户端与统一错误处理',
  )
  assert.doesNotMatch(
    switcher,
    /X-Auth-Token/,
    'AccountSwitcher 手抄 X-Auth-Token —— 令牌头只属于 src/api/core.ts',
  )
  assert.doesNotMatch(
    switcher,
    /\bgetToken\b/,
    'AccountSwitcher 直接读令牌 —— 账户读数必须经共享客户端',
  )
  // The store function is the single source of the account projection.
  assert.match(
    switcher,
    /store\.refreshAccounts\(\)/,
    '切换器的账户读数未走 store.refreshAccounts（唯一投影源）',
  )
  assert.match(
    store,
    /api\.command\('accounts'\)/,
    'store.refreshAccounts 未通过共享 api.command 读账户簿',
  )
  assert.doesNotMatch(
    switcher,
    /accounts\.map\(/,
    '切换器又自行投影账户行 —— 行投影只许留在 store.refreshAccounts',
  )
})

check('K线响应带请求序号 + (账户,venue,symbol) 身份守卫：旧账户/旧所响应静默丢弃', () => {
  assert.match(chart, /let requestSeq = 0/, 'KlineChart 缺单调请求序号（旧响应会覆盖新数据）')
  const identity = chart.match(/function captureIdentity\(\): KlineIdentity \{[\s\S]*?\n\}/)
  assert.ok(identity, 'KlineChart 缺请求时刻的身份捕获')
  assert.match(identity[0], /account: store\.displayAccount/, '身份捕获缺账户')
  assert.match(identity[0], /venue: store\.activeAccountVenue/, '身份捕获缺 venue')
  assert.match(identity[0], /symbol: props\.symbol/, '身份捕获缺 symbol')
  const guard = chart.match(/function responseIsCurrent\([\s\S]*?\n\}/)
  assert.ok(guard, 'KlineChart 缺响应守卫（序号 + 身份比对）')
  assert.match(guard[0], /requestId === requestSeq/, '守卫未比对请求序号')
  assert.match(guard[0], /identity\.account === store\.displayAccount/, '守卫未比对账户身份')
  assert.match(guard[0], /identity\.venue === store\.activeAccountVenue/, '守卫未比对 venue 身份')
  assert.match(guard[0], /identity\.symbol === props\.symbol/, '守卫未比对 symbol 身份')
  const fn = chart.match(/async function refresh\(\): Promise<void> \{[\s\S]*?\n\}/)
  assert.ok(fn, 'KlineChart 缺 refresh')
  assert.match(fn[0], /const requestId = \+\+requestSeq/, 'refresh 未取新请求序号')
  assert.match(fn[0], /const identity = captureIdentity\(\)/, 'refresh 未捕获请求时刻身份')
  const guarded = fn[0].match(/responseIsCurrent\(requestId, identity\)/g) ?? []
  assert.ok(
    guarded.length >= 2,
    `成功与失败路径都必须过守卫（当前只有 ${guarded.length} 处）`,
  )
})

check('store 快照写回按世代 + 身份门控：旧账户的迟到快照不得覆盖新账户读数', () => {
  const setter = store.match(/function setDisplayAccount\(id: string\): void \{[\s\S]*?\n  \}/)
  assert.ok(setter, 'store 缺 setDisplayAccount')
  assert.match(setter[0], /displayEpoch\+\+/, '切换动作未推进世代号 —— 在途旧请求不会作废')
  const accountsFn = store.match(/async function refreshAccounts\(\): Promise<string \| null> \{[\s\S]*?\n  \}/)
  assert.ok(accountsFn, 'store.refreshAccounts 形状变了（本门禁按 token 形状钉住）')
  assert.match(accountsFn[0], /const requestId = \+\+accountsSeq/, '账户簿读数缺请求序号')
  assert.match(accountsFn[0], /const epoch = displayEpoch/, '账户簿读数未捕获世代')
  assert.match(
    accountsFn[0],
    /requestId !== accountsSeq \|\| epoch !== displayEpoch/,
    '账户簿镜像写回未按序号/世代门控',
  )
  const fn = store.match(/async function refresh\(\): Promise<void> \{[\s\S]*?\n  \}/)
  assert.ok(fn, 'store 缺 refresh')
  // The request id + captured identity (no volatile fields in the key).
  assert.match(fn[0], /const requestId = \+\+refreshSeq/, '快照批次缺单调请求序号')
  assert.match(fn[0], /const epoch = displayEpoch/, '快照批次未捕获世代')
  assert.match(fn[0], /const identityAccount = displayAccount\.value/, '快照批次未捕获账户身份')
  assert.match(fn[0], /const identityVenue = activeAccountVenue\.value/, '快照批次未捕获 venue 身份')
  assert.match(fn[0], /requestId === refreshSeq/, '写回守卫未比对请求序号')
  assert.match(fn[0], /epoch === displayEpoch/, '写回守卫未比对世代')
  assert.match(fn[0], /identityAccount === displayAccount\.value/, '写回守卫未比对账户身份')
  assert.match(fn[0], /identityVenue === activeAccountVenue\.value/, '写回守卫未比对 venue 身份')
  // 首拍身份未知（''）时发起的是全局读数，采用默认账户后仍须落地 ——
  // 不然首屏会被自家账户簿镜像作废，空等 15s 的下一拍。
  assert.match(fn[0], /const identityKnown = identityAccount !== ''/, '缺「身份未知 = 全局读数」判定')
  assert.match(
    fn[0],
    /!identityKnown \|\| identityAccount === displayAccount\.value/,
    '首拍身份未知时被自家镜像作废（首屏空等下一拍）',
  )
  // Every snapshot / account-scoped write rides the same one-shot verdict.
  assert.match(fn[0], /const current = isCurrent\(\)/, '写回裁决未一次性取定（内部身份写会误伤本拍）')
  assert.match(
    fn[0],
    /if \(snap\.status === 'fulfilled' && current\)/,
    '快照写回未过守卫 —— 旧账户快照可覆盖新读数',
  )
  assert.match(
    fn[0],
    /if \(plug\.status === 'fulfilled' && current\)/,
    '插件写回未过守卫',
  )
  assert.match(
    fn[0],
    /if \(current\) \{[\s\S]*?accountTradeSummary\.value/,
    '账户聚合读数写回未过守卫',
  )
  assert.match(
    fn[0],
    /if \(current\) \{[\s\S]*?accountStrategyStats\.value/,
    '账户策略统计写回未过守卫',
  )
})

process.exit(failures ? 1 : 0)
