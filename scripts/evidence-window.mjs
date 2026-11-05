#!/usr/bin/env node
/**
 * [实盘准入门] evidence-window runner — issue #854 第一刀.
 *
 * The DRY evidence window (#854) needs a RECORDER, not a trader: this script
 * attaches to a RUNNING core (the dry evidence stack the supervisor spawned —
 * same attach-only convention as soak-monitor.mjs: resolveSocketPath +
 * requestOnce, no spawn, no credentials) and every `--interval-sec` seconds
 * (default 300 = 5 min) captures the observation slice into
 *
 *   data/evidence/<strategy>/snapshots.jsonl        (one JSON line per cycle)
 *
 * and folds each CLOSED weekly bucket of a strategy's trades into
 *
 *   data/evidence/<strategy>/weekly.jsonl           (one envelope per week)
 *
 * whose `summary` object reuses trade_db.rs's TradeSummary field names
 * verbatim (totalTrades / wins / losses / winRate / totalGrossPnl /
 * totalFees / totalNetPnl / avgHoldTimeSec / bestTradePnl / worstTradePnl /
 * lastUpdated, serde camelCase) and whose numbers are computed by a 1:1 port
 * of trade_db.rs fold_into (a net >= 0 row is a win; the running-mean hold
 * time). No schema is invented; the fold is cross-checked against the
 * snapshot aggregate deltas in every weekly report.
 *
 * KILL LINES ARE READ-ONLY. The two preregistered verdicts —
 *   (a) net-loss line   : window net PnL <= -20% x seed
 *   (b) starvation line : `--dead-cycles` consecutive cycles where
 *                         `evaluations` grew while the engine placed
 *                         ZERO orders
 * print ONE warning line to STDERR (and are recorded in the snapshot). This
 * script NEVER calls `risk.kill` and never builds its own freeze — #854 is
 * explicit that freezing stays with the operator over the EXISTING venue /
 * account freeze books. The `--self-test` asserts the absence of every
 * write-call verb in this source.
 *
 * COARSENESS (preregistered, issue #854 + evaluate.rs:178): `engine.stats.
 * signals` counts ENGINE-PLACED ORDERS (`self.stats.signals += orders.len()`),
 * NOT evaluated signals. The starvation verdict "evaluations grew, orders 0"
 * is therefore a coarse first cut — it cannot say WHY nothing was placed, and
 * per-reason zero-fill buckets (OrderRequest.reason) are ANOTHER TICKET: they
 * touch the OrderRequest contract face, which sits in the #840 freeze domain.
 * This tool does not solve that; it records that the question is open.
 *
 * Usage:
 *   node scripts/evidence-window.mjs [--strategy <name>] [--interval-sec 300]
 *        [--hours 24 | --forever] [--socket <path>] [--seed-balance 1000]
 *        [--dead-cycles 100] [--window-days 14]
 *   node scripts/evidence-window.mjs --self-test
 *
 * `--strategy` names the strategy under observation; it scopes the audit-tail
 * filter and the weekly fold's attribution (trades carry `strategy`), and it
 * names the evidence directory. `all` = attribute per trade row instead of
 * filtering (multi-strategy comparison runs).
 */

import { appendFileSync, existsSync, mkdirSync, readFileSync } from 'fs';
import { join, dirname, resolve } from 'path';
import { fileURLToPath } from 'url';
import { resolveSocketPath } from './lib/core-socket.mjs';
import { requestOnce } from './lib/core-client.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
const DAY_MS = 24 * 3600 * 1000;

// PREREGISTERED window constants (issue #854 / docs/evidence-window.md).
// These are the plan-of-record numbers, not tuning knobs: 14 days, or >= 300
// qualified signal rounds, whichever arrives first; kill lines fixed below.
const MIRROR_WINDOW_MS = 14 * DAY_MS;        // the snapshot window this runner reasons about
const WINDOW_MIN_ROUNDS = 300;               // …or >= 300 qualified signal rounds
const NET_LOSS_LINE_PCT = 20;                // kill line (a): -20% of seed
const DEFAULT_DEAD_CYCLES = 100;             // kill line (b): 100 zero-order cycles
const WEEK_MS = 7 * DAY_MS;

// ── Args ─────────────────────────────────────────────────────────────────────
const argv = process.argv.slice(2);
const opt = (name, def) => {
  const i = argv.indexOf(name);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[i + 1] : def;
};
const SELF_TEST = process.argv.includes('--self-test');

