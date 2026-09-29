#!/usr/bin/env node
/**
 * kline-aggregation-check — DEV_V0_3 §16 E29 gate (issue #335, §10.3 + §12.2).
 *
 * The contract, split by where it can honestly be proven:
 *
 *   WIRE FACE (this gate, real dry core):
 *     * `kline.history` answers the frozen envelope — {symbol, interval,
 *       klines[]} — with every bar obeying the §10.2 arithmetic:
 *         - closeTimeMs == openTimeMs + intervalMs - 1 (constructor-derived;
 *           a caller-passed close time cannot disagree, and any bar that does
 *           is a red flag for the whole derivation);
 *         - openTimeMs sits ON the UTC bucket boundary (div_euclid floor, not
 *           a +1 rounding — a bar starting at …001 is bucket misalignment);
 *         - openTimeMs strictly advances — out-of-order input is DROPPED and
 *           counted (§10.3), never emitted, so emitted bars are monotonic;
 *         - high >= max(open, close), low <= min(open, close), prices inside
 *           the prediction band;
 *         - tradeCount >= 1: a bar with no trades does not exist (silent
 *           buckets are counted as no_data_bars, not fabricated as bars);
 *         - the LAST bar is the growing one (isClosed=false), every earlier
 *           bar is closed exactly once.
 *     * closes are NEVER throttled: every bar that closed after the
 *       subscription arrived as exactly one KLINE_UPDATE event; previews
 *       (isClosed=false) are throttled to <= 1/s per (symbol, interval) —
 *       an 80-tick feed would flood ~80 previews unthrottled, so an 8-cap
 *       catches a throttle regression loudly.
 *     * subscriptions are SESSION-scoped: a second connection subscribing to
 *       the same series sees count 1, not 2 (a shared set would say 2); after
 *       kline.unsubscribe the session receives no K-line events at all.
 *     * limit is clamped kernel-side (1..=1000) and truncates to the NEWEST
 *       bars, growing bar last.
 *
 *   RUST UNIT TESTS (cannot be a JS gate — pinned in kline/aggregator.rs):
 *     * the 10k-tick LCG stream vs the offline reference, field by field
 *       (OHLCV, tradeCount, open/close times) — `lcg_stream_matches_…`;
 *     * no_data_bars counting, the 1000-bar cap, take_closed draining once,
 *       the div_euclid pre-epoch bucket, out-of-order drop + counter.
 *     A JS re-run of the 10k benchmark would re-implement the aggregator to
 *     test it — a weaker check than the unit test that runs the real one.
 *
 *   STRUCTURAL (unobservable on the wire, by design): a session's subscription
 *   set dies with its connection — the set lives in the session task and the
 *   disconnect IS the drop. There is no verb to read another session's set,
 *   so the gate pins the observable half (opt-in filtering, per-session
 *   counts, unsubscribe) and the Rust session tests pin the rest.
 *
 * Usage:
 *   node scripts/kline-aggregation-check.mjs              # the real verdict
 *   node scripts/kline-aggregation-check.mjs --self-test  # judge fixtures, no binary
 *   node scripts/kline-aggregation-check.mjs --teeth      # must go red
 * Exit: 0 pass / 1 verdict failure / 2 environment missing.
 */

import { spawn } from './lib/child-guard.mjs';
import { CoreClient } from './lib/core-client.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { waitForSocket, sleep } from './lib/wait.mjs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { existsSync, mkdtempSync, rmSync, unlinkSync } from 'fs';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const SYMBOL = 'tok';
const INTERVAL = 'sec1';
const INTERVAL_MS = 1000;

/**
 * The pure verdict for ONE bar list — every problem names what a broken
 * aggregator would have hidden, using the §16.6 vocabulary.
 */
