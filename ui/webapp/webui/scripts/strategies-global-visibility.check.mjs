/**
 * 策略全局可见 — the strategies-page global-visibility gate.
 *
 * The user's ruling (2026-10, 账户↔venue 解耦裁决), verbatim anchors:
 *
 *   「策略列表必须完整显示全部已安装策略」
 *   「我们不应该存在为账户分配策略的逻辑……我说的记录账户的策略启用状态
 *     是指 UI 策略内启用按钮的状态，而不是让某个账户只能看见某个策略」
 *
 * Reverse acceptance — the following must be impossible on the UI:
 *   · selecting account B hides strategies that are not enabled in B;
 *   · the rows list shrinks by the active account (per-account assignment
 *     filtering);
 *   · the enable button loses its per-account semantics (启停绑定在
 *     「账户+策略」级别：A 启用的策略 1 在切到 B 后仍由 A 在后台跑).
 *
 * What this gate pins (source-level, following the repo's .check.mjs teeth
 * pattern):
 *   1. Strategies.vue's row list is exactly the store's strategyRows — the
 *      computed carries NO activeAccount term and NO .filter drop.
 *   2. No hasActivity-style derived visibility flag exists.
 *   3. Per-account enable semantics exist: isAccountEnabled (feature-detect
 *      on account.list strategies / strategy.list accounts, null = degrade)
 *      and the accountId-scoped setStrategy call in toggle().
 *   4. The 「N 个账户启用中」 badge (enabledAccountCount) exists.
 *   5. AccountSwitcher stays an enablement reader only: the assignment
 *      editor (分配 / strategiesDraft / saveStrategies) is retired — the
 *      sole toggle entry is the strategies page's button.
 *
 *   cd ui/webapp/webui && npm run check:strategy-visibility
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

console.log('策略全局可见 — the strategies global-visibility contract')

const page = read('src', 'pages', 'Strategies.vue')
const switcher = read('src', 'components', 'AccountSwitcher.vue')

// ── 1. the row list is the full installed-strategy list, unfiltered ─────────
check('策略列表 = store.strategyRows 原样，computed 不含 activeAccount 项', () => {
  const m = page.match(/const rows = computed<StrategyStatsRow\[\]>\(\(\) => ([^\n]*)\)/)
  assert.ok(m, 'Strategies.vue 缺少 rows computed —— 行列表派生被移走或改名')
  assert.equal(
    m[1].trim(),
    'store.strategyRows',
    `rows computed 不是纯 store.strategyRows（出现二次过滤）：${m[1].trim()}`,
  )
})

check('行列表不按 activeAccount 过滤（无 .filter(activeAccount / accounts.includes 行丢弃）', () => {
  // The only legitimate accounts.includes use is READING enablement inside
  // isAccountEnabled — never dropping rows. Row dropping would put a filter
  // between strategyRows and the template's `v-for rows`.
  assert.doesNotMatch(
    page,
    /store\.strategyRows\s*\n?\s*\.filter/i,
    'strategyRows 被二次 .filter —— 按账户过滤复活的入口',
  )
  const rowsSpan = page.match(/const rows = computed[\s\S]*?\n\n/)
  assert.ok(rowsSpan, 'rows computed 块缺失')
  assert.doesNotMatch(
    rowsSpan[0],
    /activeAccount|accounts\.includes/,
    'rows computed 引用了 activeAccount / accounts.includes —— 选择账户 B 会隐藏未在 B 启用的策略',
  )
})

check('无 hasActivity 式可见性派生（旧过滤的遗迹必须保持拆除）', () => {
  assert.doesNotMatch(page, /\bhasActivity\b/, 'hasActivity 复活 —— 活动性过滤行是按账户隐藏的另一入口')
})

// ── 2. per-account enable button semantics exist ─────────────────────────────
check('isAccountEnabled 存在：启用按钮 = 当前账户的启用状态（账户+策略 级别）', () => {
  assert.match(page, /function isAccountEnabled\(/, '缺 isAccountEnabled —— 启用按钮退回全局语义')
})

check('isAccountEnabled 是 feature-detect 读数：strategies 启用集 / accounts 反查，缺失降级 null', () => {
  const body = page.match(/function isAccountEnabled\([\s\S]*?\n\}/)
  assert.ok(body, 'isAccountEnabled 函数体缺失')
  assert.match(body[0], /strategies/, '未读 account.list 行的 strategies 启用集')
  assert.match(body[0], /accounts/, '未读 strategy.list 行的 accounts 反查')
  assert.match(body[0], /null/, '缺失字段时必须返回 null 降级（老核退回全局读数）')
})

check('toggle 按账户定标：setStrategy 传 accountId（scoped），无账户读数退回全局开关', () => {
  const body = page.match(/async function toggle\([\s\S]*?\n\}/)
  assert.ok(body, 'toggle 函数缺失')
  assert.match(
    body[0],
    /api\.setStrategy\(\s*r\.name,\s*next,\s*[^)]*activeAccount/,
    'toggle 未把 accountId 传给 setStrategy —— 启停没有绑定到账户',
  )
})

check('「N 个账户启用中」徽标存在（enabledAccountCount + ×N Badge）', () => {
  assert.match(page, /function enabledAccountCount\(/, '缺 enabledAccountCount')
  assert.match(
    page,
    /<Badge[^>]*>×<RollingNumber/,
    '策略行缺 ×N 徽标（数字必须走 RollingNumber，round-header 门禁同款）',
  )
})

// ── 3. AccountSwitcher: enablement reader only, no assignment editor ────────
check('账户切换器不再有策略分配编辑器（分配/草稿/保存分配 全部退役）', () => {
  for (const relic of ['strategiesDraft', 'saveStrategies', 'toggleDraft', 'loadStrategyOptions', 'strategiesEditing']) {
    assert.doesNotMatch(switcher, new RegExp(`\\b${relic}\\b`), `分配编辑器遗迹 ${relic} 复活 —— 分配逻辑必须保持退役`)
  }
  assert.doesNotMatch(switcher, /保存分配/, '「保存分配」按钮复活 —— 唯一启停入口是策略页按钮')
})

check('账户切换器的 strategies 读数是只读启用集（缺读数如实降级，不发明语义）', () => {
  assert.match(
    switcher,
    /strategiesLabel/,
    '缺 strategiesLabel —— 启用集必须如实展示（N 个已启用 / 跟随全局开关）',
  )
  assert.match(
    switcher,
    /Array\.isArray\(a\.strategies\)/,
    'store.setAccounts 镜像未带 strategies 字段（feature-detect 缺失）',
  )
})

process.exit(failures ? 1 : 0)