const STRATEGY = opt('--strategy', 'all');
const INTERVAL_SEC = parseInt(opt('--interval-sec', '300'), 10);
const HOURS = parseFloat(opt('--hours', '24'));
const FOREVER = argv.includes('--forever');
const SOCKET = opt('--socket', resolveSocketPath());
const SEED_ARG = opt('--seed-balance', null);
const DEAD_CYCLES = parseInt(opt('--dead-cycles', String(DEFAULT_DEAD_CYCLES)), 10);
const WINDOW_DAYS = parseInt(opt('--window-days', '14'), 10);

if (SELF_TEST) {
  // The self-test exercises pure functions only; argument validation runs in
  // the live path so a typo in a real run stays a loud exit 2.
} else {
  const bad = (name, v) => {
    console.error(`invalid ${name} ${JSON.stringify(v)}: use a positive number`);
    process.exit(2);
  };
  if (!(INTERVAL_SEC > 0)) bad('--interval-sec', opt('--interval-sec', ''));
  if (!FOREVER && !(HOURS > 0)) bad('--hours', opt('--hours', ''));
  if (!(DEAD_CYCLES > 0)) bad('--dead-cycles', opt('--dead-cycles', ''));
  if (!(WINDOW_DAYS > 0)) bad('--window-days', opt('--window-days', ''));
}

// BK_EVIDENCE_DIR: the same out-of-real-data seam soak-monitor's BK_SOAK_DIR
// gives its harnesses; the default is the real evidence directory. `data/` is
// gitignored — evidence stays on the operator's disk, the repo carries only
// the tool and the preregistration doc.
const EVIDENCE_ROOT = process.env.BK_EVIDENCE_DIR
  ? resolve(ROOT, process.env.BK_EVIDENCE_DIR)
  : join(ROOT, 'data', 'evidence');
const DIR = join(EVIDENCE_ROOT, STRATEGY);
const SNAPSHOTS = join(DIR, 'snapshots.jsonl');
const WEEKLY = join(DIR, 'weekly.jsonl');

// ── RPC helper ───────────────────────────────────────────────────────────────
/**
 * A sample, not an assertion (soak-monitor convention): a refused or
 * unanswered call is returned as `{ error }` and lands in the snapshot — a
 * recording of a hole is itself evidence, never a reason to end the run.
 * Every verb this script ever issues is READ-ONLY (see the self-test's
 * write-verb assertion).
 */
async function rpc(method, params = {}, timeoutMs = 5000) {
  try {
    return { result: await requestOnce(SOCKET, method, params, { timeoutMs }) };
  } catch (e) {
    return { error: String(e?.message ?? e) };
  }
}

// ── Pure helpers (exported for --self-test) ──────────────────────────────────

/** Decimal wire: finite f64, or the string spelling. Anything else is 0. */
export function num(v) {
  if (typeof v === 'number') return Number.isFinite(v) ? v : 0;
  if (typeof v === 'string' && v.trim() !== '') {
    const n = Number(v);
    return Number.isFinite(n) ? n : 0;
  }
  return 0;
}

/**
 * The UTC-Monday 00:00 bucket a timestamp belongs to. The weekly report's
 * ONLY time boundary — no locale, no business-week judgment calls.
 */
export function weekStartMs(ts) {
  const d = new Date(ts);
  const midnight = Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate());
  const dow = (d.getUTCDay() + 6) % 7; // Mon=0 … Sun=6
  return midnight - dow * DAY_MS;
}

/**
 * trade_db.rs fold_into (line 234) ported 1:1 over TradeRecord camelCase
 * rows: a trade is a WIN when its net PnL is >= 0 (trade_db folds at
 * `net >= 0.0`), winRate = wins/total*100, average hold time is the
 * running-mean formula, best/worst fold the net. Field names are the
 * TradeSummary's (trade_db.rs:135, serde camelCase) — nothing invented.
 */
export function foldTradeSummary(rows) {
  const s = {
    totalTrades: 0, wins: 0, losses: 0, winRate: 0,
    totalGrossPnl: 0, totalFees: 0, totalNetPnl: 0,
    avgHoldTimeSec: 0, bestTradePnl: 0, worstTradePnl: 0,
    lastUpdated: Math.floor(Date.now() / 1000),
  };
  for (const rec of rows) {
    const net = num(rec?.netPnlUsd);
    s.totalTrades += 1;
    if (net >= 0) s.wins += 1; else s.losses += 1;
    s.winRate = s.totalTrades > 0 ? (s.wins / s.totalTrades) * 100 : 0;
    s.totalGrossPnl += num(rec?.grossPnlUsd);
    s.totalFees += num(rec?.feesUsd);
    s.totalNetPnl += net;
    const n = s.totalTrades;
    s.avgHoldTimeSec = (s.avgHoldTimeSec * (n - 1) + num(rec?.holdTimeSec)) / n;
    s.bestTradePnl = Math.max(s.bestTradePnl, net);
    s.worstTradePnl = Math.min(s.worstTradePnl, net);
  }
  return s;
}

