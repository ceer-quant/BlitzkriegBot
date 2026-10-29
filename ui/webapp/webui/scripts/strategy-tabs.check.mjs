/**
 * 策略表现 tab 化 — the strategy-tabs gate（单所/套利双段 + 套利收益 feature-detect）。
 *
 * The user's ruling (2026-10), verbatim anchors:
 *   「把策略表现部分 tab 化：单交易所策略 和 套利策略（套利策略面板仅限被
 *     设置套利的两个账户可见）UI 格式继续参考普通策略列表。另外策略列表
 *     的全部内容都应该跟随账户」
 *
 * Reverse acceptance — the following must be impossible on the UI:
 *   · 对非腿账户渲染 arb 段（可见性门 = configured && 当前显示账户是 A/B
 *     腿账户之一；不可见是「段消失」，不是禁用置灰）。
 *   · 行切分失败：cross_venue_arb 行落进单所 tab，或单所策略落进套利 tab。
 *   · arb 收益字段消费不做 feature-detect（老内核缺 arbNetPnlUsd 等 → 必须回落
 *     行自身值，缺 arbActivePairs → 不渲染徽标）。
 *   · 弹窗策略段提交一个内核 arb.config.set 不会收的策略名（唯一 [cross_venue_arb]
 *     节）—— 不假提交，未支持即置灰。
 *
 * What this gate pins (source-level, following the repo's .check.mjs teeth
 * pattern):
 *   1. SegmentedControl 双段：'single' 单交易所策略 / 'arb' 套利策略，badge=行数。
 *   2. 可见性门：arbConfigGet 读一次；arbArbVisible 谓词消费 displayAccount 与
 *      legAAccount/legBAccount；读失败回落单所（照实降级不猜）。
 *   3. 行切分：ARB_STRATEGY 单实例合同 + 切分谓词，套利 tab 净 PnL 消费
 *      arbNetPnlUsd ?? netPnlUsd。
 *   4. 「创建套利」按钮可达性：非腿账户也必须能开弹窗（可达性优先，按钮
 *      在 CardHeader 共用）。
 *
 *   cd ui/webapp/webui && npm run check:strategy-tabs
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

console.log('策略表现 tab 化 — the strategy-tabs contract')

const page = read('src', 'pages', 'Strategies.vue')
const client = read('src', 'api', 'snapshot.ts')

// ── 1. 双段 SegmentedControl ────────────────────────────────────────────────
check('策略表现卡内有 SegmentedControl 双段（单所 / 套利）', () => {
  assert.match(page, /import SegmentedControl from/, '缺 SegmentedControl import')
  assert.match(page, /v-model="perfTab"/, '缺 perfTab v-model —— tab 状态必须是有名字的单一事实')
  assert.match(page, /id: 'single'/, "缺 'single' 段（单交易所策略）")
  assert.match(page, /label: '单交易所策略'/, "缺「单交易所策略」段标签")
  assert.match(page, /id: 'arb'/, "缺 'arb' 段（套利策略）")
  assert.match(page, /label: '套利策略'/, "缺「套利策略」段标签")
})

check('两段 badge = 各自的行数', () => {
  assert.match(page, /singleRows\.length/, '单所段 badge 不是行数')
  assert.match(page, /arbRows\.length/, '套利段 badge 不是行数')
})

// ── 2. 可见性门 ─────────────────────────────────────────────────────────────
check('挂载时读一次 arbConfigGet（api.arbConfigGet 消费在场）', () => {
  assert.match(page, /api\s*\.\s*arbConfigGet\(\)/, '页面缺 arbConfigGet 读数 —— 可见性门无从判定')
})

check('arb 段可见性 = configured && 显示账户是腿账户（arbTabVisible 谓词在场）', () => {
  assert.match(page, /arbTabVisible/, '缺 arbTabVisible 谓词 —— 可见性门必须是有名字的判定')
  const m = page.match(/const arbTabVisible = computed[\s\S]*?\n\}\)/)
  assert.ok(m, 'arbTabVisible 不是 computed')
  assert.match(m[0], /arbConfigured/, '可见性判据缺 configured（未配置的套利不能有 tab）')
  assert.match(m[0], /legAAccount|legBAccount|arbLegAccounts/, '可见性判据缺腿账户比对')
  assert.match(m[0], /displayAccount/, '可见性判据缺显示账户（issue 561 唯一可信账户源）')
})

check('arb 段渲染是条件门（不可见 = 段消失，不是禁用）', () => {
  // 两种合法拼法：模板 v-if 门，或 :segments 里的条件展开（段项缺席）。
  // 禁用置灰（:disabled）两种都不匹配 —— 裁决是消失，不是置灰。
  assert.match(
    page,
    /v-if="arbTabVisible"|\.\.\.\(\s*arbTabVisible\s*\?/,
    'arb 段不是条件渲染 —— 禁用置灰不满足「不可见即消失」的裁决',
  )
})

check('段消失时选中 tab 拨回单所（悬空 arb id = 高亮变黑 + 行过滤泄漏）', () => {
  assert.match(
    page,
    /watch\(arbTabVisible/,
    '缺 arbTabVisible watch —— 腿账户切走后 perfTab 悬停在 arb，段没了高亮匹配也没了',
  )
  assert.match(
    page,
    /perfTab\.value === 'arb'\)?\s*perfTab\.value = 'single'/,
    '拨回目标不是 single',
  )
})

// ── 3. 行切分 ───────────────────────────────────────────────────────────────
check('ARB_STRATEGY 单实例合同（平台化时换配置驱动的 ponytail 注释在场）', () => {
  assert.match(page, /const ARB_STRATEGY = 'cross_venue_arb'/, '缺 ARB_STRATEGY 单实例常量')
  assert.match(
    page,
    /ARB_STRATEGY = 'cross_venue_arb'[^\n]*平台化/,
    'ARB_STRATEGY 行缺「平台化」注释 —— 换配置驱动时的替换点必须可寻',
  )
})

check('行切分谓词：单所 = 非 ARB_STRATEGY 行，套利 = ARB_STRATEGY 行', () => {
  const m = page.match(/const singleRows = computed[\s\S]*?\n\}\)/)
  assert.ok(m, '缺 singleRows computed')
  assert.match(m[0], /ARB_STRATEGY/, '单所切分未消费 ARB_STRATEGY')
  const a = page.match(/const arbRows = computed[\s\S]*?\n\}\)/)
  assert.ok(a, '缺 arbRows computed')
  assert.match(a[0], /ARB_STRATEGY/, '套利切分未消费 ARB_STRATEGY')
})

check('搜索框只过滤当前 tab 行（filteredRows 消费当前 tab 集合）', () => {
  const m = page.match(/const filteredRows = computed[\s\S]*?\n\}\)/)
  assert.ok(m, '缺 filteredRows computed')
  assert.match(m[0], /arbRows|singleRows/, '搜索过滤没有作用于 tab 行集合 —— 搜索框越过 tab 边界')
})

check('表格 v-for 消费分组后的当前 tab 行（groups 谓词跟随 tab）', () => {
  const m = page.match(/const groups = computed\(\(\) =>\s*\[[\s\S]*?\]\.filter\(\(g\) => g\.rows\.length > 0\)/)
  assert.ok(m, 'groups computed 缺失')
  assert.match(m[0], /filteredRows/, 'groups 不消费 filteredRows —— tab 切分与表格脱钩')
})

// ── 4. 套利收益（feature-detect 回落）────────────────────────────────────────
check('套利 tab 净 PnL 消费 arbNetPnlUsd ?? netPnlUsd（feature-detect 回落）', () => {
  assert.match(
    page,
    /arbNetPnlUsd \?\? r\.netPnlUsd|arbNetPnlUsd\s*\?\?\s*r\.netPnlUsd/,
    '净 PnL 列缺 arbNetPnlUsd ?? netPnlUsd 回落 —— 老内核零破坏的合同点',
  )
})

check('活跃配对徽标：arbActivePairs 缺失不渲染（feature-detect）', () => {
  assert.match(page, /arbActivePairs/, '缺 arbActivePairs 消费')
  const m = page.match(/v-if="r\.arbActivePairs[^"]*"/)
  assert.ok(m, '活跃配对徽标缺 v-if feature-detect 门 —— 字段缺失必须不渲染')
})

check('原因列：徽标与展开按钮同一条 flex 队列，不靠内容长度碰运气', () => {
  // 2026-10 截图重叠修复：「N 对」徽标与盾牌/下拉按钮曾裸排在 td 里 —— 没有
  // flex、没有 gap、徽标不可收缩，窄列（7%）下内容一长就压到按钮上。结构修法
  // 钉三件事：flex 容器 + gap 保底间距；按钮 shrink-0（控件永不被压）；徽标
  // min-w-0 shrink（可收缩，极窄时 flex-wrap 兜底换行而不是叠字）。
  const cell = page.match(/<td class="px-2 py-2\.5 text-right">\s*<!-- 原因列[\s\S]*?<\/td>/)
  assert.ok(cell, '原因列 td 缺失')
  assert.match(cell[0], /flex flex-wrap items-center justify-end gap-x-1\.5/, '徽标与按钮没有排进同一条 flex 队列（gap 缺失）')
  assert.match(cell[0], /class="shrink-0"\s+variant="ghost"/, '展开按钮必须 shrink-0 —— 控件被压缩就是新的重叠面')
  assert.match(cell[0], /max-w-full min-w-0 shrink whitespace-nowrap/, '徽标缺收缩约束 —— 不可收缩就会重新压到按钮上')
})

check('client.ts StrategyStatsRow 带五个可选 arb 字段（老内核缺省 = undefined）', () => {
  for (const f of ['arbNetPnlUsd', 'arbWins', 'arbLosses', 'arbClosedTrades', 'arbActivePairs']) {
    assert.match(
      client,
      new RegExp(`${f}\\?:`),
      `StrategyStatsRow 缺可选字段 ${f} —— 内核分支并行交付，前端只做 feature-detect`,
    )
  }
})

// ── 5. 创建套利按钮可达性 ─────────────────────────────────────────────────────
check('「创建套利」按钮在 CardHeader action（两 tab 共用，可达性优先）', () => {
  const header = page.match(/<CardHeader label="策略表现">[\s\S]*?<\/CardHeader>/)
  assert.ok(header, '策略表现 CardHeader 缺失')
  assert.match(header[0], /arbOpen = true/, '创建套利按钮不在 CardHeader —— 非腿账户也会失去唯一的弹窗入口')
})

check('arb 行数徽标读数在无 arb 行时也不崩（空表回落 EmptyState 在场）', () => {
  assert.match(page, /没有匹配的策略/, '表格空态缺失 —— 套利 tab 无行时需要与单所一致的空态')
})

process.exit(failures ? 1 : 0)
