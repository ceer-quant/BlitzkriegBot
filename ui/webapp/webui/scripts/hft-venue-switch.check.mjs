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
// 首页（Overview）的轮次瓦片与行情页倒计时卡共用一条跟随链（issue 559 起
// identityVenue/displayVenue），混钟尾审计（2026-10-20）后这里一并钉住它：
// 同一症状（slot 随身份所、剩余/窗口随全局保守钟）不许在第二张页面复活。
const overviewPage = read('src', 'pages', 'Overview.vue')

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

// ── 1b. the identity single source (issue 559) ───────────────────────────────
// 「行情显示谁的市场」的 UNIQUE source：内核快照顶层的 displayVenue（显示
// 账户的 venue，accounts.toml 顶层 display_account 持久化 + 运行时生效），
// 老内核缺字段回落账户绑定链。回落按「字段缺省」判定 —— 内核在场即全听
// 它的（含空串 = 未绑定 = 回落全局读数）。
check('身份唯一源 identityVenue：displayVenue 优先，快照缺字段回落账户绑定链（issue 559）', () => {
  const client = read('src', 'api', 'client.ts')
  // The wire contract: the snapshot type declares BOTH display fields.
  assert.match(client, /displayAccount\?: string \| null/, 'Snapshot 合同缺 displayAccount —— 显示账户没进合同')
  assert.match(client, /displayVenue\?: string \| null/, 'Snapshot 合同缺 displayVenue —— 显示 venue 没进合同')
  // The page derives ONE identity venue with the documented fallback chain.
  const m = page.match(/const identityVenue = computed<string>\(\(\) => \{[\s\S]*?\n\}\)/)
  assert.ok(m, '缺 identityVenue computed —— 身份行没有唯一源')
  assert.match(m[0], /displayVenue/, 'identityVenue 没读快照 displayVenue —— 回落链断了')
  assert.match(
    m[0],
    /store\.activeAccountVenue/,
    'identityVenue 缺账户绑定回落 —— 老内核的身份行会悬空',
  )
  assert.match(
    m[0],
    /displayAccount !== undefined \|\| s\?\.displayVenue !== undefined/,
    '回落判定不是「字段缺省」—— 内核显式空串会被误判成缺字段',
  )
  // The countdown card's identity line reads ONLY the identity venue.
  assert.match(page, /const marketName = computed\(\(\) => \{[\s\S]*?identityVenue\.value/, '倒计时卡徽章名未接 identityVenue')
  assert.match(page, /const marketTypeText = computed\(\(\) => \{[\s\S]*?identityVenue\.value/, '倒计时卡类型徽章未接 identityVenue')
  assert.doesNotMatch(
    page,
    /const marketName = computed\(\(\) => \{[\s\S]{0,600}activeVenue\.value/,
    '倒计时卡身份仍读账户绑定的 activeVenue —— 唯一源被旁路（issue 559 的错位会回来）',
  )
  // The countdown's clock selection follows the same identity venue (the
  // identity line and the clock must never disagree about whose round it is).
  assert.match(page, /watch\(identityVenue/, '倒计时重绑没有挂 identityVenue —— 身份变了钟不清')
})

// ── 2. the switch watcher stamps the event ───────────────────────────────────
check('venue 切换被 watch 捕获并落时间戳（清旧 + 开 Loading 的事件起点）', () => {
  const body = page.match(/watch\(identityVenue,[\s\S]*?\n\}\)/)
  assert.ok(body, '缺 watch(identityVenue) —— 切换事件无人捕获，旧所行会原地残留')
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

// ── 7. #550（症状②）the countdown re-binds to the identity venue's own clock ──
check('倒计时优先读身份 venue 自己的钟行（round.venueRounds，#550 症状②）', () => {
  // The wire contract: the snapshot type declares the per-venue rows.
  const client = read('src', 'api', 'client.ts')
  assert.match(client, /interface VenueRound\b/, 'client.ts 缺 VenueRound 行类型 —— venueRounds 没有进合同')
  assert.match(
    client,
    /venueRounds\?: VenueRound\[\]/,
    'Round 合同缺 venueRounds 字段 —— 老内核降级路径无从判定',
  )
  // The page binds the countdown to the identity venue's row, keyed by venue.
  const vr = page.match(/const venueRound = computed\(\(\) => \{[\s\S]*?\n\}\)/)
  assert.ok(vr, '缺 venueRound computed —— 倒计时没有 per-venue 选行逻辑')
  assert.match(vr[0], /venueRounds/, '选行逻辑没读 venueRounds')
  assert.match(vr[0], /identityVenue\.value/, '选行逻辑没按身份 venue 过滤')
  // The snap watcher must consume the venue row BEFORE falling back to the
  // global scalar block, and the fallback must be gated on the venue key.
  const snapWatch = page.match(/watch\(snap, \(s\) => \{[\s\S]*?\n\}, \{ immediate: true \}\)/)
  assert.ok(snapWatch, '缺 watch(snap) 倒计时喂入')
  assert.match(
    snapWatch[0],
    /countForVenue\.value !== identityVenue\.value/,
    '倒计时没有 venue 键卫兵 —— 不同 venue 的钟行会刷新同一个倒计时（残值泄漏）',
  )
  assert.match(
    snapWatch[0],
    /if \(!countForVenue\.value && s\?\.round/,
    '全局标量块回退没有按 venue 键门控 —— 切换后旧钟仍会续跳',
  )
})

check('venue 切换时倒计时残值清零（serverLeft/serverAt/leftSec 归零再等新钟首帧）', () => {
  // The switch watcher must clear the residual local-interpolation state —
  // otherwise the old venue's countdown is kept alive by the local 500ms
  // ticker across the switch gap (the exact #550 symptom ② reproduction).
  const body = page.match(/watch\(identityVenue,[\s\S]*?\n\}\)/)
  assert.ok(body, '缺 watch(identityVenue)')
  assert.match(body[0], /serverLeft\.value = 0/, '切换未清 serverLeft —— 旧倒计时残值跨 venue 存活')
  assert.match(body[0], /serverAt\.value = 0/, '切换未清 serverAt —— 本地插值基点跨 venue 存活')
  assert.match(body[0], /leftSec\.value = 0/, '切换未清 leftSec —— 渲染值跨 venue 存活')
  // issue 561（裁决 4）：清键必须【无条件】——曾以 `hasRows`（本帧
  // venueRounds 非空）作前提，切到快照尚无钟行的 venue 时整段跳过，
  // countForVenue 残键为真值把 fallback 门锁死，倒计时愣在 00:00。
  // 对老内核清键是幂等空操作（它的 countForVenue 从未被赋过键），
  // 无条件化不改变老内核行为，故 feature-detect 前提被本裁决移除。
  assert.doesNotMatch(
    body[0],
    /hasRows/,
    '清残值仍以 hasRows（venueRounds 非空）为前提 —— 无钟行 venue 的 00:00 卡死会回来（issue 561 裁决 4）',
  )
  assert.match(
    body[0],
    /countForVenue\.value = nv/,
    '切换未把倒计时键钉到新 venue —— fallback 分支会被残键门死',
  )
})

// ── 7b. 首页（Overview）轮次瓦片同一跟随链 —— 混钟尾（审计 2026-10-20）────────
// 症状与 #550 症状②同族但长在第二张页面：瓦片副行的「剩余」与「可交易」
// 徽章此前永远读全局标量块 —— 多族（多插件）下全局块是各 venue 最早到期
// 的保守钟，slot 随身份所、剩余随别家，一个瓦片两个钟。反向验收：
//   · Overview 的剩余秒/窗口徽章必须消费身份所自己的 venueRounds 行；
//   · 行缺席（老内核无 venueRounds / 该所钟未开口）必须 feature-detect
//     回落全局块 —— 单族/老内核逐字节不变；
//   · 旁路即 FAIL：两处消费不得绕过 venueRound 直读全局块。
check('Overview 轮次瓦片剩余秒跟随身份所钟行（混钟尾）', () => {
  const vt = overviewPage.match(/const roundTimeLeft = computed\(\(\) => \{[\s\S]*?\n\}\)/)
  assert.ok(vt, 'Overview 缺 roundTimeLeft —— 副行「剩余」仍直读全局块 timeLeftSec')
  assert.match(vt[0], /venueRound\.value/, 'roundTimeLeft 未消费身份所钟行 venueRound')
  assert.match(vt[0], /timeLeftSec/, 'roundTimeLeft 没读钟行的 timeLeftSec')
  assert.match(vt[0], /round\.value\?\.timeLeftSec/, 'roundTimeLeft 缺全局块回落 —— 老内核/无钟行时读数悬空')
  // 模板侧旁路即 FAIL：副行必须吃 roundTimeLeft，不再有 `round?.timeLeftSec` 直插。
  assert.doesNotMatch(overviewPage, /\{\{\s*duration\(round\?\.timeLeftSec/, '模板仍直插全局块 timeLeftSec —— 修了 computed 没接线等于没修')
  assert.match(overviewPage, /duration\(roundTimeLeft\)/, '模板副行未接 roundTimeLeft')
})

check('Overview 可交易徽章按身份所窗口判定（全局 or 值不标到单所头上）', () => {
  const ct = overviewPage.match(/const roundCanTrade = computed\(\(\) => \{[\s\S]*?\n\}\)/)
  assert.ok(ct, 'Overview 缺 roundCanTrade —— 徽章仍吃全局 or 值 canTrade')
  assert.match(ct[0], /venueRound\.value/, 'roundCanTrade 未消费身份所钟行 venueRound')
  assert.match(ct[0], /round\.value\?\.canTrade/, 'roundCanTrade 缺全局块回落 —— 老内核行为改变')
  // 模板侧旁路即 FAIL：徽章必须吃 roundCanTrade。
  assert.doesNotMatch(overviewPage, /:variant="round\?\.canTrade/, '模板徽章仍直读全局块 canTrade —— 多族下把别族的窗口标到身份所头上')
  assert.match(overviewPage, /:variant="roundCanTrade/, '模板徽章未接 roundCanTrade')
})

// ── 8. 轮次告警三修（2026-10-23）：Loading 必有超时释放 + discovery 空判信号 ──
// 反向验收 — 以下必须在 UI 上不可能：
//   · venue 切到「快照永远不来行」的所时，页面永远转圈 —— Loading 门
//     必须有超时释放，把判定交给空判信号而不是无限 Loading；
//   · 该所确实没有当前轮次档位的市场（discovery 空集）时，用户看到的
//     还是「网络拦截」文案 —— 空判信号必须在快照层面（钟行+行情行双
//     缺席）成立，且带稳定窗口（防轮界瞬间的合法空拍误报）。
check('venueLoading 有超时释放（VENUE_LOADING_TIMEOUT_MS）—— 旧核降级之外的第二条防永转圈线', () => {
  const body = page.match(/const venueLoading = computed\([\s\S]*?\n\}\)/)
  assert.ok(body, '缺 venueLoading 门')
  assert.match(body[0], /VENUE_LOADING_TIMEOUT_MS/, 'Loading 门没有超时常量 —— 新所数据永不到达时页面无限转圈')
  assert.match(body[0], /nowMs\.value\s*-\s*venueSwitchAt\.value/, '超时未以切换时间戳为起点的墙钟差计算')
  const timeout = page.match(/const VENUE_LOADING_TIMEOUT_MS = ([\d_]+)/)
  assert.ok(timeout, '缺 VENUE_LOADING_TIMEOUT_MS 常量定义')
  assert.ok(Number(timeout[1].replaceAll('_', '')) >= 10_000, `超时窗口 ${timeout[1]} 过短 —— 轮界正常等待会被误释放`)
  assert.ok(Number(timeout[1].replaceAll('_', '')) <= 60_000, `超时窗口 ${timeout[1]} 过长 —— 释放前用户等待不可接受`)
})

check('discovery 空判信号（venueDiscoveryEmpty）：钟行+行情行双缺席 + 稳定窗口防轮界误报', () => {
  const body = page.match(/const venueDiscoveryEmpty = computed\([\s\S]*?\n\}\)/)
  assert.ok(body, '缺 venueDiscoveryEmpty 空判信号')
  // 双条件：钟行与行情行都要缺席才成立（任一在场 = 该所活着，只是没数据）。
  assert.match(body[0], /venueRounds/, '空判没看钟行（venueRounds）—— 只看行情行会在轮界空拍误报')
  assert.match(body[0], /prices/, '空判没看行情行（prices）—— 只看钟行会在行情插件活着但暂无报价时误报')
  // 稳定窗口：空拍持续满 VENUE_DISCOVERY_EMPTY_MS 才真（轮界瞬间合法为空）。
  assert.match(body[0], /VENUE_DISCOVERY_EMPTY_MS/, '空判没有稳定窗口 —— 轮界瞬间双缺席会闪出「可能不支持」')
  const win = page.match(/const VENUE_DISCOVERY_EMPTY_MS = ([\d_]+)/)
  assert.ok(win, '缺 VENUE_DISCOVERY_EMPTY_MS 常量定义')
  assert.ok(Number(win[1].replaceAll('_', '')) >= 5_000, `空判窗口 ${win[1]} 过短 —— 轮界空拍会闪报`)
  // 复位纪律：任一缺席条件被推翻（有钟行/有行情行/有可见行/Loading/未绑定）必须清零。
  assert.match(body[0], /venueDiscoveryEmptySince\.value = 0/, '空判窗口没有复位路径 —— 恢复后旧窗口残留会立即误报')
})

check('空判信号接入轮次告警判定（venueRoundUnsupported 的或支）—— 快照信号与探针判据并存', () => {
  const body = page.match(/const venueRoundUnsupported = computed\([\s\S]*?\n\}\)/)
  assert.ok(body, '缺 venueRoundUnsupported 判定')
  assert.match(body[0], /venueDiscoveryEmpty\.value/, '空判信号没接进轮次告警 —— discovery 空集时用户仍看网络文案')
  assert.match(body[0], /rep\.ok === true/, '探针判据（netCheck report）丢失 —— 降级面只剩信号一条腿')
})

process.exit(failures ? 1 : 0)