/**
 * Weekly attribution = trade's EXIT time inside the bucket. A trade opened
 * the previous week but exited inside this one belongs to THIS week — exit
 * is when fees and net PnL became fact.
 */
export function inWeek(rec, weekStart, weekEnd) {
  const t = Number(rec?.exitTime);
  return Number.isFinite(t) && t >= weekStart && t < weekEnd;
}

/** Which snapshots fall inside the evidence window. */
export function inWindow(snap, windowStart, windowEnd) {
  const t = Number(snap?.ts);
  return Number.isFinite(t) && t >= windowStart && t < windowEnd;
}

/**
 * The digest the report needs from one audit-tail record (records arrive
 * verbatim per views.rs:435 — camelCase rows with decision.status /
 * gates[].outcome): only NEW ones (above the high-water mark, no id the last
 * fetch already saw), with the REJECTing gates' reasons carried so a
 * zero-order regime can be eyeballed without inventing a bucket taxonomy.
 */
export function auditDigest(rec, since, seenIds) {
  const ts = Number(rec?.tsMs);
  const id = String(rec?.intentId ?? '');
  if (!Number.isFinite(ts) || ts <= since) return null;
  if (id && seenIds.has(id)) return null;
  const status = String(rec?.decision?.status ?? 'unknown');
  const rejects = [];
  for (const g of Array.isArray(rec?.gates) ? rec.gates : []) {
    if (String(g?.outcome ?? '').toUpperCase() === 'REJECT') {
      rejects.push(`${g?.gate ?? '?'}:${String(g?.detail ?? '').slice(0, 80)}`);
    }
  }
  return { ts, id, status, rejects };
}

/**
 * READ-ONLY kill-line verdicts: the return value is warning STRINGS and the
 * caller's only use of it is console.error + persisting them. There is no
 * third channel — no client, no request builder — so no edit short of
 * rewriting this file could fire anything (asserted by --self-test).
 */
export function killLine(state, now) {
  const warns = [];
  // (a) net-loss line: window net PnL vs -20% x seed. The window baseline is
  // the first summary OBSERVED inside the window (the kernel counts
  // all-time; the window arithmetic lives here — restart semantics are
  // "observed window", documented in docs/evidence-window.md). No seed
  // (live core: ledger.balance.seed is absent and no --seed-balance given)
  // means the line CANNOT fire: warn once, never treat seed as 0.
  if (state.curSummary && state.baseSummary) {
    const netWindow = num(state.curSummary.totalNetPnl) - num(state.baseSummary.totalNetPnl);
    const seed = state.seedUsd ?? null;
    if (seed == null || !(seed > 0)) {
      if (!state.netLossSeedUnknown) {
        state.netLossSeedUnknown = true;
        warns.push(`[EVIDENCE-KILL-LINE:net-loss] cannot evaluate: no seed known (ledger.balance.seed absent, no --seed-balance). Window net PnL so far ${netWindow.toFixed(4)} — the operator judges against the pilot principal by hand.`);
      }
    } else {
      state.netLossSeedUnknown = false;
      const line = -(NET_LOSS_LINE_PCT / 100) * seed;
      if (netWindow <= line) {
        if (!state.netLossBreached) {
          state.netLossBreached = true;
          warns.push(`[EVIDENCE-KILL-LINE:net-loss] window netPnlUsd=${netWindow.toFixed(4)} <= line ${line.toFixed(4)} (-${NET_LOSS_LINE_PCT}% of seed ${seed.toFixed(2)}). READ-ONLY verdict: stopping the strategy is the OPERATOR's act (issue #854 forbids a self-built freeze).`);
        }
      } else if (state.netLossBreached && netWindow > line * 0.9) {
        // Recovered 10% past the line before re-arming, so flapping around
        // the threshold warns once per genuine breach episode.
        state.netLossBreached = false;
      }
    }
  }
  // (b) starvation line: DEAD_CYCLES consecutive observed cycles with
  // evaluations growing and zero engine orders. COARSE by construction:
  // `signals` is the ORDER count (evaluate.rs:178), so this says "the
  // engine stopped placing", not "qualified rounds went unfilled".
  if (state.deadStreak >= DEAD_CYCLES && !state.deadBreached) {
    state.deadBreached = true;
    warns.push(`[EVIDENCE-KILL-LINE:starvation] ${state.deadStreak} consecutive cycles: evaluations grew, engine orders 0 (signals is the ORDER count — evaluate.rs:178 — so this is a coarse first cut, and OrderRequest.reason bucketing is another ticket, #840 contract face). READ-ONLY: the kill is the operator's act.`);
  }
  if (state.deadStreak === 0) state.deadBreached = false;
  return warns;
}

