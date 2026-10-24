/**
 * 轮次周期集 — the per-account rounds whitelist gate（轮次多族 PR-D；issue 550）.
 *
 * The contract (B1d 内核面已合并，B2 的 `account.rounds.set` 并行落地):
 * `accounts.toml` 的 `[[account]]` 带 `rounds: Vec<i64>`（许可轴：空/缺省 =
 * 不限制；白名单 [300, 900, 3600, 14400, 86400]，与全局轮次档位同一张表），
 * `account.list` 的 AccountView 带回 `rounds`。面板侧合同：
 *
 *   1. client 有 accountRoundsSet 专用封装（/account-rounds/set，body 即
 *      params 逐字转发 —— 同 account-trading/set 的代理纪律）。
 *   2. 账户卡（AccountSwitcher）有白名单五档 checkbox 多选，五档秒数全部
 *      在场 —— 少一档就是替内核裁剪了许可轴。
 *   3. 老内核降级：AccountView.rounds 缺字段（undefined）= 整段只读提示
 *      「当前内核不支持周期集」，不出编辑面 —— feature-detect，不炸。
 *   4. 错误透出不吞：内核拒绝（移除有敞口的族，消息点名族列表）原文落
 *      inline 错误位；保存路径没有静默 catch。
 *   5. HftPage 挂单过期无裸 900 回落 —— 必须 durationSec（身份所钟行）
 *      → round_duration.get 生效值 → 900 的回落链。
 *   6. 注释 issue 号写「issue 550」空格形式 —— `#数字` 会撞 RAW_COLOR 门
 *      禁的 hex 色正则。
 *
 *   cd ui/webapp/webui && npm run check:rounds
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

console.log('轮次周期集 — the per-account rounds whitelist contract (issue 550)')

const client = read('src', 'api', 'client.ts')
const store = read('src', 'stores', 'panel.ts')
const switcher = read('src', 'components', 'AccountSwitcher.vue')
const hft = read('src', 'pages', 'HftPage.vue')
const gateway = read('../../..', 'ui', 'ui_kit', 'src', 'web', 'mod.rs')

// ── 1. the client封装 ─────────────────────────────────────────────────────────
check('client 有 accountRoundsSet 专用封装（/account-rounds/set，body 逐字转发）', () => {
  assert.match(
    client,
    /accountRoundsSet:\s*\(body:\s*\{ accountId: string; rounds: number\[\]; reason\?: string \}\)/,
    'client.ts 缺 accountRoundsSet 方法（签名须带 accountId + rounds + reason?）',
  )
  assert.match(client, /'\/account-rounds\/set'/, 'accountRoundsSet 未走 /account-rounds/set 代理路由')
  const m = client.match(/accountRoundsSet:\s*\(body[^`]*?\/account-rounds\/set',\s*\{[\s\S]*?\n\s*\}\)/)
  assert.ok(m, 'accountRoundsSet 方法体残缺')
  assert.match(m[0], /JSON\.stringify\(body\)/, '方法未把 body 逐字 JSON 转发 —— 网关侧零加工的前提')
  assert.match(client, /interface AccountRoundsSetResult\b/, '缺 AccountRoundsSetResult 回执类型')
})

check('网关注册 /api/account-rounds/set 代理路由（account.rounds.set 逐字转发）', () => {
  assert.match(
    gateway,
    /\("POST", "\/api\/account-rounds\/set"\)/,
    'ui_kit 网关缺 /api/account-rounds/set 路由 —— 面板写动词无处落',
  )
  const m = gateway.match(/\("POST", "\/api\/account-rounds\/set"\) => \{[\s\S]*?\n\s*\}/)
  assert.ok(m, 'account-rounds/set 路由体残缺')
  assert.match(m[0], /proxy_backtest_body\("account\.rounds\.set"/, '路由未转发 account.rounds.set 动词')
})

// ── 2. the store action ───────────────────────────────────────────────────────
check('store 有 setAccountRounds action（写内核成功后重拉账户簿）', () => {
  const fn = store.match(/async function setAccountRounds\([\s\S]*?\n  \}/)
  assert.ok(fn, 'stores/panel.ts 缺 setAccountRounds')
  assert.match(fn[0], /api\.accountRoundsSet\(/, 'action 必须走 api.accountRoundsSet（专用代理）')
  assert.match(fn[0], /refreshAccounts\(\)/, '成功路径必须 refreshAccounts 重拉 AccountView.rounds')
  assert.match(fn[0], /throw new Error\(out\.error\)/, '内核拒绝必须抛出 —— 静默返回就是吞掉敞口拒绝')
  assert.match(store, /setAccountRounds,/, 'store 导出缺 setAccountRounds')
})

// ── 3. the card face ──────────────────────────────────────────────────────────
check('账户卡有白名单五档 checkbox（五档秒数全部在场）', () => {
  const opts = switcher.match(/const ROUND_OPTIONS[\s\S]*?\n\]/)
  assert.ok(opts, 'AccountSwitcher 缺 ROUND_OPTIONS 白名单表')
  for (const sec of [300, 900, 3600, 14400, 86400]) {
    assert.match(opts[0], new RegExp(`sec: ${sec},`), `白名单缺 ${sec}s 档`)
  }
  assert.match(switcher, /type="checkbox"/, '编辑面必须是多选 checkbox（许可轴，不是单选档位）')
})

check('rounds undefined = 老内核 → 只读降级提示在场（不出编辑面）', () => {
  assert.match(
    switcher,
    /roundsUnsupported = \(a: AccountRow\): boolean => a\.rounds === undefined/,
    '缺 feature-detect（字段缺省 = 老内核，空数组是「不限制」，两者不能混）',
  )
  assert.match(switcher, /当前内核不支持周期集/, '降级只读提示文案缺失')
  // The editor never opens for an unsupported row.
  assert.match(switcher, /if \(roundsUnsupported\(a\)\) return/, '老内核行必须挡在编辑面之外')
})

check('空选集 = 「不限制（未声明）」如实显示（不造至少选一个的前端门）', () => {
  assert.match(switcher, /不限制（未声明）/, '空集如实显示文案缺失')
  assert.match(switcher, /roundsDraftEmpty/, '空选集提示缺失')
})

check('保存走 store.setAccountRounds，错误原文透出到卡内（refusal attribution）', () => {
  const save = switcher.match(/async function saveRounds\([\s\S]*?\n\}/)
  assert.ok(save, 'AccountSwitcher 缺 saveRounds')
  assert.match(save[0], /store\.setAccountRounds\(/, '保存必须走 store action')
  assert.match(save[0], /roundsErr\.value = e instanceof Error \? e\.message : String\(e\)/, '失败必须落 inline 错误位 —— 吞掉就是操作员看不见内核拒绝')
  assert.match(switcher, /\{\{ roundsErr \}\}/, '模板缺错误渲染位')
  // 内核是拒绝方：确认弹窗反而是 UI 预判敞口（裁决：不造）。
  const saveBlock = save[0]
  assert.doesNotMatch(saveBlock, /confirm|ConfirmWordDialog/, '保存不得造确认门 —— 内核是拒绝方，UI 不预判敞口')
})

// ── 4. HftPage：无裸 900 回落 ─────────────────────────────────────────────────
check('HftPage 挂单过期轮长走 durationSec → round_duration.get → 900 回落链', () => {
  assert.match(hft, /identityDurSec/, '缺 identityDurSec 回落链 computed')
  const chain = hft.match(/const identityDurSec = computed<number \| null>\(\(\) => \{[\s\S]*?\n\}\)/)
  assert.ok(chain, 'identityDurSec 形状残缺')
  assert.match(chain[0], /venueRound\.value\?\.durationSec/, '回落链第一级必须是身份所钟行的 durationSec（轮次多族）')
  assert.match(chain[0], /roundDurHintSec\.value/, '回落链第二级必须是 round_duration.get 生效值')
  assert.match(hft, /const dur = identityDurSec\.value \?\? 900/, '终级回落 900（现行为）必须显式成链')
  // 裸常量回落被删：任何 `* 900` / `= 900` 的裸数学都不许回来。
  assert.doesNotMatch(hft, /\* 900\b/, '裸 `* 900` 数学回归 —— 轮长必须走回落链')
  assert.doesNotMatch(hft, /const roundDurSec = 900/, 'const roundDurSec = 900 裸常量回归')
})

// ── 4b. 周期切换入口在行情页倒计时卡（2026-10-24 操作者指令）──────────────────
// 入口从设置 · 通用 挪到行情页（盯盘时切周期不离开监视语境）；两卡共享
// lib/round-duration.ts 一份读数与写路径，语义零改动。牙钉四点：
//   · composable 是唯一写路径（两页面都消费它，不许各自再摸 roundDurationSet）；
//   · 行情页倒计时卡必须挂 SegmentedControl（入口在场）；
//   · next_session 语义钉死：写入后走 useRestartCore 重启链（不许热切幻觉）；
//   · flagLocked（cli_or_env 显式参数在管）必须禁用切换控件。
check('轮次周期切换入口在行情页倒计时卡（共享 composable 单一写路径）', () => {
  const lib = read('src', 'lib', 'round-duration.ts')
  assert.match(lib, /roundDurationSet/, 'composable 缺 roundDurationSet 写路径')
  assert.match(lib, /useRestartCore/, 'composable 缺重启链 —— next_session 写入必须 10s 自动重启')
  assert.match(lib, /onMounted\(load\)/, 'composable 缺装载 —— 卡面挂着就要读数')
  // 行情页：入口在场 + 消费 composable + 语义钉死。
  assert.match(hft, /useRoundDuration\(\)/, 'HftPage 缺 useRoundDuration —— 周期切换入口没进倒计时卡')
  assert.match(hft, /v-model="roundSwitch\.value\.value"[\s\S]*?:segments="roundSwitch\.segments\.value"/, '倒计时卡缺 SegmentedControl 切换控件')
  assert.match(hft, /roundSwitch\.flagLocked\.value/, '倒计时卡未消费 flagLocked —— 显式启动参数在管时必须禁用切换')
  assert.match(hft, /roundSwitch\.restart\.counting\.value/, '倒计时卡缺重启倒计时条 —— next_session 写入后无生效路径可见')
  // 设置页保留同源卡面（第二入口），且不许再私藏一份独立写路径。
  const settings = read('src', 'pages', 'settings', 'SettingsGeneral.vue')
  assert.match(settings, /useRoundDuration\(\)/, '设置页轮次卡未接 composable —— 两卡必须共享一份逻辑')
  for (const [file, src] of [['HftPage.vue', hft], ['SettingsGeneral.vue', settings]]) {
    assert.doesNotMatch(src, /api\.roundDurationSet/, `${file} 直接摸 roundDurationSet —— 写路径必须走 lib/round-duration.ts 单源`)
  }
})

// ── 5. comment hygiene（RAW_COLOR / venue-name bans）──────────────────────────
check('注释 issue 号用「issue 550」格式（禁 #550 —— RAW_COLOR 门禁误判 hex 色）', () => {
  for (const [file, src] of [['client.ts', client], ['panel.ts', store], ['AccountSwitcher.vue', switcher], ['HftPage.vue', hft]]) {
    const comments = src.match(/\/\*[\s\S]*?\*\/|\/\/[^\n]*/g) ?? []
    for (const c of comments) {
      assert.doesNotMatch(c, /#550\b/, `${file} 注释出现 #550 形式 —— 会撞 RAW_COLOR 门禁，写「issue 550」`)
    }
  }
})

process.exit(failures ? 1 : 0)