export function judgeBars(bars, { symbol = SYMBOL, intervalMs = INTERVAL_MS } = {}) {
  const problems = [];
  if (!Array.isArray(bars) || bars.length === 0) return ['history carries no bars'];
  let prevOpen = null;
  bars.forEach((b, i) => {
    const where = `bar[${i}]`;
    if (b?.symbol !== symbol) problems.push(`${where}: symbol ${JSON.stringify(b?.symbol)} != ${JSON.stringify(symbol)}`);
    const open = Number(b?.openTimeMs);
    const close = Number(b?.closeTimeMs);
    if (!Number.isFinite(open)) problems.push(`${where}: openTimeMs not a number`);
    if (!Number.isFinite(close) || close !== open + intervalMs - 1) {
      problems.push(`${where}: close time invariant violated — closeTimeMs ${b?.closeTimeMs} != openTimeMs + ${intervalMs - 1} (the constructor derives it; a disagreeing bar means a caller-passed close time exists)`);
    }
    if (Number.isFinite(open) && open % intervalMs !== 0) {
      problems.push(`${where}: bucket alignment violated — openTimeMs ${open} is not on a ${intervalMs}ms UTC boundary (div_euclid floor, not +1 rounding)`);
    }
    if (prevOpen !== null && Number.isFinite(open) && open <= prevOpen) {
      problems.push(`${where}: monotonicity violated — openTimeMs ${open} does not advance past ${prevOpen} (out-of-order input is dropped and counted, never emitted)`);
    }
    const o = Number(b?.open), h = Number(b?.high), l = Number(b?.low), c = Number(b?.close);
    if (![o, h, l, c].every(Number.isFinite)) {
      problems.push(`${where}: OHLC not numeric`);
    } else {
      if (h < Math.max(o, c) - 1e-12) problems.push(`${where}: OHLC inconsistent — high ${h} < max(open, close) ${Math.max(o, c)}`);
      if (l > Math.min(o, c) + 1e-12) problems.push(`${where}: OHLC inconsistent — low ${l} > min(open, close) ${Math.min(o, c)}`);
      if (o <= 0 || o > 1) problems.push(`${where}: outside the prediction band — open ${o}`);
    }
    if (!(Number(b?.tradeCount) >= 1)) {
      problems.push(`${where}: tradeCount ${b?.tradeCount} < 1 — a bar with no trades must not exist (silent buckets are no_data_bars, not bars)`);
    }
    if (Number.isFinite(open)) prevOpen = open;
  });
  const last = bars[bars.length - 1];
  if (last?.isClosed !== false) {
    problems.push(`the LAST bar must be the growing one (isClosed=false) — history is the closed tail + the growing bar last, got isClosed=${last?.isClosed}`);
  }
  bars.slice(0, -1).forEach((b, i) => {
    if (b?.isClosed !== true) {
      problems.push(`bar[${i}] (openTimeMs ${b?.openTimeMs}) is not closed — a closed bar closes exactly once and never reopens`);
    }
  });
  return problems;
}

/** The whole `kline.history` reply: envelope first, then the bars. */
export function judgeHistoryDoc(doc, { symbol = SYMBOL, interval = INTERVAL, intervalMs = INTERVAL_MS, limit = null } = {}) {
  const problems = [];
  if (!doc || typeof doc !== 'object') return ['history reply is not an object'];
  if (doc.error) problems.push(`history reply carries an error: ${doc.error}`);
  if (doc.symbol !== symbol) problems.push(`envelope symbol ${JSON.stringify(doc.symbol)} != ${JSON.stringify(symbol)}`);
  if (doc.interval !== interval) problems.push(`envelope interval ${JSON.stringify(doc.interval)} != ${JSON.stringify(interval)}`);
  if (!Array.isArray(doc.klines)) return [...problems, 'envelope carries no klines array'];
  if (limit != null && doc.klines.length > limit) {
    problems.push(`limit clamp violated — asked ${limit}, got ${doc.klines.length} bars`);
  }
  problems.push(...judgeBars(doc.klines, { symbol, intervalMs }).map((p) => `klines: ${p}`));
  return problems;
}

/** The session's KLINE_UPDATE stream: closed events one-per-bar, previews throttled. */
export function judgeEventStream(events, { symbol = SYMBOL, interval = INTERVAL } = {}) {
  const problems = [];
  for (const ev of events) {
    if (ev?.kind !== 'KLINE_UPDATE') problems.push(`non-KLINE_UPDATE event leaked through the filter: ${ev?.kind}`);
    const k = ev?.kline;
    if (!k) { problems.push('KLINE_UPDATE carries no kline'); continue; }
    if (k.symbol !== symbol || k.interval !== interval) {
      problems.push(`event for (${k.symbol}, ${k.interval}) reached a session subscribed to (${symbol}, ${interval}) — OPT-IN filter breached`);
    }
  }
  return problems;
}

function bar(over = {}) {
  return {
    symbol: SYMBOL, interval: INTERVAL,
    openTimeMs: 1_758_888_010_000, closeTimeMs: 1_758_888_010_999,
    open: 0.4, high: 0.42, low: 0.39, close: 0.42,
    volume: 3, tradeCount: 3, isClosed: true, ...over,
  };
}

