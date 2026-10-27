#!/usr/bin/env node
/**
 * venue-identity-probe — RFC #661 真线只读探针（READ-ONLY）。
 *
 * RFC-661 §五.3 的真线验收三点（M3 落地后的真线证明，不启动 Live、不改任何
 * 运行配置——本脚本只说，不动手）：
 *
 *   ① engine.books 行数按 venue 计数：寄生 venue 与被寄生 venue 各自非零、
 *      无合并（旧形态 N + 0）；同字节 token 出现在两个 venue 名下 = 双键生效。
 *   ② trade.venue = 执行身份：polysimulator 账户的成交行 venue 应为
 *      polysimulator，面板 venue 滤网行数 ≡ 徽标计数（截图 bug「汇总 2 单
 *      0 行」不可复现）。迁移前的旧违建行被明列为 legacy-tagged —— 那正是
 *      违建活体证据；迁移后同脚本应转绿。
 *   ③ 上游组件集合 ≡ 原生集合：声明 feedSource 的消费端插件不得新增
 *      feed/discovery 组件（组件集合 ⊆ 原生未声明者集合）——M2 复用生效，
 *      消费端只吃源家的流，总上游轮询流不因消费端启用而增长；kalshi 等
 *      第二原生 venue 自持 feed 是合法的第二条流，判据不是「恒 ≡1」。
 *      旧核 market.list 缺 feedSource 字段时本断言退化为 note（向后兼容，
 *      不误报）。
 *
 * 只读铁律：本脚本只调只读 IPC 动词 —— core.ready / engine.books /
 * engine.round / engine.stats / trades.history / trades.summary /
 * account.list / market.list / market.plugin.get。绝不触 place / cancel /
 * set / switch / exit 类写动词（scripts 家规：禁裸跑 ops 动词）。它也从不
 * 读、不持有、不发送任何凭据；token id 一律截断输出（绝不打印全值）。
 *
 * 迁移前后都可跑（phase 自标注，不 hardcode 任何行数断言——生产行数随行情
 * 波动，只断结构性质）：
 *   - books：`dualKey`（同 token 两个 venue 名下各有行）+ `venueRounds`
 *     判 phase。M1 落地后镜像所行进 scanner 表、有自己的轮钟 → post（自然
 *     双键）；M1 之前镜像所即使 books 视图里有行（显示簿 mirror 合成臂，
 *     违建 #4）也没有自己的轮钟 → pre，标注 synthesized。
 *   - trades：行 venue ≡ 账户绑定 venue 的一致率；不一致行 = legacy-tagged
 *     （迁移前旧违建数据）。每账户「summary.totalTrades vs venue 滤网命中
 *     行数」对照 —— 相等即截图 bug 不可复现。
 *   - upstream：启用插件中带数据 feed / discovery 的组件集合 ⊆ 原生
 *     （未声明 feedSource）集合——消费端不新增组件；feed 计数窗
 *     （Δbooks/Δtops）与 tickHealth 作为活性读数（非断言）。
 *
 * 入口：UDS 直连（lib/core-client.mjs 的 requestOnce，与 feed-live-probe /
 * account-drift-check 同一客户端）。socket 不在时报清晰错误退出（码 2），
 * 绝不碰网关 HTTP 登录 —— 探针在宿主上跑。`--socket` 可指一个私有核（自测
 * /fixture 惯例，同 account-drift-check）。
 *
 * 退出码：0 = 探针完成（发现以标注呈现，phase 各安其位）；1 = 结构性断言
 * 失败（任何 phase 都不该出现的形态：同 venue 内 (venue,token) 重复行 /
 * 合并形态 N+0 / 消费端新增上游组件）；2 = 连不上核或用法错误。
 *
 * Usage:
 *   node scripts/venue-identity-probe.mjs                    # 三模式连跑
 *   node scripts/venue-identity-probe.mjs books              # 单模式
 *   node scripts/venue-identity-probe.mjs trades upstream
 *   node scripts/venue-identity-probe.mjs --socket /path/core.sock books
 *   node scripts/venue-identity-probe.mjs upstream --sample-sec 5
 */
import { resolveSocketPath } from './lib/core-socket.mjs';
import { requestOnce } from './lib/core-client.mjs';