// ── Runtime state (restart-safe: seeded from the snapshot file) ─────────────
const state = {
  cycle: 0,
  curSummary: null,
  baseSummary: null,        // first summary observed in-window (net-loss base)
  seedUsd: null,
  netLossBreached: false, netLossSeedUnknown: false,
  deadStreak: 0, deadBreached: false,
  lastEvaluations: null, lastSignals: null, lastRounds: null,
  seenIntentIds: new Set(), lastAuditTs: 0,
  pendingWeekly: [],        // closed buckets still owed a report
  weeklyDone: new Set(),    // weekStart values already in weekly.jsonl
};

function ensureDir() { if (!existsSync(DIR)) mkdirSync(DIR, { recursive: true }); }

function readJsonl(path) {
  if (!existsSync(path)) return [];
  try {
    return readFileSync(path, 'utf8').split('\n').filter((l) => l.trim()).map((l) => {
      try { return JSON.parse(l); } catch { return null; }
    }).filter(Boolean);
  } catch { return []; }
}

/** Seed counters from the snapshot tail so a restart does not zero streaks. */
function initStateFromFile() {
  const snaps = readJsonl(SNAPSHOTS);
  if (snaps.length === 0) return;
  const last = snaps[snaps.length - 1];
  const lastEval = Number(last?.engine?.evaluations);
  const lastSig = Number(last?.engine?.signals);
  const lastRounds = Number(last?.engine?.rounds);
  if (Number.isFinite(lastEval)) state.lastEvaluations = lastEval;
  if (Number.isFinite(lastSig)) state.lastSignals = lastSig;
  if (Number.isFinite(lastRounds)) state.lastRounds = lastRounds;
  state.lastAuditTs = Number(last?.audit?.tsMax) || 0;
  state.cycle = Number(last?.cycle) || 0;
  state.seenIntentIds = new Set(
    Array.isArray(last?.audit?.recentIds) ? last.audit.recentIds.map(String) : [],
  );
  const base = snaps.find((s) => inWindow(s, Date.now() - MIRROR_WINDOW_MS, Date.now()) && s?.summary);
  if (base) state.baseSummary = base.summary;
}

/** Which CLOSED weekly buckets still owe a report (unreachable core defers,
 * never invents). */
function refreshPendingWeekly() {
  const now = Date.now();
  state.weeklyDone = new Set(
    readJsonl(WEEKLY).map((r) => String(r?.weekStart ?? '')).filter(Boolean),
  );
  for (const s of readJsonl(SNAPSHOTS)) {
    const t = Number(s?.ts);
    if (!Number.isFinite(t)) continue;
    const ws = weekStartMs(t);
    if (ws + WEEK_MS <= now && !state.weeklyDone.has(String(ws))) {
      state.pendingWeekly.push(ws);
    }
  }
  state.pendingWeekly = [...new Set(state.pendingWeekly)].sort((a, b) => a - b);
}

// Upper bound of the trades pull per weekly fold. Best-effort documented:
// HISTORY_LIMIT rows cover the window's earliest reachable week; beyond that
// the report says `truncated` rather than silently missing trades.
const HISTORY_LIMIT = 1000;

async function foldWeek(weekStart) {
  if (state.weeklyDone.has(String(weekStart))) return false;
  const hist = await rpc('trades.history', { limit: HISTORY_LIMIT });
  if (hist.error || !Array.isArray(hist.result?.trades)) return false; // deferred
  const weekEnd = weekStart + WEEK_MS;
  const rows = hist.result.trades.filter(
    (r) => inWeek(r, weekStart, weekEnd)
      && (STRATEGY === 'all' || String(r?.strategy ?? '') === STRATEGY),
  );
  const summary = foldTradeSummary(rows);
  // Cross-check: the trade-row fold vs the snapshot aggregate deltas over the
  // same span. A mismatch is VISIBLE evidence (a row the tail missed, or a
  // strategy filter biting), never swallowed.
  const snaps = readJsonl(SNAPSHOTS).filter(
    (s) => inWindow(s, weekStart, weekEnd) && s?.summary,
  );
  let snapNetDelta = null;
  if (snaps.length >= 2) {
    snapNetDelta = num(snaps[snaps.length - 1].summary.totalNetPnl)
      - num(snaps[0].summary.totalNetPnl);
  }
  const truncated = rows.length >= HISTORY_LIMIT
    && Number(rows[0]?.exitTime) >= weekStart;
  const report = {
    kind: 'weekly',
    strategy: STRATEGY,
    weekStart,
    weekEnd,
    weekIso: new Date(weekStart).toISOString().slice(0, 10),
    summary,                                  // ← TradeSummary shape (trade_db.rs:135)
    crossCheck: {
      snapshotNetDelta: snapNetDelta,
      foldNetDelta: summary.totalNetPnl,
      historyRows: rows.length,
      truncated,
    },
    generatedAtMs: Date.now(),
  };
  ensureDir();
  appendFileSync(WEEKLY, JSON.stringify(report) + '\n');
  state.weeklyDone.add(String(weekStart));
  console.log(`weekly fold ${report.weekIso}: trades=${summary.totalTrades} netPnl=${summary.totalNetPnl.toFixed(4)} fees=${summary.totalFees.toFixed(4)} winRate=${summary.winRate.toFixed(1)}%${truncated ? ' (TRUNCATED — history cap reached)' : ''}`);
  return true;
}