function selfTest() {
  const good = [bar({ close: 0.41, isClosed: true }), bar({ openTimeMs: 1_758_888_011_000, closeTimeMs: 1_758_888_011_999 }), bar({ openTimeMs: 1_758_888_012_000, closeTimeMs: 1_758_888_012_999, isClosed: false })];
  const rows = [
    ['a closed tail + growing bar is clean', judgeBars(good), 0, null],
    ['an empty history is red', judgeBars([]), 1, 'no bars'],
    ['teeth A: out-of-order bars are red', judgeBars([good[1], good[0], good[2]]), 1, 'monotonicity violated'],
    ['teeth B: a +1-rounded open is red', judgeBars([bar({ openTimeMs: 1_758_888_010_001, closeTimeMs: 1_758_888_011_000 })]), 2, 'bucket alignment violated'],
    ['teeth C: a caller-passed close time is red', judgeBars([bar({ closeTimeMs: 1_758_888_010_998 })]), 1, 'close time invariant violated'],
    ['a non-monotonic duplicate open is red', judgeBars([good[0], bar({ openTimeMs: 1_758_888_010_000, isClosed: false })]), 1, 'monotonicity violated'],
    ['high below max(open, close) is red', judgeBars([bar({ high: 0.405 })]), 1, 'OHLC inconsistent'],
    ['low above min(open, close) is red', judgeBars([bar({ low: 0.405 })]), 1, 'OHLC inconsistent'],
    ['an out-of-band open is red', judgeBars([bar({ open: 1.5, high: 1.5, low: 1.5, close: 1.5 })]), 1, 'prediction band'],
    ['a zero-trade bar is red', judgeBars([bar({ tradeCount: 0, volume: 0 })]), 1, 'no trades'],
    ['a wrong-symbol bar is red', judgeBars([bar({ symbol: 'other' })]), 1, 'symbol'],
    ['a closed bar after the growing bar is red', judgeBars([good[2], good[0]]), 1, 'growing one'],
    ['a mid-history growing bar is red', judgeBars([good[0], good[2], good[1]]), 1, 'never reopens'],
    ['the envelope (good kernel) is clean', judgeHistoryDoc({ symbol: SYMBOL, interval: INTERVAL, klines: good }), 0, null],
    ['the envelope with an error field is red', judgeHistoryDoc({ symbol: SYMBOL, interval: INTERVAL, klines: good, error: 'x' }), 1, 'error'],
    ['a limit overflow is red', judgeHistoryDoc({ symbol: SYMBOL, interval: INTERVAL, klines: good }, { limit: 2 }), 1, 'limit clamp'],
    ['an interval mismatch is red', judgeHistoryDoc({ symbol: SYMBOL, interval: 'min5', klines: good }), 1, 'envelope interval'],
    ['a clean opt-in event stream is clean', judgeEventStream([{ kind: 'KLINE_UPDATE', kline: good[0] }, { kind: 'KLINE_UPDATE', kline: { ...good[2] } }]), 0, null],
    ['a foreign-series event is red', judgeEventStream([{ kind: 'KLINE_UPDATE', kline: { ...good[0], symbol: 'nope' } }]), 1, 'OPT-IN filter breached'],
  ];
  let bad = 0;
  for (const [name, problems, want, mention] of rows) {
    const ok = want === 0 ? problems.length === 0
      : problems.length >= want && (mention === null || problems.some((p) => p.includes(mention)));
    if (!ok) { bad += 1; console.error(`  FAIL ${name} — ${JSON.stringify(problems)}`); }
    else console.log(`  ok   ${name}`);
  }
  if (bad > 0) { console.error(`\nkline-aggregation self-test: ${bad} of ${rows.length} fixtures failed`); process.exit(1); }
  console.log(`\nkline-aggregation self-test: ${rows.length} fixtures passed`);
}