const argv = process.argv.slice(2);
const opt = (n, d) => { const i = argv.indexOf(n); return i >= 0 && argv[i + 1] ? argv[i + 1] : d; };
const MODES = ['books', 'trades', 'upstream'].filter((m) => argv.includes(m));
if (MODES.length === 0) MODES.push('books', 'trades', 'upstream');
const SOCK_OVERRIDE = opt('--socket', null);
const SOCK = SOCK_OVERRIDE ?? resolveSocketPath(process.env);
/** Liveness window for the upstream feed-counter delta (seconds). */
const SAMPLE_SEC = Math.max(1, parseFloat(opt('--sample-sec', '3')));
/** trades.history limit; 0 = drain ALL closed rows (the gateway's own read). */
const TRADE_LIMIT = Math.max(0, parseInt(opt('--trade-limit', '0'), 10));
const TIMEOUT_MS = 8000;

// Strict: a read that fails is a finding (the probe must never mistake a
// failed read for a clean comparison), so no catch-to-null here.
const rpc = (method, params = {}) => requestOnce(SOCK, method, params, { timeoutMs: TIMEOUT_MS });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// 生产数据脱敏：token id 只出前 10 字节 + 长度。全值（或任何凭据形状的值）
// 绝不进入输出。
const short = (t) => (typeof t === 'string' && t.length > 12)
  ? `${t.slice(0, 10)}…(${t.length})`
  : (t ?? '');
const jline = (obj) => console.log(JSON.stringify(obj));

function banner(mode) {
  console.log(`\n== venue-identity-probe :: ${mode} (READ-ONLY) ==`);
}

/** ① books — venue 分组行计数 + 双键/合成形态判定。 */
async function probeBooks() {
  banner('books');
  const [rows, round, accounts, plugins] = await Promise.all([
    rpc('engine.books'),
    rpc('engine.round'),
    rpc('account.list'),
    rpc('market.plugin.get'),
  ]);
  const bookRows = Array.isArray(rows) ? rows : [];
  const bindings = (accounts?.accounts ?? [])
    .filter((a) => a.venue && a.venue.length > 0)
    .map((a) => a.venue);
  const enabledVenues = plugins?.derivedVenues ?? bindings;

  // 按 venue 分组；token 字节 → venue 集合（双键判定面）。
  const byVenue = new Map();
  const tokenVenues = new Map();
  const dupWithinVenue = [];
  for (const r of bookRows) {
    const venue = r.venue ?? '';
    const g = byVenue.get(venue) ?? { rows: 0, assets: new Set(), tokens: new Set() };
    g.rows += 1;
    for (const t of [r.upTokenId, r.downTokenId]) {
      if (!t) continue;
      if (!g.tokens.add(t)) {
        dupWithinVenue.push({ venue, token: short(t) });
      }
      if (!tokenVenues.has(t)) tokenVenues.set(t, new Set());
      tokenVenues.get(t).add(venue);
    }
    g.assets.add(r.asset ?? '?');
    byVenue.set(venue, g);
  }
  const shared = [...tokenVenues.entries()].filter(([, vs]) => vs.size > 1);
  const dualKey = shared.length > 0;

  // phase 判别：镜像所是否有自己的轮钟（venueRounds = scanner 表里该所
  // 自有行开口的钟）。无自有钟的行 venue = 显示簿合成行（违建 #4 活体）。
  const clocksKnown = Array.isArray(round?.venueRounds);
  const ownClock = new Set(clocksKnown ? round.venueRounds.map((r) => r.venue) : []);
  const rowVenues = [...byVenue.keys()].filter((v) => v !== '');
  const synthesized = clocksKnown && rowVenues.some((v) => !ownClock.has(v));
  // 合并形态（N + 0）判在「开了轮钟的 venue」上：post-M1 行从 scanner 表
  // 自然来，有钟必有行 —— 有钟无行 = 该所的行被 token 唯一键吞进别的
  // venue 名下。账户绑定集只作披露（绑定 ≠ 本窗口在交易）。
  const clockVenuesNoRows = clocksKnown
    ? [...ownClock].filter((v) => !rowVenues.includes(v))
    : [];
  let phase, phaseNote;
  if (!clocksKnown) {
    phase = 'unknown';
    phaseNote = 'core predates venueRounds (pre-B1a) — phase 不可判';
  } else if (!dualKey) {
    phase = 'pre';
    phaseNote = '单 venue 形态：轮次 token 只在一个 venue 名下有行（旧 N + 0）';
  } else if (synthesized) {
    phase = 'pre';
    phaseNote = 'books 视图双行但部分 venue 无自有轮钟 → 显示簿合成行（寄生形态，' +
      '违建 #4 活体）；M1 后应转为自然双键行（该所出现在 venueRounds）';
  } else {
    phase = 'post';
    phaseNote = '双键形态：同 token 两 venue 各有行、各有自有轮钟（自然行）';
  }

  for (const v of rowVenues) {
    const g = byVenue.get(v);
    jline({
      mode: 'books', venue: v, rows: g.rows,
      assets: [...g.assets].sort(), distinctTokens: g.tokens.size,
      ownRoundClock: clocksKnown ? ownClock.has(v) : null,
    });
  }
  for (const [t, vs] of shared) {
    jline({ mode: 'books', sharedToken: short(t), venues: [...vs].sort(), dualKey: true });
  }
  const mergeForm = clockVenuesNoRows.length > 0 && !synthesized;
  jline({
    mode: 'books', phase, note: phaseNote,
    totalRows: bookRows.length, venuesWithRows: rowVenues.length,
    enabledVenues: [...enabledVenues].sort(), dualKey,
    sharedTokens: shared.length,
    clockVenuesNoRows, // 披露：有钟无行（合并形态嫌疑），判定见 assertions.noMergeForm
    assertions: {
      noDupWithinVenue: dupWithinVenue.length === 0,
      noMergeForm: !mergeForm,
    },
  });
  if (dupWithinVenue.length > 0) {
    console.error(`  ASSERT FAIL 同 venue 内 (venue,token) 重复行 ×${dupWithinVenue.length}: ` +
      JSON.stringify(dupWithinVenue.slice(0, 5)));
    return 1;
  }
  if (mergeForm) {
    console.error('  ASSERT FAIL 合并形态（N + 0）：这些 venue 开了轮钟却在自己名下一行都没有，' +
      `其 token 全在别的 venue 名下（${clockVenuesNoRows.join(', ')}）—— RFC §4.3 双键未生效`);
    return 1;
  }
  console.log(`  ok — rows=${bookRows.length} venues=${rowVenues.length} ` +
    `dualKey=${dualKey} phase=${phase}`);
  return 0;
}