// ── One observation cycle ────────────────────────────────────────────────────
async function cycle() {
  const tsMs = Date.now();
  const [stats, summary, balance, orders, audit] = await Promise.all([
    rpc('engine.stats'),
    rpc('trades.summary', {}),
    rpc('ledger.balance'),
    rpc('orders.list'),
    rpc('intent.audit.tail', STRATEGY === 'all'
      ? { limit: 200 }
      : { limit: 200, strategy: STRATEGY }),
  ]);

  const cur = stats.result ?? null;
  const curSummary = summary.result?.summary ?? null;
  const curBalance = balance.result ?? null;

  // Window base for the net-loss line: the first summary OBSERVED in-window.
  if (curSummary) {
    const baseAge = state.baseSummary ? Number(state.baseSummary.lastUpdated) : NaN;
    if (!state.baseSummary || !Number.isFinite(baseAge) || baseAge < tsMs - MIRROR_WINDOW_MS) {
      state.baseSummary = curSummary;
    }
  }
  // Seed: ledger.balance.seed (the DRY principal), else the --seed-balance arg.
  const seedWire = curBalance && curBalance.seed != null ? num(curBalance.seed) : null;
  const seedArg = SEED_ARG != null ? num(SEED_ARG) : null;
  state.seedUsd = seedWire > 0 ? seedWire : (seedArg > 0 ? seedArg : null);

  // Audit digest: only records newer than the high-water mark (and not in the
  // last fetch's id set — tail windows can overlap).
  const records = Array.isArray(audit.result?.records) ? audit.result.records : [];
  const fresh = records
    .map((r) => auditDigest(r, state.lastAuditTs, state.seenIntentIds))
    .filter(Boolean);
  const byStatus = {};
  const topRejects = [];
  for (const f of fresh) {
    byStatus[f.status] = (byStatus[f.status] ?? 0) + 1;
    for (const rj of f.rejects) topRejects.push(rj);
  }
  if (fresh.length > 0) {
    state.lastAuditTs = Math.max(state.lastAuditTs, ...fresh.map((f) => f.ts));
    state.seenIntentIds = new Set(
      records.map((r) => String(r?.intentId ?? '')).filter(Boolean),
    );
  }

  const orderRows = Array.isArray(orders.result?.orders) ? orders.result.orders : [];
  const liveOrders = orderRows.filter((o) =>
    ['LIVE', 'PENDING', 'PARTIALLY_FILLED'].includes(String(o?.status ?? ''))).length;

  const evalDelta = cur && state.lastEvaluations != null
    ? num(cur.evaluations) - state.lastEvaluations : null;
  const sigDelta = cur && state.lastSignals != null
    ? num(cur.signals) - state.lastSignals : null;
  const roundsDelta = cur && state.lastRounds != null
    ? num(cur.rounds) - state.lastRounds : null;

  // Starvation streak — the PREREGISTERED definition, verbatim: evaluations
  // grew, engine orders 0. (`signals` counts orders, evaluate.rs:178; the
  // per-reason fills breakdown is another ticket, not this file's job.)
  if (cur && state.lastEvaluations != null) {
    if (evalDelta > 0 && (sigDelta ?? 0) === 0) state.deadStreak += 1;
    else if ((sigDelta ?? 0) > 0 || evalDelta === 0) state.deadStreak = 0;
  }
  if (cur) {
    state.lastEvaluations = num(cur.evaluations);
    state.lastSignals = num(cur.signals);
    state.lastRounds = num(cur.rounds);
  }

  // Kill lines: read-only verdicts → STDERR only. Nothing here issues a call.
  state.curSummary = curSummary;
  const warns = killLine(state, tsMs);
  for (const w of warns) console.error(w);

  const snap = {
    ts: tsMs,
    cycle: state.cycle,
    strategy: STRATEGY,
    socket: SOCKET,
    engine: cur ? {
      evaluations: cur.evaluations, signals: cur.signals, rounds: cur.rounds,
      books: cur.books, spots: cur.spots, resolutions: cur.resolutions,
      placeRejected: cur.placeRejected, venueRejected: cur.venueRejected,
      evalDelta, sigDelta, roundsDelta,
      tradingFrozen: cur.tradingFrozen ?? null,
      lastError: cur.lastError ?? null,
    } : { error: stats.error ?? 'no result' },
    summary: curSummary ?? null,
    balance: curBalance ? {
      balance: curBalance.balance, available: curBalance.available,
      seed: curBalance.seed ?? null, seedUsd: state.seedUsd,
    } : { error: balance.error ?? 'no result' },
    orders: orderRows.length,
    liveOrders,
    audit: {
      fresh: fresh.length, byStatus, topRejects: topRejects.slice(0, 5),
      tsMax: state.lastAuditTs, recentIds: [...state.seenIntentIds].slice(-50),
      error: audit.error ?? null,
    },
    deadStreak: state.deadStreak,
    warnings: warns,
  };
  ensureDir();
  appendFileSync(SNAPSHOTS, JSON.stringify(snap) + '\n');

  // Fold whatever closed weekly buckets are owed (incl. ones deferred while
  // the core was unreachable).
  await foldWeek(weekStartMs(tsMs) - WEEK_MS);
  refreshPendingWeekly();
  for (const ws of state.pendingWeekly.splice(0)) {
    if (!(await foldWeek(ws))) state.pendingWeekly.push(ws);
  }

  // Heartbeat: window progress in OBSERVED qualified rounds vs the
  // preregistered 300, plus the file-side window age.
  const snaps = readJsonl(SNAPSHOTS);
  const winSnaps = snaps.filter((s) => inWindow(s, tsMs - MIRROR_WINDOW_MS, tsMs));
  const windowRounds = winSnaps.reduce(
    (a, s) => a + Math.max(0, num(s?.engine?.roundsDelta)), 0,
  );
  const daysInWindow = winSnaps.length > 0
    ? (tsMs - Number(winSnaps[0].ts)) / DAY_MS : 0;
  console.log(
    `[evw #${String(state.cycle).padStart(4)}] eval+${evalDelta ?? '?'} sig+${sigDelta ?? '?'} ` +
    `ord=${orderRows.length}(live ${liveOrders}) ` +
    `netWin=${curSummary && state.baseSummary
      ? (num(curSummary.totalNetPnl) - num(state.baseSummary.totalNetPnl)).toFixed(4)
      : '?'} ` +
    `deadStreak=${state.deadStreak}/${DEAD_CYCLES} ` +
    `windowRounds=${windowRounds}/${WINDOW_MIN_ROUNDS} windowAge=${daysInWindow.toFixed(1)}d/${(MIRROR_WINDOW_MS / DAY_MS)}d ` +
    `warns=${warns.length}`,
  );
  state.cycle += 1;
}

