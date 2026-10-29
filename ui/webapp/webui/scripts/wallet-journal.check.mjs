/**
 * 钱包现金流水卡 — `account.journal` 的面板消费面，executable reverse-acceptance.
 *
 * #509 给 Ledger 加了 append-only 现金流水，#511 开了只读动词
 * `account.journal`；本批把流水呈现在行情页余额瓦片区下方。用户裁决「钱包应
 * 是真实记账」的最后一公里：操作者能看见每笔钱的来去 —— 每行时间/类型/金额/
 * 余额/ref，截断如实披露，老内核优雅退化。
 *
 * 钉住的四条（回归即红）：
 *
 *   1. client.ts 有 `accountJournal` 封装（`/account-journal` 网关代理路径）。
 *   2. HftPage 有九个内核 JournalKind 的中文标签映射（注资/买入成交/卖出成交/
 *      手续费/合并赎回/结算赔付/返现/对账回正/重置）。
 *   3. truncated 披露文案在场 —— 读者必须被告知「看到的是窗口，不是全史」。
 *   4. 「需要升级内核」容错在场 —— 老内核未知动词退化为一句提示，不崩卡。
 *
 *   cd ui/webapp/webui && npm run check:wallet
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

const client = read('src', 'api', 'accounts.ts')
// issue 717 面板瘦身：流水卡拆到 components/hft/JournalCard.vue（含九类
// 标签映射、truncated 披露、老内核降级、reset 分隔线、口径说明）—— 消费
// 断言跟着卡走，正则逐字不动。
const page = read('src', 'components', 'hft', 'JournalCard.vue')

console.log('钱包现金流水卡 — account.journal 的面板消费面')

// ① The read path: client carries the accountJournal wrapper over the gateway
//    proxy route, typed (rows + truncated).
check('client.ts has the accountJournal wrapper and wire types', () => {
  assert.match(client, /accountJournal:\s*\(params\?\:\s*\{[^}]*accountId\?[^}]*limit\?[^}]*\}\)/,
    'accountJournal(params {accountId?, limit?}) wrapper missing')
  assert.match(client, /request<AccountJournalDoc>\('\/account-journal'/,
    'accountJournal must hit the /account-journal gateway route')
  assert.match(client, /export interface AccountJournalRowDoc/, 'row wire type missing')
  const doc = client.match(/export interface AccountJournalDoc \{[\s\S]*?\n\}/)?.[0] ?? ''
  assert.match(doc, /rows:/, 'AccountJournalDoc.rows missing')
  assert.match(doc, /truncated:\s*boolean/, 'AccountJournalDoc.truncated missing')
})

// ② The nine kinds, in Chinese, verbatim — the kernel's JournalKind vocabulary
//    rendered as labels, one map on the page (single consumer, no lib file).
check('HftPage maps all nine journal kinds to Chinese labels', () => {
  const map = page.match(/const JOURNAL_KIND_LABELS[^=]*=\s*\{[\s\S]*?\}/)?.[0] ?? ''
  assert.ok(map, 'JOURNAL_KIND_LABELS map missing from HftPage')
  for (const kind of ['seed', 'buy_fill', 'sell_fill', 'fee', 'merge', 'redemption', 'cashflow', 'realign', 'reset']) {
    assert.ok(map.includes(`${kind}:`), `kind ${kind} missing from the label map`)
  }
  for (const label of ['注资', '买入成交', '卖出成交', '手续费', '合并赎回', '结算赔付', '返现', '对账回正', '重置']) {
    assert.ok(map.includes(`'${label}'`), `Chinese label ${label} missing`)
  }
})

// ③ The truncation disclosure: the reader sees a WINDOW (JOURNAL_CAP = 10000),
//    not the whole trail — the copy must say so when truncated is true.
check('truncated disclosure copy is rendered', () => {
  assert.match(page, /journalDoc\?\.truncated/, 'truncated flag never read')
  assert.match(page, /仅显示最近 100 笔（内核窗口 10000）/, 'disclosure sentence missing')
})

// ④ The old-kernel degrade: an unknown verb becomes one upgrade sentence, not
//    a crashed card — same posture as AccountSwitcher's fault-tolerant regex.
check('unknown-verb degrade to 「需要升级内核」 is present', () => {
  assert.match(page, /journalUnsupported/, 'unsupported detection missing')
  assert.match(page, /unknown\|not found\|method\|未知的\|不支持/, 'AccountSwitcher-style regex missing')
  assert.match(page, /当前内核不支持流水查看，需要升级内核/, 'upgrade sentence missing')
})

// ⑤ reset 分隔线（流水卡可读性 2026-10-20）：reset 是「此前一切清账」的硬
//    事实，账目在 reset 前后互不可加和 —— 分隔行显性切刀，免得读者硬算一
//    个不存在的总额（生产 kalshi/default 流水卡「对不上」的可读性半边）。
check('reset rows render as a divider before the row (not a silent mid-table reset)', () => {
  assert.match(page, /r\.kind === 'reset' && i > 0/, 'reset divider condition missing')
  assert.match(page, /交易环境已重置/, 'divider copy missing')
})

// ⑥ 口径说明（2026-10-20）：一笔成交 = 买入+卖出两行现金事件，与交易记录的
//    一笔一行口径不同 —— 卡头必须把这一点说出来，行数对不上是本来的形状。
check('caliber note「现金事件口径」with tooltip is on the header', () => {
  assert.match(page, /现金事件口径/, 'header caliber note missing')
  assert.match(page, /一笔成交拆成「买入成交」\+「卖出成交」两行/, 'tooltip explanation missing')
})

console.log(`\nRESULT: ${failures > 0 ? `FAIL — ${failures} check(s) red` : 'PASS — journal card wired: wrapper + nine labels + truncation disclosure + old-kernel degrade + reset divider + caliber note'}`)
if (failures > 0) process.exit(1)