/** ② trades — 行 venue ≡ 执行身份 + 徽标/滤网对照。 */
async function probeTrades() {
  banner('trades');
  const [hist, accounts] = await Promise.all([
    rpc('trades.history', { limit: TRADE_LIMIT }),
    rpc('account.list'),
  ]);
  const rowsAll = Array.isArray(hist?.trades) ? hist.trades : [];
  // 与核自己的折算同规：无 accountId 的行（E28 前）归 default。
  const rowsOf = (id) => rowsAll.filter((r) => (r.accountId || 'default') === id);
  const listed = accounts?.accounts ?? [];
  const known = new Set(listed.map((a) => a.id));
  const orphanIds = [...new Set(rowsAll.map((r) => r.accountId || 'default'))]
    .filter((id) => !known.has(id));

  let green = true;
  for (const a of listed) {
    const rows = rowsOf(a.id);
    let summaryTotal = null;
    try {
      const s = await rpc('trades.summary', { accountId: a.id });
      summaryTotal = s?.summary?.totalTrades ?? null;
    } catch { /* older core: parity 不可判 */ }
    const bound = a.venue ?? '';
    const matched = bound ? rows.filter((r) => r.venue === bound) : [];
    const legacy = bound ? rows.filter((r) => r.venue && r.venue !== bound) : [];
    const undeclared = rows.filter((r) => !r.venue);
    // 截图 bug 的判定面：徽标计（summary.totalTrades）vs 面板 venue 滤网
    // 命中行数。两者相等 = 滤网不再吞行 = bug 不可复现。
    const parity = bound && summaryTotal != null
      ? matched.length === Number(summaryTotal)
      : null;
    if (parity === false) green = false;
    jline({
      mode: 'trades', account: a.id, boundVenue: bound || null,
      status: a.status ?? null, rowsTotal: rows.length,
      venueMatchedRows: matched.length, summaryTotalTrades: summaryTotal,
      filterParity: parity,
      legacyTagged: legacy.length, undeclaredVenue: undeclared.length,
    });
    for (const r of legacy) {
      jline({
        mode: 'trades', legacyTagged: true, id: r.id, account: a.id,
        rowVenue: r.venue, boundVenue: bound, asset: r.asset ?? null,
        token: short(r.tokenId), exitReason: r.exitReason ?? null,
      });
    }
    if (bound && Number(summaryTotal) > 0 && matched.length === 0) {
      console.log(`  !! 账户 ${a.id}：徽标计 ${summaryTotal} 单，venue:${bound} 滤网筛出 0 行 ` +
        '—— 截图 bug 可复现（pre-M3 形态，行被表行标签吞掉）');
    }
  }
  for (const id of orphanIds) {
    const rows = rowsOf(id);
    jline({ mode: 'trades', account: id, unlisted: true, rowsTotal: rows.length });
  }
  jline({
    mode: 'trades', totalRows: rowsAll.length, accounts: listed.length,
    verdict: green
      ? 'post-form：所有绑定账户 行 venue ≡ 执行身份，滤网 ≡ 徽标（截图 bug 不可复现）'
      : 'pre-form：存在 legacy-tagged 行（旧违建数据）—— 迁移后同脚本应转绿',
  });
  console.log(`  ok — rows=${rowsAll.length} accounts=${listed.length} ` +
    `verdict=${green ? 'post-form (green)' : 'pre-form (legacy evidence)'}`);
  return 0;
}