function teeth() {
  // §16.6: feed the broken implementations' OUTPUT to the judges; the judges
  // must go red naming the expected message, else this gate has no teeth.
  const good = [bar({}), bar({ openTimeMs: 1_758_888_011_000, closeTimeMs: 1_758_888_011_999, isClosed: false })];
  const mutations = [
    { name: 'teeth A: out-of-order input emitted instead of dropped',
      run: () => judgeBars([good[1], good[0]]), mention: 'monotonicity violated' },
    { name: 'teeth B: bucket_open_ms rounds up (+1) instead of div_euclid floor',
      run: () => judgeBars([bar({ openTimeMs: 1_758_888_010_001, closeTimeMs: 1_758_888_011_000, isClosed: false })]), mention: 'bucket alignment violated' },
    { name: 'teeth C: close_time_ms accepted from the caller (1ms off)',
      run: () => judgeBars([bar({ closeTimeMs: 1_758_888_010_998, isClosed: false })]), mention: 'close time invariant violated' },
    { name: 'teeth D: a silent no-data bucket materialized as a bar (§10.3 violated)',
      run: () => judgeBars([bar({}), bar({ openTimeMs: 1_758_888_011_000, closeTimeMs: 1_758_888_011_999 }), bar({ openTimeMs: 1_758_888_012_000, closeTimeMs: 1_758_888_012_999, isClosed: false })].map((b, i) => i === 1 ? { ...b, tradeCount: 0, volume: 0 } : b)), mention: 'no trades' },
    { name: 'teeth E: the OPT-IN filter off (a foreign series reaches the session)',
      run: () => judgeEventStream([{ kind: 'KLINE_UPDATE', kline: { ...good[0], symbol: 'other-token' } }]), mention: 'OPT-IN filter breached' },
    { name: 'teeth F: history answers an error envelope as if it were bars',
      run: () => judgeHistoryDoc({ symbol: SYMBOL, interval: INTERVAL, klines: good, error: 'kernel busy' }), mention: 'error' },
  ];
  let missed = 0;
  for (const m of mutations) {
    const problems = m.run();
    const caught = problems.length > 0 && problems.some((p) => p.includes(m.mention));
    console.log(`${caught ? '  ok  ' : '  FAIL'} teeth: ${m.name}`);
    if (caught) console.log(`       caught: ${problems.find((p) => p.includes(m.mention)).slice(0, 140)}`);
    else missed += 1;
  }
  if (missed === 0) {
    console.log('\nkline-aggregation --teeth: every broken aggregator output was caught — the gate has teeth');
    process.exit(0);
  }
  console.error(`\nkline-aggregation --teeth: ${missed} mutation(s) slipped through green — the gate has no teeth`);
  process.exit(1);
}

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  if (argv.includes('--teeth')) return teeth();

  if (!existsSync(CORE)) {
    console.error(`missing core binary ${CORE}: cargo build --release --workspace --locked`);
    process.exit(2);
  }

  const gate = createChecks();
  const { check } = gate;
  const temp = mkdtempSync(join(tmpdir(), 'bk-e29-kline-'));
  const sock = join(tmpdir(), `bk-e29-kline-${process.pid}.sock`);
  try { unlinkSync(sock); } catch { /* fresh */ }

  const proc = spawn(CORE, [
    '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--max-order-notional', '6',
    '--engine', '--no-discovery', '--no-event-archive',
    '--no-trade-log', '--no-order-log', '--no-position-log', '--no-strategy-dir',
  ], { stdio: ['ignore', 'ignore', 'pipe'], cwd: temp });
  await waitForSocket(sock, { timeoutMs: 15000 });
  const a = await CoreClient.connect({ socketPath: sock });
  await a.request('core.ready');

  const eventsA = [];
  a.onEvent = (ev) => { if (ev?.kind === 'KLINE_UPDATE') eventsA.push(ev); };

  // A round for the engine's market layer; the K-line feed itself is token-keyed.
  const now = Date.now();
  const slot = Math.floor(now / 1000 / 3600);
  await a.request('engine.markets', {
    markets: [{
      asset: 'BTC', conditionId: '0xc', questionId: '0xq',
      upTokenId: SYMBOL, downTokenId: 'DOWN', upPrice: 0.5, downPrice: 0.5,
      expiresAtMs: (slot + 1) * 3600 * 1000, roundSlot: slot,
      negRisk: true, question: 'BTC up/down',
    }],
  });

  const feed = (mid) => a.request('engine.book', {
    tokenId: SYMBOL,
    bids: [{ price: mid - 0.01, size: 100 }],
    asks: [{ price: mid + 0.01, size: 100 }],
  });

  // ── session scoping: two connections, two independent sets ────────────────
  const sub1 = await a.request('kline.subscribe', { symbols: [SYMBOL], intervals: [INTERVAL] });
  check('kline.subscribe answers the session count', sub1?.subscribed === 1, JSON.stringify(sub1));

  const b = await CoreClient.connect({ socketPath: sock });
  const eventsB = [];
  b.onEvent = (ev) => { if (ev?.kind === 'KLINE_UPDATE') eventsB.push(ev); };
  const sub2 = await b.request('kline.subscribe', { symbols: [SYMBOL], intervals: [INTERVAL] });
  check('a second session subscribes INDEPENDENTLY (a shared set would report 2)',
    sub2?.subscribed === 1, JSON.stringify(sub2));

  // ── feed ~80 ticks paced ~50ms: crosses 3+ second boundaries ─────────────
  for (let i = 0; i < 80; i++) {
    await feed(i % 2 === 0 ? 0.40 : 0.42);
    await sleep(50);
  }
  await sleep(700); // closes push immediately; let the last beat settle

  const hist = await a.request('kline.history', { symbol: SYMBOL, interval: INTERVAL, limit: 1000 });
  const docProblems = judgeHistoryDoc(hist);
  check('kline.history answers the frozen envelope with §10.2-clean bars',
    docProblems.length === 0, docProblems.join(' | ').slice(0, 300));

  const closedOpens = (hist?.klines ?? []).filter((k) => k.isClosed).map((k) => Number(k.openTimeMs));
  check('the feed crossed enough bucket boundaries to make closes observable',
    closedOpens.length >= 2, `${closedOpens.length} closed bar(s)`);

  const closedEventOpens = eventsA.filter((e) => e.kline?.isClosed).map((e) => Number(e.kline.openTimeMs));
  const missing = closedOpens.filter((o) => !closedEventOpens.includes(o));
  const dupes = closedEventOpens.length !== new Set(closedEventOpens).size;
  check('every closed bar arrived as EXACTLY one KLINE_UPDATE (closes are never throttled)',
    missing.length === 0 && !dupes,
    `history closed=${closedOpens.length} events=${closedEventOpens.length}${missing.length ? ` missing=[${missing}]` : ''}${dupes ? ' DUPLICATES' : ''}`);

  const previews = eventsA.filter((e) => e.kline && !e.kline.isClosed);
  check('growing-bar previews are throttled to <= 1/s per (symbol, interval)',
    previews.length >= 1 && previews.length <= 8,
    `${previews.length} preview(s) over the feed window (unthrottled would be ~80)`);

  const streamProblems = judgeEventStream(eventsA);
  check('the session received ONLY its subscribed series',
    streamProblems.length === 0, streamProblems.join(' | ').slice(0, 200));
  check('the second session received the same closes (both opted in)',
    eventsB.filter((e) => e.kline?.isClosed).length >= 1,
    `${eventsB.filter((e) => e.kline?.isClosed).length} close event(s)`);

  // ── limit clamp truncates to the newest ──────────────────────────────────
  const h1 = await a.request('kline.history', { symbol: SYMBOL, interval: INTERVAL, limit: 1 });
  const newest = Number(hist?.klines?.[hist.klines.length - 1]?.openTimeMs);
  check('limit=1 returns exactly the newest bar',
    h1?.klines?.length === 1 && Number(h1.klines[0].openTimeMs) === newest,
    `got ${h1?.klines?.length} bar(s), openTimeMs ${h1?.klines?.[0]?.openTimeMs} vs newest ${newest}`);

  // ── disconnect is the unsubscribe; unsubscribe silences the session ──────
  b.stop(); // adopted client: closes the socket, which drops the session
  await sleep(200);
  const unsub = await a.request('kline.unsubscribe', { symbols: [SYMBOL], intervals: [INTERVAL] });
  check('kline.unsubscribe empties THIS session\'s set (B\'s life and death left no trace)',
    unsub?.subscribed === 0, JSON.stringify(unsub));

  const marker = eventsA.length;
  for (let i = 0; i < 24; i++) {
    await feed(0.40);
    await sleep(50);
  }
  await sleep(1800); // one preview beat + settle: any leak would have arrived
  const leaked = eventsA.slice(marker);
  check('after unsubscribe the session receives no K-line events (OPT-IN enforced)',
    leaked.length === 0, leaked.length ? `leaked ${leaked.length} event(s), first isClosed=${leaked[0]?.kline?.isClosed}` : '');

  a.stop();
  try { proc.kill('SIGTERM'); } catch { /* already gone */ }

  const failed = gate.failures;
  console.log(`\nRESULT: ${failed === 0 ? 'PASS' : `FAIL (${failed})`} — bars obey §10.2 arithmetic, closes are never throttled, previews are, and subscriptions are session-scoped`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`kline-aggregation-check interrupted: ${String(e.message || e).slice(0, 500)}`);
  process.exit(1);
});
