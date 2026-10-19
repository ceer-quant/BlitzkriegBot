/**
 * 行情跟所 — the HftPage venue-following gate (clear + Loading on switch).
 *
 * The user's ruling (2026-10, 账户↔venue 解耦裁决), verbatim anchors:
 *
 *   「行情数据源应当跟随当前选中的'账户绑定的交易所'动态切换……切换到
 *     账户 B（绑定的 Kalshi）时，行情页面必须立刻解除 Polymarket 订阅……
 *     切换时 UI 必须显示 Loading 状态」——避免旧数据残留。
 *
 * Reverse acceptance — the following must be impossible on the UI:
 *   · switching accounts leaves the OLD venue's order book / price rows on
 *     screen where they can be misread as the new venue's quotes;
 *   · the page shows old venue data during the switch gap instead of a
 *     Loading state;
 *   · an unbound account (venue = '') renders stale market rows instead of
 *     the 「该账户未绑定交易所」 empty state.
 *
 * What this gate pins (source-level, following the repo's .check.mjs teeth
 * pattern):
 *   1. activeVenue is derived from the account binding (store.activeAccountVenue).
 *   2. A venue-switch watcher exists and stamps the switch (local timestamp +
 *      the venue it came from) instead of silently keeping old rows.
 *   3. A venueLoading gate exists whose open condition is "switched, new
 *      venue data not yet arrived", and it feature-detects venue tags — an
 *      old core without tags never gets stuck loading (degrade = show all).
 *   4. While loading, visiblePrices / visibleBooks are CLEARED (empty array
 *      before the filterByVenue chain) — no old-venue residue is renderable.
 *   5. The template renders the Loading card and the 未绑定 empty state, and
 *      every market display surface (prices grid, books card, kline select)
 *      reads the venue-filtered visible rows, not the raw ones.
 *   6. Venue stays data: no branch on a concrete exchange name, and the
 *      price/book surfaces render NO venue badges (the countdown card already
 *      names the exchange; the 2026-10 ruling removed the per-row labels).
 *
 *   cd ui/webapp/webui && npm run check:hft-venue
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

console.log('行情跟所 — the venue-following contract')

const page = read('src', 'pages', 'HftPage.vue')

// ── 1. venue follows the account binding ─────────────────────────────────────
check('activeVenue 派生自账户绑定（store.activeAccountVenue）——行情源跟账户走', () => {
  const m = page.match(/const activeVenue = computed\(\(\) => ([^\n]*)\)/)
  assert.ok(m, '缺 activeVenue computed')
  assert.match(
    m[1],
    /store\.activeAccountVenue/,
    'activeVenue 未接 store.activeAccountVenue —— 页面行情源没有跟随账户绑定',
  )
})

// ── 2. the switch watcher stamps the event ───────────────────────────────────
check('venue 切换被 watch 捕获并落时间戳（清旧 + 开 Loading 的事件起点）', () => {
  const body = page.match(/watch\(activeVenue,[\s\S]*?\n\}\)/)
  assert.ok(body, '缺 watch(activeVenue) —— 切换事件无人捕获，旧所行会原地残留')
  assert.match(body[0], /venueSwitchAt/, '切换未记录时间戳')
  assert.match(body[0], /venueSwitchFrom/, '切换未记录来源 venue（Loading 文案要如实说出清掉了谁）')
})

// ── 3. the Loading gate: open until the new venue's first tagged row arrives ─
check('venueLoading 门存在：切换后新所数据未到 → Loading；行不带 venue 标签（老核）门永不开', () => {
  const body = page.match(/const venueLoading = computed\([\s\S]*?\n\}\)/)
  assert.ok(body, '缺 venueLoading 门')
  assert.match(body[0], /venueSwitchAt/, '门未接切换时间戳 —— Loading 与切换事件脱钩')
  assert.match(body[0], /venueDataArrived|tagged/, '门未判定「新所数据已到达」')
  // Feature-detect discipline: a core that strips venue tags must degrade to
  // show-all, never freeze the page behind an eternal spinner.
  const arrived = page.match(/const venueDataArrived = computed\([\s\S]*?\n\}\)/)
  assert.ok(arrived, '缺 venueDataArrived 判定')
  assert.match(
    arrived[0],
    /typeof r\.venue === 'string'/,
    '到达判定未做 venue 字段 feature-detect（老核会被误卡在加载态）',
  )
})

// ── 4. loading clears the visible rows (no old-venue residue renderable) ────
check('Loading 期间 visiblePrices / visibleBooks 清空（旧所盘口零残留）', () => {
  for (const [name, type] of [['visiblePrices', 'MarketPrice\\[\\]'], ['visibleBooks', 'AssetBook\\[\\]']]) {
    const body = page.match(new RegExp(`const ${name} = computed<${type}>\\(\\(\\) => \\{[\\s\\S]*?\\n\\}\\)`))
    assert.ok(body, `缺 ${name} computed`)
    assert.match(
      body[0],
      /if \(venueLoading\.value\) return \[\]/,
      `${name} 未在 Loading 时清空 —— 旧所行会残留到新所首帧`,
    )
    assert.match(
      body[0],
      /filterByVenue/,
      `${name} 未走 filterByVenue —— venue 过滤链断了`,
    )
  }
})

// ── 5. the template renders the gate (Loading card + 未绑定 empty state) ────
check('模板渲染 Loading 卡（正在接入新所行情，指出已解除旧所订阅）', () => {
  assert.match(page, /v-else-if="venueLoading"|v-if="venueLoading"/, '缺 Loading 卡的 venueLoading 分支')
  assert.match(page, /正在接入/, 'Loading 卡缺「正在接入」文案')
  assert.match(page, /venueSwitchFrom/, 'Loading 文案未指出从哪个 venue 切出（解除订阅要如实说）')
})

check('未绑定账户渲染「该账户未绑定交易所」空态，不显示任何行情数据', () => {
  assert.match(page, /const venueUnbound = computed\(/, '缺 venueUnbound 判定')
  assert.match(page, /该账户未绑定交易所/, '缺未绑定空态文案')
})

check('行情面全部读 venue 过滤后的可见行（prices 网格 / 盘口卡 / K线选项均不读原始行）', () => {
  // The prices grid, the depth card and both asset selects must consume the
  // gate-cleared rows; a template still iterating raw `prices`/`books` would
  // render straight through the Loading gate.
  const template = page.slice(page.indexOf('<template>'))
  assert.match(template, /v-if="venueUnbound/, '未绑定空态没有挡在行情面之前')
  for (const relic of ['in prices"', 'in books"', 'v-if="prices.length"', 'v-if="books.length"']) {
    assert.doesNotMatch(
      template,
      new RegExp(relic.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')),
      `模板仍直接迭代原始行（${relic}）—— 绕过了 Loading 清空与 venue 过滤`,
    )
  }
  assert.match(template, /in visiblePrices/, '行情网格未消费 visiblePrices')
  assert.match(template, /in visibleBooks/, '盘口/K线选项未消费 visibleBooks')
})

check('行情面不渲染 venue 徽标（倒计时卡已写交易所，其他卡不再标注）', () => {
  // 2026-10 裁决：行情页不到处强调交易所。倒计时卡已显示交易所，行情网格
  // 与盘口卡的 per-row/per-card venue 徽标（含「未分所」降级徽标）已删除；
  // venue 仍是纯数据（行内字段照常流动），页面不渲染它。
  assert.doesNotMatch(page, /未分所/, '行情面仍渲染「未分所」徽标 —— 去交易所强调的裁决被回退')
  assert.doesNotMatch(
    page,
    /<Badge[^>]*>\{\{[^}]*(m\.venue|visibleBooks\[0\]\?\.venue)/,
    '行情网格/盘口卡仍渲染 venue 徽标 —— 去交易所强调的裁决被回退',
  )
  const banned = ['polymarket', 'kalshi', 'binance', 'okx']
  for (const name of banned) {
    assert.doesNotMatch(
      page,
      new RegExp(name, 'i'),
      `页面分支/硬编码了具体交易所名 ${name} —— venue 必须是数据不是逻辑`,
    )
  }
})

process.exit(failures ? 1 : 0)