/** ③ upstream — 上游 feed/discovery 组件一份 + 活性读数。 */
async function probeUpstream() {
  banner('upstream');
  const [list, readout, round] = await Promise.all([
    rpc('market.list'),
    rpc('market.plugin.get'),
    rpc('engine.round'),
  ]);
  const plugins = list?.plugins ?? [];
  const enabled = plugins.filter((p) => p.enabled);
  const feedPlugins = enabled.filter((p) => p.hasDataFeed);
  const discoveryPlugins = enabled.filter((p) => p.hasDiscovery);
  const executors = enabled.filter((p) => p.hasExecutor);
  // M2 修正后的判据（RFC §五.2）：消费端（声明 feedSource）不新增组件——
  // 组件集合 ⊆ 原生（未声明 feedSource 的启用插件）集合。kalshi 等第二
  // 原生 venue 自持 feed 是合法的第二条流，因此判据不是「恒 ≡1」而是
  // 「没有消费端新增组件」。旧核 market.list 无 feedSource 字段 → 无法
  // 判别寄生，断言退化为 note 非 ASSERT（向后兼容，不误报）。
  const feedSourceKnown = enabled.some((p) => 'feedSource' in p);
  const nativeEnabled = feedSourceKnown ? enabled.filter((p) => !p.feedSource) : [];
  const nativeVenues = nativeEnabled.map((p) => p.name);
  const parasiticFeeds = feedSourceKnown
    ? feedPlugins.filter((p) => p.feedSource)
    : [];
  const parasiticDiscovery = feedSourceKnown
    ? discoveryPlugins.filter((p) => p.feedSource)
    : [];
  for (const p of plugins) {
    jline({
      mode: 'upstream', plugin: p.name, enabled: p.enabled, active: p.active,
      dataFeed: p.hasDataFeed, discovery: p.hasDiscovery, executor: p.hasExecutor,
      feedSource: p.feedSource ?? null,
      marketType: p.type ?? null,
    });
  }
  // 连接台账的组件面：上游连接数 ≡ 原生（自持）feed 组件数。寄生消费端
  // （声明 feedSource）不得新增 feed/discovery 组件——M2 复用后
  // polysimulator 声明 feedSource=polymarket，组件集合应仍为原生 venue
  // 集合（polymarket、kalshi 各自持一条），总上游流不因消费端启用而增长。
  const noParasiticComponents = feedSourceKnown
    && parasiticFeeds.length === 0 && parasiticDiscovery.length === 0;
  jline({
    mode: 'upstream', enabledVenues: readout?.derivedVenues ?? [],
    enablementSource: readout?.source ?? null, activePlugin: readout?.activePlugin ?? null,
    nativeVenues,
    feedComponents: feedPlugins.map((p) => p.name),
    discoveryComponents: discoveryPlugins.map((p) => p.name),
    executorComponents: executors.map((p) => p.name),
    venueRounds: (round?.venueRounds ?? []).map((r) => r.venue),
    assertion: feedSourceKnown
      ? { noParasiticComponents }
      : { upstreamComponentsOne: null },
  });
  if (feedSourceKnown && !noParasiticComponents) {
    console.error('  ASSERT FAIL 寄生组件泄漏：声明 feedSource 的消费端插件仍挂着' +
      '自己的 feed/discovery 组件 —— ' +
      `feed=${parasiticFeeds.map((p) => p.name)} ` +
      `discovery=${parasiticDiscovery.map((p) => p.name)}；` +
      'M2 复用后消费端应只吃源家的上游流（组件集合 ⊆ 原生 venue 集合）');
    return 1;
  }
  if (!feedSourceKnown) {
    console.log('  note 旧核（market.list 无 feedSource）— 寄生判别退化，' +
      `组件计数仅记录：feed=${feedPlugins.map((p) => p.name)} ` +
      `discovery=${discoveryPlugins.map((p) => p.name)}`);
  }
  // 活性读数（非断言）：feed 计数窗 + tickHealth。两条独立上游会把同一批
  // token 的 tops 速率翻倍 —— 人工比对口径。
  const s1 = await rpc('engine.stats');
  await sleep(SAMPLE_SEC * 1000);
  const s2 = await rpc('engine.stats');
  jline({
    mode: 'upstream', sampleSec: SAMPLE_SEC,
    feedCounters: {
      booksTotal: s2?.books ?? null, topsTotal: s2?.tops ?? null,
      booksDelta: s2?.books != null && s1?.books != null ? s2.books - s1.books : null,
      topsDelta: s2?.tops != null && s1?.tops != null ? s2.tops - s1.tops : null,
    },
    tickHealth: s2?.probe?.tickHealth == null ? null : {
      ...s2.probe.tickHealth,
      // 内核回报的零 tick token 列表是全值 —— 与本脚本同规截断后再出。
      zeroTickTokens: (s2.probe.tickHealth.zeroTickTokens ?? []).map(short),
    },
  });
  console.log(`  ok — feed×${feedPlugins.length} discovery×${discoveryPlugins.length} ` +
    `native=[${nativeVenues.join(',')}] ` +
    `(Δbooks=${s2?.books - s1?.books} Δtops=${s2?.tops - s1?.tops}/${SAMPLE_SEC}s)`);
  return 0;
}