// ── Self-test (pure functions + source assertions; no socket, no writes) ────
function runSelfTest() {
  let failed = 0;
  const check = (cond, msg) => {
    console.log(`${cond ? '  ok' : '  FAIL'}  ${msg}`);
    if (!cond) failed++;
  };

  // foldTradeSummary == trade_db fold_into, including decimal-string spellings.
  const folded = foldTradeSummary([
    { netPnlUsd: '1.5',  grossPnlUsd: '2.0', feesUsd: '0.5',  holdTimeSec: 10 },
    { netPnlUsd: '-0.5', grossPnlUsd: '0',   feesUsd: '0.5',  holdTimeSec: 30 },
    { netPnlUsd: 0,      grossPnlUsd: '0.1', feesUsd: '0.1',  holdTimeSec: 20 },
    { netPnlUsd: '2.25', grossPnlUsd: '3',   feesUsd: '0.75', holdTimeSec: 40 },
  ]);
  check(folded.totalTrades === 4, `fold totalTrades=4 (${folded.totalTrades})`);
  check(folded.wins === 3 && folded.losses === 1, `fold wins=3 losses=1 (${folded.wins}/${folded.losses}) — net>=0 is a win (fold_into)`);
  check(Math.abs(folded.winRate - 75) < 1e-9, `fold winRate=75 (${folded.winRate})`);
  check(Math.abs(folded.totalFees - 1.85) < 1e-9, `fold totalFees=1.85 (${folded.totalFees})`);
  check(Math.abs(folded.totalNetPnl - 3.25) < 1e-9, `fold totalNetPnl=3.25 (${folded.totalNetPnl})`);
  check(Math.abs(folded.bestTradePnl - 2.25) < 1e-9 && Math.abs(folded.worstTradePnl + 0.5) < 1e-9, 'fold best=2.25 worst=-0.5');
  check(Math.abs(folded.avgHoldTimeSec - 25) < 1e-9, `fold avgHoldTimeSec=25 (${folded.avgHoldTimeSec})`);
  const empty = foldTradeSummary([]);
  check(empty.totalTrades === 0 && empty.winRate === 0 && Number.isFinite(empty.avgHoldTimeSec), 'empty fold zeroed, never NaN');

  // The summary keys are EXACTLY TradeSummary's (trade_db.rs:135 camelCase):
  // the weekly `summary` object is contract-aligned, not reinvented.
  const want = ['totalTrades', 'wins', 'losses', 'winRate', 'totalGrossPnl', 'totalFees',
    'totalNetPnl', 'avgHoldTimeSec', 'bestTradePnl', 'worstTradePnl', 'lastUpdated'];
  const got = Object.keys(foldTradeSummary([{ netPnlUsd: '1', holdTimeSec: 1 }]));
  check(got.length === want.length && want.every((k) => got.includes(k)),
    `summary keys == TradeSummary (trade_db.rs:135): ${got.join(',')}`);

  // Weekly bucket boundary: UTC-Monday 00:00.
  const mon = Date.UTC(2026, 9, 19);           // Mon 2026-10-19 00:00Z
  const wed = Date.UTC(2026, 9, 21, 15, 0, 0); // Wed 2026-10-21 15:00Z
  const sun = Date.UTC(2026, 9, 25, 23, 59);   // Sun 2026-10-25 23:59Z
  check(weekStartMs(wed) === mon && weekStartMs(mon) === mon && weekStartMs(sun) === mon,
    'weekStartMs: Wed/Sun/its-Monday all fold to UTC-Monday 00:00');

  // Kill lines: pure string producers.
  const st = { curSummary: { totalNetPnl: '-18' }, baseSummary: { totalNetPnl: '0' },
    seedUsd: 1000, netLossBreached: false, netLossSeedUnknown: false,
    deadStreak: 0, deadBreached: false };
  check(killLine(st, 0).length === 0, 'netWindow -18 vs line -200 (seed 1000): inside the line → no warning');
  const st2 = { ...st, curSummary: { totalNetPnl: '-201' } };
  const w2 = killLine(st2, 0);
  check(w2.length === 1 && /net-loss/.test(w2[0]) && /READ-ONLY/.test(w2[0]) && /OPERATOR/.test(w2[0]),
    'netWindow -201 <= -200: ONE warning, naming READ-ONLY + OPERATOR');
  check(killLine({ ...st2, netLossBreached: true }, 0).length === 0, 'net-loss breach is edge-triggered (second cycle silent)');
  const st4 = { ...st, seedUsd: null };
  check(/cannot evaluate/.test(killLine(st4, 0)[0] ?? ''), 'unknown seed → loud warning, never silently armed');
  const st5 = { ...st, curSummary: null, baseSummary: null, deadStreak: DEFAULT_DEAD_CYCLES };
  const w5 = killLine(st5, 0);
  check(w5.length === 1 && /starvation/.test(w5[0]) && /coarse first cut/.test(w5[0])
    && /another ticket/.test(w5[0]),
    `${DEFAULT_DEAD_CYCLES}-cycle starvation: fires, names the coarseness (evaluate.rs:178) and the other ticket`);
  check(killLine({ ...st5, deadStreak: DEFAULT_DEAD_CYCLES - 1 }, 0).length === 0,
    'starvation below the line is silent');

  // Attribution edges.
  check(inWeek({ exitTime: mon }, mon, mon + WEEK_MS) === true, 'exitTime at week start is inside');
  check(inWeek({ exitTime: mon + WEEK_MS }, mon, mon + WEEK_MS) === false, 'exitTime at next week start is outside');
  check(inWeek({ exitTime: 'nope' }, mon, mon + WEEK_MS) === false, 'non-numeric exitTime is outside (never NaN-true)');
  check(inWindow({ ts: mon }, mon, mon + WEEK_MS) && !inWindow({ ts: mon - 1 }, mon, mon + WEEK_MS), 'window edges: inclusive start, exclusive end');

  // Decimal/number/string coercion.
  check(num('0.25') === 0.25 && num(2) === 2 && num(undefined) === 0 && num('x') === 0
    && num(Infinity) === 0, 'num(): decimal string / number / junk / Infinity');

  // The audit digest respects the high-water mark and dedupes by intent id.
  check(auditDigest({ tsMs: mon + 1, intentId: 'i2', decision: { status: 'REJECTED' }, gates: [] },
    mon, new Set()) !== null, 'auditDigest: record above the mark is fresh');
  check(auditDigest({ tsMs: mon + 1, intentId: 'i2', decision: { status: 'REJECTED' }, gates: [] },
    mon, new Set(['i2'])) === null, 'auditDigest: already-seen intent id is skipped');
  const withReject = auditDigest(
    { tsMs: mon + 2, decision: { status: 'REJECTED' },
      gates: [{ gate: 'postonly', outcome: 'REJECT', detail: 'WouldCross' }] },
    mon, new Set(),
  );
  check(withReject?.rejects?.[0] === 'postonly:WouldCross', 'auditDigest: REJECT gates carry gate+detail verbatim');

  // READ-ONLY by construction: the WHOLE set of verbs this script can issue
  // is the whitelist below — every call flows through the single `rpc()`
  // call site, so anything not on the list cannot be sent. The forbidden
  // write verbs (risk.kill/resume, account.freeze/status, orders.place/
  // cancel, positions.exit, risk.set*) are absent by virtue of the whitelist
  // being closed; the full-file string scan of a header that TALKS about
  // those verbs can only produce false positives.
  const src = readFileSync(fileURLToPath(import.meta.url), 'utf8');
  const callSites = [...src.matchAll(/rpc\(\s*(['"])((?:[^'"\\]|\\.)*)\1/g)]
    // The rpc() HELPER definition itself is not a verb it sends.
    .filter((m) => m[2] !== '')
    .map((m) => m[2]);
  const allowed = ['engine.stats', 'trades.summary', 'ledger.balance',
    'orders.list', 'intent.audit.tail', 'trades.history'];
  const notAllowed = callSites.filter((v) => !allowed.includes(v));
  check(callSites.length > 0 && notAllowed.length === 0 && new Set(callSites).size === callSites.length,
    `RPC verbs are exactly the read-only whitelist${notAllowed.length ? ` — OUT OF LIST: ${[...new Set(notAllowed)]}` : ``}`);

  console.log(failed === 0 ? 'SELF-TEST: PASS' : `SELF-TEST: FAIL (${failed})`);
  return failed === 0 ? 0 : 1;
}

// ── Main ─────────────────────────────────────────────────────────────────────
async function main() {
  if (SELF_TEST) process.exit(runSelfTest());
  ensureDir();
  initStateFromFile();
  refreshPendingWeekly();
  console.log(`evidence-window: strategy=${STRATEGY} interval=${INTERVAL_SEC}s ` +
    (FOREVER ? 'resident(--forever)' : `bounded-${HOURS}h`) +
    ` dead-cycles=${DEAD_CYCLES} window=${WINDOW_DAYS}d`);
  console.log(`socket=${SOCKET}`);
  console.log(`snapshots=${SNAPSHOTS}`);
  console.log(`weekly=${WEEKLY} (fold on UTC-Monday rollover; summary = trade_db.rs TradeSummary)`);
  console.log('kill lines: READ-ONLY — warnings go to stderr; freezing/killing stays with the operator (issue #854).');
  if (!existsSync(SOCKET)) {
    console.error(`note: socket ${SOCKET} not reachable yet — cycles will record {error} samples until the core appears.`);
  }

  // The loop is AWAITED, not timer-driven: an unref'd setInterval with
  // nothing else holding the event loop lets the process exit right after
  // the banner — a recorder that records nothing. Same await-loop pattern
  // as soak-monitor.mjs (its D-30 reasoning applies verbatim: TERM must be
  // honoured in 1s slices, and "stopped on purpose" must be visible).
  const startedAt = Date.now();
  let stopping = false;
  let stopSignal = null;
  const onStop = (sig) => { stopping = true; stopSignal = sig; };
  process.on('SIGTERM', () => onStop('SIGTERM'));
  process.on('SIGINT', () => onStop('SIGINT'));

  for (let i = 1; !stopping; i++) {
    try { await cycle(); } catch (e) {
      console.error(`cycle ${i} error: ${e?.message || e}`);
    }
    if (FOREVER) {
      if (stopping) break;
    } else if (Date.now() - startedAt >= HOURS * 3600 * 1000) {
      break;
    }
    let slept = 0;
    while (slept < INTERVAL_SEC && !stopping) {
      await new Promise((r) => setTimeout(r, 1000));
      slept += 1;
    }
  }
  const how = stopping ? `signal (${stopSignal})` : 'duration reached';
  console.log(`stopping (${how}); snapshots=${SNAPSHOTS} weekly=${WEEKLY}`);
  process.exit(0);
}

main().catch((e) => {
  console.error('evidence-window failed:', e?.stack || e);
  process.exit(1);
});
