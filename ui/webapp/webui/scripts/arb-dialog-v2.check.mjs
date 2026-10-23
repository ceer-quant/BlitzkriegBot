/**
 * 配置意图模型 — the arb-dialog v2 gate（跨所套利创建弹窗的四段结构合同）。
 *
 * The user's ruling (2026-10 配置意图模型裁决), verbatim anchors:
 *   「选 A 腿(交易所+账户) + B 腿(交易所+账户) + 币对多选 → 创建即持久化」
 *
 * Reverse acceptance — the following must be impossible on the UI:
 *   · 「选择目标映射」/「已配对、无差异标记」概念回来（它等一个 live 链路
 *     产不出的候选 —— 内核候选机制在单所喂入批上死锁，弹窗永远无法完成
 *     创建）。unified.candidates / unified.confirm / unified.revoke 不再是
 *     本弹窗的数据源。
 *   · 文案出现「执行账户」「记账账户」—— 账户是腿的属性（A 腿账户 / B 腿
 *     账户），双腿双账户模型里没有单一的「执行账户」。
 *   · 对具体交易所名的分支（venue 是数据 —— no-venue-branch 纪律沿用
 *     venue-presentation gate 的判定形状）。
 *   · 老内核（arb.config.get feature-detect 失败）降级回旧的单账户单选 ——
 *     降级面只能是只读提示 + 禁用创建。
 *
 * What this gate pins (source-level, following the repo's .check.mjs teeth
 * pattern):
 *   1. 弹窗的四段锚点：A 腿 / B 腿 / 币对（静态候选集披露）/ 确认页只读汇总。
 *   2. 提交序：arb.config.set 成功在前，setStrategy 启用在后（A 腿账户作用域，
 *      策略名 = 弹窗策略段选中的策略）。
 *   3. 硬门在前端先拦：B 交易所 ≠ A、B 账户 ≠ A —— 两道门的判据与文案。
 *   4. 账户过滤在源头：venue 匹配（含绑定留空 = 任意交易所）+ 可交易状态
 *      白名单（read_only / frozen 不进列表）。
 *   5. client.ts 的 wire 面：arbConfigGet / arbConfigSet 走专用代理路由。
 *   6. 去标题化（2026-10）：标题是中性词「创建套利」，cross_venue_arb 只是
 *      策略名不是套利本身；弹窗首段是策略选择（静态单选项 + 未支持不假提交）。
 *
 *   cd ui/webapp/webui && npm run check:arb-dialog
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

console.log('配置意图模型 — the arb-dialog v2 contract')

const dialog = read('src', 'components', 'CreateArbDialog.vue')
const client = read('src', 'api', 'client.ts')

// ── 四段结构锚点 ────────────────────────────────────────────────────────────
check('弹窗有 A 腿段（交易所 + 账户两个下拉）', () => {
  assert.match(dialog, /label-micro">A 腿</, '缺 A 腿段标题')
  assert.match(dialog, /v-model="legAVenue"/, 'A 腿段缺 legAVenue 绑定（交易所下拉）')
  assert.match(dialog, /v-model="legAAccount"/, 'A 腿段缺 legAAccount 绑定（账户下拉）')
})

check('弹窗有 B 腿段（交易所排除 A + 账户）', () => {
  assert.match(dialog, /label-micro">B 腿</, '缺 B 腿段标题')
  assert.match(dialog, /legBVenueOptions/, 'B 腿交易所下拉没有独立的排除后选项集')
  assert.match(
    dialog,
    /venueOptions\.value\.filter\(\(v\) => v !== legA/,
    'B 腿选项集未排除 A 腿已选交易所（B≠A 硬门之一）',
  )
})

check('币对段是多选且候选集披露为面板静态清单', () => {
  assert.match(dialog, /type="checkbox"/, '币对不是复选框组')
  assert.match(dialog, /PAIR_CANDIDATES/, '缺币对候选集常量')
  assert.match(
    dialog,
    /候选集为面板静态清单/,
    '确认页缺静态候选集披露 —— 面板不得谎称候选集是内核读数',
  )
})

check('确认页是只读汇总（两腿账户@venue + 凭证状态 + 币对清单）', () => {
  assert.match(dialog, /label-micro">币对清单</, '确认页缺币对清单块')
  assert.match(dialog, /凭证状态/, '确认页缺 per-leg 凭证状态')
  assert.match(dialog, /accountName\(legAAccount\)/, '确认页缺 A 腿账户汇总')
  assert.match(dialog, /accountName\(legBAccount\)/, '确认页缺 B 腿账户汇总')
})

// ── 旧模型概念整体作废 ──────────────────────────────────────────────────────
check('「选择目标映射」「已配对、无差异标记」概念已删', () => {
  for (const banned of [/选择目标映射/, /已配对、无差异标记/]) {
    assert.doesNotMatch(dialog, banned, `旧映射段文案复活（匹配 ${banned}）`)
  }
  for (const verb of ['unifiedCandidates', 'unifiedConfirm', 'unifiedRevoke', 'unifiedList']) {
    assert.doesNotMatch(
      dialog,
      new RegExp(`api\\.${verb}`),
      `弹窗仍调用 ${verb} —— 候选机制已定谳死锁，不是数据源`,
    )
  }
})

check('文案纪律：无「执行账户」「记账账户」', () => {
  for (const banned of [/执行账户/, /记账账户/]) {
    assert.doesNotMatch(dialog, banned, `违禁文案（匹配 ${banned}）—— 账户是腿的属性`)
  }
})

// ── 硬门在前端先拦 ──────────────────────────────────────────────────────────
check('B 账户 ≠ A 账户的硬门判据与文案在弹窗里', () => {
  assert.match(dialog, /sameAccountClash/, '缺 B=A 账户冲突判据')
  assert.match(dialog, /B 腿账户不能与 A 腿相同/, '缺 B≠A 账户的门文案')
})

check('账户过滤在源头：venue 匹配 + 只读/冻结状态不进列表', () => {
  const src = dialog
  assert.match(
    src,
    /bound === ''\s*\|\|\s*bound === venue/,
    '账户过滤缺 venue 匹配（绑定留空 = 可交易任意交易所）',
  )
  assert.match(
    src,
    /read_only|frozen/,
    '账户过滤缺可交易状态判定（read_only/frozen 不进列表）',
  )
  assert.match(src, /tradable\(/, '状态判定不是共享谓词（过滤与说明会分叉）')
})

check('换交易所清空该腿账户选择；唯一匹配自动预选', () => {
  // 用户动作走 @change 清空/预选（程序化预填不被 watch 打掉）。
  assert.match(dialog, /@change="onLegAVenueChange"/, 'A 腿交易所缺 @change 处理')
  assert.match(dialog, /@change="onLegBVenueChange"/, 'B 腿交易所缺 @change 处理')
  assert.match(
    dialog,
    /opts\.length === 1 \? opts\[0\]\.id : ''/,
    '缺唯一匹配自动预选（两腿 @change 与 watch 共用同一规则）',
  )
  assert.match(dialog, /watch\(\[legAVenue, \(\) => store\.accounts\]/, 'A 腿缺账户簿晚到/清空后的自动预选 watch')
  assert.match(dialog, /watch\(\[legBVenue, \(\) => store\.accounts\]/, 'B 腿缺账户簿晚到/清空后的自动预选 watch')
})

// ── 提交序 + 降级面 ─────────────────────────────────────────────────────────
check('提交序：arb.config.set 成功在前，setStrategy 启用在后（A 腿账户作用域）', () => {
  const commit = dialog.match(/async function commit\(\)[\s\S]*?\n\}/)?.[0] ?? ''
  const setAt = commit.indexOf('arbConfigSet')
  const onAt = commit.indexOf('setStrategy')
  assert.ok(setAt >= 0, 'commit 缺 arbConfigSet 调用')
  assert.ok(onAt > setAt, 'setStrategy 出现在 arb.config.set 之前 —— 合同提交序反了')
  // 2026-10 弹窗加策略选择段：启用名 = 选中的策略（selectedArbStrategy），
  // 不再钉死字面量 —— 硬编码字面量会让平台化时启用错策略。
  assert.match(
    commit,
    /setStrategy\(\s*selectedArbStrategy\.value\s*,\s*true,\s*legAAccount\.value\)/,
    '启用未落在 A 腿账户作用域，或仍硬编码策略名（应消费选中策略）',
  )
  assert.match(commit, /cfg\.error/, 'config 回执的 error 字段未检查（APPLICATION 错误会被吞）')
})

// ── 去标题化 + 策略选择段（2026-10）─────────────────────────────────────────
check('标题去 cross_venue_arb 化：弹窗标题是中性词（创建套利）', () => {
  const title = dialog.match(/<h3[^>]*>([^<]*)<\/h3>/)?.[1] ?? ''
  assert.ok(!title.includes('cross_venue_arb'), `标题仍含策略名：「${title}」—— cross_venue_arb 只是套利策略之一，不是套利本身`)
  assert.ok(/创建套利/.test(title), `标题缺中性词「创建套利」：「${title}」`)
})

check('弹窗有策略选择段（置于最前，A 腿段之前）', () => {
  assert.match(dialog, /label-micro">策略</, '缺「策略」段标题')
  const segAt = dialog.indexOf('label-micro">策略<')
  const legAt = dialog.indexOf('label-micro">A 腿<')
  assert.ok(segAt >= 0 && legAt >= 0 && segAt < legAt, '策略段不在 A 腿段之前 —— 策略选择是第一段')
  assert.match(dialog, /selectedArbStrategy/, '缺 selectedArbStrategy 选中状态')
})

check('策略候选面：静态单选项 + 平台化时换能力声明驱动的注释在场', () => {
  // 探测结论：strategy.list 行 wire 上无 arb/atomic_batch 能力字段
  // （modes wire 对象只带 market_type/structure），没有可靠的运行时判定
  // 面板能据它推导「谁是套利策略」—— 候选集只能静态。
  assert.match(dialog, /const ARB_STRATEGY_OPTIONS/, '缺策略候选常量 ARB_STRATEGY_OPTIONS')
  // 注释必须钉在常量附近（平台化时的替换点可寻）。
  const block = dialog.match(/\/\*[\s\S]*?\*\/\nconst ARB_STRATEGY_OPTIONS|\/\/[^\n]*\nconst ARB_STRATEGY_OPTIONS/)
  assert.ok(block, '候选常量附近缺注释 —— 平台化时换能力声明驱动的替换依据必须钉在数据源上')
  assert.match(block[0], /能力声明/, '候选常量的注释未点名「能力声明」驱动')
})

check('未支持的策略名不假提交：选择非 cross_venue_arb 时置灰/提示', () => {
  // 内核 arb.config.set IPC 只认 [cross_venue_arb] 节 —— 面板不提交一个
  // 内核必拒的名字。
  const m = dialog.match(/strategySupported|arbSupported|strategyNotSupported/)
  assert.ok(m, '缺「该策略暂未支持」的具名判定 —— 未支持的策略必须被拦，不假提交')
  assert.match(dialog, /该策略暂未支持|暂未支持/, '缺未支持提示文案')
  assert.match(
    dialog,
    /ARB_STRATEGY\.includes\(|ARB_STRATEGY ===|strategySupported\(/,
    '未支持判定不消费 ARB_STRATEGY 合同（判定与提交链脱钩）',
  )
})

check('老内核降级面：只读提示 + 禁用创建，不降级为单账户单选', () => {
  assert.match(dialog, /legacyCore/, '缺 legacyCore feature-detect 状态')
  assert.match(
    dialog,
    /当前内核版本为单账户模型，双腿双账户需升级内核/,
    '缺降级提示原文（操作员必须读到为什么创建面停用）',
  )
  assert.doesNotMatch(
    dialog,
    /单账户.*radio|radio.*单账户/,
    '降级面出现了单账户单选控件 —— 恢复旧模型不是降级选项',
  )
  assert.match(
    dialog,
    /method not found|rpc\s*\\?-32601/i,
    '缺 unknown-method 的 feature-detect（错误码/文本判定）',
  )
})

// ── dry 模式凭证豁免 ─────────────────────────────────────────────────────────
// 凭证门是 LIVE 语义（执行器拉单要凭据）；内核 arb_precheck 的凭证门本就
// 包在 `mode.may_trade()` 里（dry 模式不拦），前端无条件拦是面板超owner。
// dry 模式下创建不得被「腿凭证未就绪」阻断 —— 但披露保留（切 live 前还
// 是要看）。
check('dry 模式豁免：凭证门判据必须先看运行模式（may_trade 同形）', () => {
  assert.match(
    dialog,
    /isDry/,
    '弹窗缺 isDry 派生 —— 凭证门无从感知运行模式',
  )
  assert.match(
    dialog,
    /dryCredExempt/,
    '缺 dryCredExempt 判据 —— dry 豁免必须是有名字的独立谓词，不许内联进 credBlocked',
  )
  assert.match(
    dialog,
    /credBlocked[\s\S]*?dryCredExempt|dryCredExempt[\s\S]*?credBlocked/,
    'credBlocked 判据未消费 dryCredExempt',
  )
  assert.match(
    dialog,
    /Dry 模式|dry 模式/,
    '缺 dry 模式下的门语义披露（确认页必须说明门只对 live 生效）',
  )
})

// ── wire 面 ─────────────────────────────────────────────────────────────────
check('client.ts 提供 arbConfigGet / arbConfigSet 专用代理封装', () => {
  assert.match(client, /arbConfigGet:\s*\(\)\s*=>\s*request<ArbConfigDoc>\('\/arb-config'\)/, '缺 arbConfigGet')
  assert.match(
    client,
    /arbConfigSet:\s*\(body: ArbConfigSetBody\)\s*=>\s*[\s\S]*?request<ArbConfigDoc>\('\/arb-config\/set'/,
    '缺 arbConfigSet（POST /api/arb-config/set）',
  )
  assert.match(client, /version:\s*1\b/, 'set 载荷缺 version 恒 1 的类型面')
})

check('no-venue-branch：弹窗与 client 的新增面不分支于具体交易所名', () => {
  for (const [name, text] of [['CreateArbDialog.vue', dialog], ['client.ts', client]]) {
    const branch = text.match(/(==|!=|===|!==)\s*['"]?(polymarket|kalshi|predictfun|predict_fun)/i)
    assert.equal(branch, null, `${name} branches on a concrete venue`)
  }
})

process.exit(failures ? 1 : 0)