async function main() {
  console.log(`venue-identity-probe (READ-ONLY) → ${SOCK}` +
    (SOCK_OVERRIDE ? ' (--socket override)' : ' (canonical)'));
  let ready;
  try {
    ready = await rpc('core.ready');
  } catch (e) {
    console.error(`  FAIL 该 socket 上没有应答的核（${e.message}）。`);
    console.error(`  socket: ${SOCK}`);
    console.error('  先起栈（网关/面板会拉起内核），或用 --socket 指向一个私有核：');
    console.error('    node scripts/venue-identity-probe.mjs --socket /path/to/core.sock books');
    process.exit(2);
  }
  console.log(`  core mode=${ready.mode} commit=${ready.commit ?? '?'} ` +
    `pid=${ready.pid ?? '?'} startedAt=${ready.startedAtMs ?? '?'} ` +
    `build=${ready.build ?? '?'}`);
  let bad = 0;
  for (const m of MODES) {
    try {
      bad += await (m === 'books' ? probeBooks()
        : m === 'trades' ? probeTrades()
        : probeUpstream());
    } catch (e) {
      console.error(`  FAIL ${m}: ${e.message}`);
      bad += 1;
    }
  }
  process.exit(bad > 0 ? 1 : 0);
}

main().catch((e) => {
  console.error(`venue-identity-probe — harness error: ${e?.stack || e}`);
  process.exit(2);
});
