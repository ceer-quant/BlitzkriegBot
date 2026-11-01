#!/usr/bin/env node
/**
 * pair-fill replay — the fill-path matrix for pair_discount_arb v1.3.
 *
 * Why this exists: v1.2 quoted each leg AT its own ask with `entry_maker`, and
 * the kernel refuses a post-only BUY whose limit reaches the live ask at POST
 * (#464, `venue.WouldCross`) — the 10/15 replay died 4 orders / 4 rejections /
 * 0 fills without ever resting. v1.3 quotes UNDER the ask (`entry_quote_mode`
 * 1 = ask − ticks, 2 = mid floored to the grid, 3 = the bid; 0 = the v1.2
 * baseline kept as a replay arm). This script measures the matrix on real
 * archive windows: does the pair discount still OPEN, do the legs now REST and
 * FILL, and at what PF / net PnL — every number from a `--backtest-report`
 * JSON, never from this script's arithmetic.
 *
 * Two modes:
 *   --scan      stream the archive segments overlapping [--from --hours] (or
 *               each --window) and report where the fresh-book pair ask-sum
 *               dips under `--scan-max-sum` (default 0.985): the windows the
 *               strategy can actually fire in. Round lines carry the
 *               condition → token map; book rows carry the asks.
 *   (default)   run the arms below over each window, one staged package copy
 *               per arm (manifest defaults rewritten — the mad-dog shape),
 *               `--resolve-from <activity.jsonl>` injecting the real REDEEM
 *               verdicts so merges settle as $1.00 facts.
 *
 * Arms (the whole experimental design; each is a pure manifest-default diff):
 *   pair-baseline    entry_quote_mode=0 — v1.2 at-ask, expected 0 fills
 *                    (WouldCross), the control.
 *   pair-ask-1t      entry_quote_mode=1, entry_reprice_ticks=1 — the v1.3
 *                    default: the most aggressive RESTABLE quote.
 *   pair-mid         entry_quote_mode=2 — half a spread deep.
 *   pair-bid         entry_quote_mode=3 — the almach path, deepest.
 *   pair-ask1t-999   mode 1 + max_pair_cost 0.999 — the widening contrast arm.
 *
 * Usage:
 *   node scripts/pair-fill-replay.mjs --scan \
 *     --archive '/Volumes/Hard Disk/BlitzkriegBot/data/archive' --from 2026-10-29T00:00:00Z --hours 4
 *   node scripts/pair-fill-replay.mjs \
 *     --archive '/Volumes/Hard Disk/BlitzkriegBot/data/archive' \
 *     --window w1015=2026-10-15T17:00:00Z --window w1029=2026-10-29T00:30:00Z --hours 2 \
 *     --resolve-from '/Volumes/Hard Disk/BlitzkriegBot/data/almach/activity-full.jsonl'
 *
 * Outputs under $BK_SMR_OUT (default /Volumes/Hard Disk/bk-session/smr-round/
 * backtest-pair-fill-<stamp>): plan.json, results.json, one core report per
 * arm × window. Scratch (staged packages, corpus slices) under the worktree's
 * target/pair-fill-replay — never /tmp.
 */

import { createHash } from 'crypto';
import { createReadStream, createWriteStream, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'fs';
import { createInterface } from 'readline';
import { basename, dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';
import { spawn } from './lib/child-guard.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..');
const BIN = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const SCRATCH = process.env.BK_PAIR_FILL_SCRATCH || join(ROOT, 'target', 'pair-fill-replay');
const DEFAULT_OUT =
  process.env.BK_SMR_OUT || join('/Volumes/Hard Disk/bk-session/smr-round', 'backtest-pair-fill-' + stamp());

function stamp() {
  return new Date().toISOString().replace(/[-:]/g, '').replace(/\..*/, '').replace('T', '-');
}

// ── the arms (each a manifest-default diff over the worktree package) ────────

const PKG = { name: 'pair_discount_arb', dir: join(ROOT, 'user_layer', 'strategies_lua', 'pair_discount_arb') };

const ARMS = [
  { name: 'pair-baseline', params: { entry_quote_mode: '0' } },
  { name: 'pair-ask-1t', params: { entry_quote_mode: '1', entry_reprice_ticks: '1' } },
  { name: 'pair-mid', params: { entry_quote_mode: '2' } },
  { name: 'pair-bid', params: { entry_quote_mode: '3' } },
  { name: 'pair-ask1t-999', params: { entry_quote_mode: '1', entry_reprice_ticks: '1', max_pair_cost: '0.999' } },
];

// ── args ─────────────────────────────────────────────────────────────────────

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const has = (name) => args.includes(name);

const scanMode = has('--scan');
const archiveDir = flag('--archive');
const fromArg = flag('--from');
const hoursArg = Number(flag('--hours') ?? 2);
const scanMaxSum = flag('--scan-max-sum') ?? '0.985';
const scanFreshMs = Number(flag('--scan-fresh-ms') ?? 60_000);
const feeModel = flag('--fee-model') ?? 'official';
const makerRebate = !has('--no-maker-rebate');
const tickMs = flag('--backtest-tick-ms') ?? '50';
const timeoutMin = Number(flag('--timeout-min') ?? 30);
const OUT_DIR = resolve(flag('--out') ?? DEFAULT_OUT);
const keepCorpus = has('--keep-corpus');
const onlyWindow = flag('--only');
const onlyArm = flag('--only-arm');
const resolveFrom = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--resolve-from') resolveFrom.push(resolve(args[i + 1]));
}
const customWindows = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--window') {
    const [name, at] = args[i + 1].split('=');
    customWindows.push({ name, at });
    i++;
  }
}
if (!Number.isFinite(hoursArg) || hoursArg <= 0 || hoursArg > 24) {
  console.error('--hours must be a number in (0, 24]');
  process.exit(2);
}

const LEAD_MS = 15 * 60_000; // the archive lead the kernel's discovery needs (smart-money-reverse-replay's value)

const windows = []; // [{ name, at (ISO), spanMs }]
if (archiveDir && (fromArg || customWindows.length)) {
  const requested = customWindows.length
    ? customWindows
    : [{ name: `archive-${stampOf(fromArg)}-${hoursArg}h`, at: fromArg, spanMs: hoursArg * 3_600_000 }];
  for (const w of requested) {
    if (Number.isNaN(Date.parse(w.at))) {
      console.error(`window ${w.name}: cannot parse time ${w.at}`);
      process.exit(2);
    }
    windows.push({ ...w, spanMs: (w.spanMs ?? hoursArg * 3_600_000) });
  }
} else {
  console.error('need --archive <dir> with --from <ISO> --hours N or repeatable --window name=AT');
  process.exit(2);
}
const runWindows = onlyWindow ? windows.filter((w) => w.name === onlyWindow) : windows;
if (!runWindows.length) {
  console.error(`--only ${onlyWindow}: no such window (have: ${windows.map((w) => w.name).join(', ')})`);
  process.exit(2);
}
const runArms = onlyArm ? ARMS.filter((a) => a.name === onlyArm) : ARMS;
if (!runArms.length) {
  console.error(`--only-arm ${onlyArm}: no such arm (have: ${ARMS.map((a) => a.name).join(', ')})`);
  process.exit(2);
}

function stampOf(iso) {
  return iso.replace(/[-:]/g, '').replace(/\.\d+Z$/, 'Z').replace('T', 'T').slice(0, 16);
}

// ── segments (the overlap rule of smart-money-reverse-replay.mjs) ────────────

function listArchiveSegments(dir) {
  if (!existsSync(dir)) {
    console.error(`--archive ${dir} does not exist`);
    process.exit(2);
  }
  const files = readdirSync(dir)
    .filter((f) => /^events\.\d{8}T\d{6}Z(-\d{4})?\.jsonl$/.test(f))
    .sort()
    .map((f) => {
      const m = /^events\.(\d{8}T\d{6}Z)(?:-(\d{4}))?\.jsonl$/.exec(f);
      const ms = Date.parse(`${m[1].slice(0, 4)}-${m[1].slice(4, 6)}-${m[1].slice(6, 8)}T${m[1].slice(9, 11)}:${m[1].slice(11, 13)}:${m[1].slice(13, 15)}Z`);
      return { file: join(dir, f), endMs: ms, seq: m[2] ? Number(m[2]) : 0, stamp: m[1] };
    })
    .sort((a, b) => a.stamp.localeCompare(b.stamp) || a.seq - b.seq);
  files.forEach((s, i) => { s.startMs = i === 0 ? 0 : files[i - 1].endMs; });
  return files;
}

function overlappingSegments(t0, t1) {
  const lead0 = t0 - LEAD_MS;
  const segs = listArchiveSegments(archiveDir).filter((s) => s.startMs < t1 && s.endMs > lead0);
  if (!segs.length) {
    const all = listArchiveSegments(archiveDir);
    console.error(
      `window covers ${new Date(lead0).toISOString()} → ${new Date(t1).toISOString()} but overlaps no segment of ${archiveDir}` +
      ` (archive: ${new Date(all[0]?.endMs ?? 0).toISOString()} → ${new Date(all.at(-1)?.endMs ?? 0).toISOString()})`,
    );
    process.exit(2);
  }
  return segs;
}

/** sha256 of a file, streamed chunk-wise (a slice must not be read whole). */
function sha256File(path) {
  return new Promise((res, rej) => {
    const h = createHash('sha256');
    const s = createReadStream(path);
    s.on('data', (c) => h.update(c));
    s.on('end', () => res(h.digest('hex')));
    s.on('error', rej);
  });
}

// ── mode: --scan (where does the fresh-book pair ask-sum dip?) ───────────────

async function scanWindow(w) {
  const t0 = Date.parse(w.at);
  const t1 = t0 + w.spanMs;
  const maxSum = Number(scanMaxSum);
  console.log(`# scan ${w.name}: ${new Date(t0).toISOString()} → ${new Date(t1).toISOString()} (sum < ${maxSum})`);
  // condition → token pair; token → latest book (ask min over levels)
  const pairs = new Map(); // conditionId -> { up, down, negRisk }
  const books = new Map(); // token -> { atMs, bestAsk }
  const hits = []; // { atMs, cid, sum }
  let lines = 0, bookRows = 0;
  for (const seg of overlappingSegments(t0, t1)) {
    const rl = createInterface({ input: createReadStream(seg.file), crlfDelay: Infinity });
    for await (const line of rl) {
      if (!line || line.length < 20) continue;
      const at = Number(line.match(/"at":(\d+)/)?.[1]);
      if (!Number.isFinite(at) || at < t0 - LEAD_MS || at >= t1) continue;
      lines++;
      if (line.includes('"k":"round"')) {
        try {
          const row = JSON.parse(line);
          for (const mk of Array.isArray(row?.m) ? row.m : []) {
            if (mk.conditionId && mk.upTokenId && mk.downTokenId) {
              pairs.set(mk.conditionId, { up: mk.upTokenId, down: mk.downTokenId });
            }
          }
        } catch { /* a malformed round line must not kill the scan */ }
        continue;
      }
      if (!line.includes('"k":"book"')) continue;
      const atMs = at;
      // cheap pre-filter: only parse book rows whose token id matches a known
      // leg — quoted-exact, DOWN first (an up token id can be a strict prefix
      // of its own down id: "…-30" vs "…-30:no").
      let tok = null;
      for (const p of pairs.values()) {
        if (line.includes(`"${p.down}"`)) { tok = p.down; break; }
        if (line.includes(`"${p.up}"`)) { tok = p.up; break; }
      }
      if (!tok) continue;
      try {
        const row = JSON.parse(line);
        if (row.t !== tok) continue;
        let bestAsk = Infinity;
        for (const lv of row.a ?? []) {
          const p = Number(lv[0]);
          if (p > 0 && p < bestAsk) bestAsk = p;
        }
        if (!Number.isFinite(bestAsk)) continue;
        books.set(tok, { atMs, bestAsk });
        bookRows++;
        // every book touch, test every mapped pair whose both legs are fresh
        for (const [cid, p] of pairs) {
          const bu = books.get(p.up);
          const bd = books.get(p.down);
          if (!bu || !bd) continue;
          if (atMs - bu.atMs > scanFreshMs || atMs - bd.atMs > scanFreshMs) continue;
          const sum = bu.bestAsk + bd.bestAsk;
          if (sum < maxSum) hits.push({ atMs, cid, sum: Math.round(sum * 10000) / 10000 });
        }
      } catch { /* skip */ }
    }
    console.log(`  ${basename(seg.file)}: +${lines} lines (${bookRows} booked)`);
  }
  // group hits into episodes (same condition, gaps < 60s collapse)
  hits.sort((a, b) => a.atMs - b.atMs);
  const episodes = [];
  for (const h of hits) {
    const last = episodes[episodes.length - 1];
    if (last && last.cid === h.cid && h.atMs - last.endMs < 60_000) {
      last.endMs = h.atMs;
      last.n++;
      last.minSum = Math.min(last.minSum, h.sum);
    } else {
      episodes.push({ cid: h.cid, startMs: h.atMs, endMs: h.atMs, n: 1, minSum: h.sum });
    }
  }
  console.log(`# ${hits.length} fresh-book samples under ${maxSum} → ${episodes.length} episodes:`);
  for (const e of episodes) {
    console.log(`  ${new Date(e.startMs).toISOString()} − ${new Date(e.endMs).toISOString()} cid=${e.cid.slice(0, 14)}… n=${e.n} minSum=${e.minSum}`);
  }
  return { window: w.name, lines, bookRows, hits: hits.length, episodes };
}

// ── mode: replay matrix ──────────────────────────────────────────────────────

const OPS_KNOBS = [
  '--mode', 'dry', '--engine',
  '--no-discovery', '--no-strategy-state', '--no-intent-audit',
  '--no-trade-log', '--no-order-log', '--no-position-log', '--no-event-archive',
  '--round-sec', '900', '--min-round-age', '0', '--min-time-left', '0',
  '--seed-balance', '1000', '--max-order-notional', '12',
  '--fee-model', feeModel,
  ...(makerRebate ? ['--maker-rebate'] : []),
];

function parseSelfCheck(stderr) {
  const m = stderr.match(/strategy startup self-check: requested=(\d+) resolved=(\d+) enabled=\[([^\]]*)\]/);
  return m ? { requested: Number(m[1]), resolved: Number(m[2]), enabled: m[3].split(',').map((s) => s.trim()).filter(Boolean) } : null;
}

function invalidReason(stderr, strategyName, selfCheck) {
  if (/unknown strategy requested/.test(stderr)) return 'kernel log: unknown strategy requested (the library was not loaded)';
  if (/refusing to start: none of the \d+ explicitly requested/.test(stderr)) return 'kernel log: the kernel refused to start over unresolved strategies';
  if (!selfCheck) return 'kernel log: no strategy startup self-check line (binary predates #265?)';
  if (selfCheck.resolved < 1) return `kernel log: requested ${selfCheck.requested} strategies, resolved ${selfCheck.resolved}`;
  if (!selfCheck.enabled.includes(strategyName)) return `kernel log: resolved but not enabled: ${strategyName} missing from [${selfCheck.enabled.join(', ')}]`;
  return null;
}

/** Stage a throwaway package copy with ONLY manifest defaults rewritten. */
function stagePackage(arm) {
  const root = mkdtempSync(join(SCRATCH, 'pkg-'));
  const dst = join(root, PKG.name);
  mkdirSync(dst, { recursive: true });
  for (const f of ['strategy.lua', 'manifest.json', 'README.md']) {
    writeFileSync(join(dst, f), readFileSync(join(PKG.dir, f)));
  }
  if (Object.keys(arm.params).length) {
    const manifestPath = join(dst, 'manifest.json');
    const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
    for (const [k, v] of Object.entries(arm.params)) {
      if (!manifest.tunables?.[k]) {
        console.error(`arm ${arm.name}: unknown tunable ${k} (known: ${Object.keys(manifest.tunables ?? {}).join(', ')})`);
        process.exit(2);
      }
      manifest.tunables[k].default = v;
    }
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
  }
  return root;
}

async function sliceArchiveWindow(w) {
  const t0 = Date.parse(w.at);
  const t1 = t0 + w.spanMs;
  const rawPath = join(SCRATCH, 'corpus', `${w.name}.raw.jsonl`);
  const rounds = new Map(); // conditionId → { upTokenId, downTokenId, negRisk, expiresAtMs }
  let lines = 0;
  for (const seg of overlappingSegments(t0, t1)) {
    const rl = createInterface({ input: createReadStream(seg.file), crlfDelay: Infinity });
    const out = createWriteStream(rawPath, { flags: 'a' });
    const write = (line) => new Promise((res) => out.write(line + '\n', res));
    for await (const line of rl) {
      if (!line) continue;
      const at = Number(line.match(/"at":(\d+)/)?.[1]);
      if (!Number.isFinite(at) || at < t0 - LEAD_MS || at >= t1) continue;
      if (line.includes('"k":"round"')) {
        try {
          const m = JSON.parse(line)?.m;
          if (Array.isArray(m)) {
            for (const mk of m) {
              if (mk.conditionId && mk.upTokenId && mk.downTokenId) {
                rounds.set(mk.conditionId, {
                  upTokenId: mk.upTokenId, downTokenId: mk.downTokenId,
                  negRisk: Boolean(mk.negRisk), expiresAtMs: mk.expiresAtMs ?? null,
                });
              }
            }
          }
        } catch { /* a malformed round line must not kill the slice */ }
      }
      await write(line);
      lines++;
    }
    out.end();
    console.log(`    slice ${w.name}: ${basename(seg.file)} → +${lines} lines`);
  }
  return { rawPath, rounds, lines, t0, t1 };
}

/** REDEEM(usdcSize>0) rows inside the replayed span → conditionId → winner. */
async function redeemWinners(span) {
  const winners = new Map();
  for (const f of resolveFrom) {
    if (!existsSync(f)) {
      console.error(`--resolve-from ${f} does not exist`);
      process.exit(2);
    }
    const rl = createInterface({ input: createReadStream(f), crlfDelay: Infinity });
    for await (const line of rl) {
      if (!line.includes('"REDEEM"')) continue;
      let row;
      try { row = JSON.parse(line); } catch { continue; }
      if (row?.type !== 'REDEEM' || !(Number(row.usdcSize) > 0)) continue;
      const atMs = Number(row.timestamp) * 1000;
      if (atMs < span.t0 - LEAD_MS || atMs > span.t1) continue;
      if (!row.conditionId || !row.asset) continue;
      const seen = winners.get(row.conditionId);
      if (!seen) winners.set(row.conditionId, { token: row.asset, atMs, conflicts: 0 });
      else if (seen.token !== row.asset) seen.conflicts++;
    }
  }
  return winners;
}

async function injectResolutions(w, span, rounds, winners) {
  let injected = 0, missingRound = 0, beyondEnd = 0, inLead = 0;
  const pending = [];
  for (const [conditionId, win] of winners) {
    const mk = rounds.get(conditionId);
    if (!mk) { missingRound++; continue; }
    const loser = win.token === mk.upTokenId ? mk.downTokenId : mk.upTokenId;
    const at = mk.expiresAtMs ?? Math.max(win.atMs, span.t0);
    if (at < span.t0) { inLead++; continue; }
    if (at > span.t1 - 1) { beyondEnd++; continue; }
    pending.push({
      at, line: JSON.stringify({
        at, k: 'resolution', c: conditionId,
        p: [[win.token, '1'], [loser, '0']], n: mk.negRisk,
      }),
    });
  }
  pending.sort((a, b) => a.at - b.at);
  const path = join(SCRATCH, 'corpus', `${w.name}.jsonl`);
  const out = createWriteStream(path);
  const write = (s) => new Promise((res) => out.write(s, res));
  const rl = createInterface({ input: createReadStream(span.rawPath), crlfDelay: Infinity });
  let pi = 0;
  for await (const line of rl) {
    if (!line) continue;
    const at = Number(line.match(/"at":(\d+)/)?.[1]);
    while (pi < pending.length && Number.isFinite(at) && pending[pi].at < at) {
      await write(pending[pi].line + '\n');
      injected++; pi++;
    }
    await write(line + '\n');
  }
  while (pi < pending.length) { await write(pending[pi].line + '\n'); injected++; pi++; }
  await new Promise((res) => out.end(res));
  rmSync(span.rawPath, { force: true });
  return { injected, missingRound, beyondEnd, inLead, total: winners.size };
}

function runOne(arm, w, corpusPathForRun, reportPath) {
  const staged = stagePackage(arm);
  return new Promise((res) => {
    const argv = [
      '--backtest', corpusPathForRun,
      '--lua-strategy-dir', staged,
      '--enable-strategy', PKG.name,
      '--backtest-report', reportPath,
      '--backtest-tick-ms', tickMs, '--backtest-tail-ms', '0',
      '--backtest-fast',
      ...OPS_KNOBS,
    ];
    const child = spawn(BIN, argv, {
      cwd: OUT_DIR,
      stdio: ['ignore', 'ignore', 'pipe'],
      env: { ...process.env, BLITZKRIEG_STRATEGY_ALLOW_DIRS: staged },
    });
    let stderr = '';
    child.stderr.on('data', (d) => (stderr += d.toString()));
    let timedOut = false;
    const watchdog = setTimeout(() => {
      timedOut = true;
      console.error(`    TIMEOUT after ${timeoutMin} min — stopping the run group`);
      try { process.kill(-child.pid, 'SIGTERM'); } catch { try { child.kill('SIGTERM'); } catch {} }
    }, timeoutMin * 60_000);
    child.on('exit', (code) => {
      clearTimeout(watchdog);
      rmSync(staged, { recursive: true, force: true });
      if (timedOut) return res({ error: `timeout after ${timeoutMin} min` });
      if (code !== 0) {
        return res({ error: `exit ${code}: ${stderr.split('\n').filter(Boolean).slice(-3).join(' | ')}` });
      }
      if (!existsSync(reportPath)) return res({ error: 'no report written' });
      let report;
      try { report = JSON.parse(readFileSync(reportPath, 'utf8')); } catch (e) {
        return res({ error: `unparsable report: ${e.message}` });
      }
      const selfCheck = parseSelfCheck(stderr);
      const invalid = invalidReason(stderr, PKG.name, selfCheck);
      if (invalid) return res({ error: `INVALID (#265): ${invalid}` });
      res({ report });
    });
  });
}

// ── metric extraction (every number from the report JSON) ────────────────────

const num = (v) => (v == null ? 0 : Number(v));
const r2 = (n) => (Number.isFinite(n) ? Math.round(n * 100) / 100 : '—');
const r4 = (n) => (Number.isFinite(n) ? Math.round(n * 10000) / 10000 : '—');

function extractMetrics(rep) {
  const t = rep.trades ?? {};
  const stratRow = (rep.strategies ?? []).find((s) => typeof s?.name === 'string') ?? {};
  const makerFeeEquiv = num(rep.makerRebateUsd) / 0.2;
  const takerFees = num(t.feesUsd);
  const makerShare = makerFeeEquiv + takerFees > 0 ? makerFeeEquiv / (makerFeeEquiv + takerFees) : null;
  return {
    closed: num(t.closed), wins: num(t.wins), losses: num(t.losses),
    winRatePct: num(t.winRatePct),
    profitFactor: t.profitFactor == null ? null : num(t.profitFactor),
    grossProfitUsd: num(t.grossProfitUsd), grossLossUsd: num(t.grossLossUsd),
    netPnlUsd: num(t.netPnlUsd), feesUsd: takerFees,
    makerRebateUsd: num(rep.makerRebateUsd),
    makerShareEst: feeModel === 'official' ? makerShare : null,
    orders: rep.orders?.orders ?? 0, ordersFilled: rep.orders?.filled ?? 0,
    ordersRejected: rep.orders?.rejected ?? 0, ordersLiveAtEnd: rep.orders?.liveAtEnd ?? 0,
    fills: num(rep.fills), fillRatePct: num(rep.fillStats?.fillRatePct),
    placed: num(stratRow.ordersPlaced), rejected: num(stratRow.ordersRejected),
    rejectionCauses: stratRow.rejectionCauses ?? {},
    blockedTiming: num(rep.blocked?.timing), blockedMomentum: num(rep.blocked?.momentum),
    openPositions: rep.openPositions ?? 0,
    events: num(rep.sourceStats?.events),
    startAtMs: rep.startAtMs, endAtMs: rep.endAtMs,
    feeSchedule: rep.feeSchedule?.name,
  };
}

// ── run ──────────────────────────────────────────────────────────────────────

if (scanMode) {
  const out = [];
  for (const w of runWindows) out.push(await scanWindow(w));
  mkdirSync(OUT_DIR, { recursive: true });
  writeFileSync(join(OUT_DIR, `scan-${stamp()}.json`), JSON.stringify({ archiveDir, scanMaxSum, windows: runWindows, out }, null, 2) + '\n');
  process.exit(0);
}

// replay mode preflight
const missing = [];
if (!existsSync(BIN)) missing.push(`core binary ${BIN} (build: cargo build --release -p blitzkrieg-core)`);
if (!existsSync(PKG.dir)) missing.push(`strategy package ${PKG.dir}`);
if (missing.length) {
  console.error('preflight FAILED — refusing:');
  for (const m of missing) console.error(`  missing: ${m}`);
  process.exit(2);
}

mkdirSync(OUT_DIR, { recursive: true });
mkdirSync(join(SCRATCH, 'corpus'), { recursive: true });

console.log(`# pair-fill replay — arms=${runArms.map((a) => a.name).join(',')} windows=${runWindows.map((w) => w.name).join(',')}`);
console.log(`# fee-model=${feeModel} maker-rebate=${makerRebate} tick-ms=${tickMs}`);
console.log(`# out: ${OUT_DIR}`);

writeFileSync(join(OUT_DIR, 'plan.json'), JSON.stringify({
  generatedAtMs: Date.now(), arms: runArms, windows: runWindows, archiveDir,
  resolveFrom, feeModel, makerRebate, tickMs, opsKnobs: OPS_KNOBS,
}, null, 2) + '\n');

// Materialize each window's corpus ONCE — every arm must see the same bytes.
const corpusProvenance = {};
for (const w of runWindows) {
  const finalPath = join(SCRATCH, 'corpus', `${w.name}.jsonl`);
  if (existsSync(finalPath)) {
    console.log(`  corpus ${w.name}: (staged from an earlier run) ${(statSync(finalPath).size / 1e6).toFixed(1)} MB`);
  } else {
    console.log(`  corpus ${w.name}: slicing archive…`);
    const span = await sliceArchiveWindow(w);
    let inj = { injected: 0, missingRound: 0, beyondEnd: 0, inLead: 0, total: 0 };
    if (resolveFrom.length) {
      const winners = await redeemWinners(span);
      inj = await injectResolutions(w, span, span.rounds, winners);
      console.log(`    resolutions: ${inj.injected} injected of ${inj.total} redeem-verdicts` +
        ` (no-round-line ${inj.missingRound}, beyond-window ${inj.beyondEnd}, pre-window ${inj.inLead})`);
    } else {
      rmSync(finalPath, { force: true });
      const { renameSync } = await import('fs');
      renameSync(span.rawPath, finalPath);
      console.log(`    resolutions: none injected (--resolve-from absent) — unsettled positions ride the core-dry settlement ladder`);
    }
  }
  corpusProvenance[w.name] = {
    path: finalPath, sha256: await sha256File(finalPath), bytes: statSync(finalPath).size,
    source: `archive slice of ${archiveDir}` + (resolveFrom.length ? ' + REDEEM resolutions' : ''),
  };
  console.log(`    corpus ${w.name}: ${(statSync(finalPath).size / 1e6).toFixed(1)} MB sha ${corpusProvenance[w.name].sha256.slice(0, 12)}…`);
}

const results = [];
for (const arm of runArms) {
  const armDir = join(OUT_DIR, arm.name);
  mkdirSync(armDir, { recursive: true });
  console.log(`\n# arm ${arm.name}  params=${JSON.stringify(arm.params)}`);
  for (const w of runWindows) {
    const reportPath = join(armDir, `${w.name}.json`);
    if (existsSync(reportPath)) {
      try {
        const cached = JSON.parse(readFileSync(reportPath, 'utf8'));
        console.log(`  ${w.name}: (resume) closed=${cached.trades?.closed ?? 0}`);
        results.push({ arm: arm.name, params: arm.params, window: w.name, metrics: extractMetrics(cached) });
        continue;
      } catch { /* stale report — rerun */ }
    }
    const corpusPathForRun = join(SCRATCH, 'corpus', `${w.name}.jsonl`);
    if (!existsSync(corpusPathForRun)) {
      console.error(`  ${w.name}: MISSING corpus — refusing to fake a zero`);
      results.push({ arm: arm.name, params: arm.params, window: w.name, error: 'corpus missing' });
      continue;
    }
    const r = await runOne(arm, w, corpusPathForRun, reportPath);
    if (r.error) {
      console.log(`  ${w.name}: ERROR — ${r.error}`);
      results.push({ arm: arm.name, params: arm.params, window: w.name, error: r.error });
      continue;
    }
    const m = extractMetrics(r.report);
    results.push({ arm: arm.name, params: arm.params, window: w.name, metrics: m });
    console.log(`  ${w.name}: orders=${m.orders}(filled=${m.ordersFilled} rej=${m.ordersRejected}) fills=${m.fills} closed=${m.closed} WR=${r2(m.winRatePct)}% PF=${r4(m.profitFactor)} net=${r4(m.netPnlUsd)} fees=${r4(m.feesUsd)} rebate=${r4(m.makerRebateUsd)} open=${m.openPositions}`);
  }
}

// per-arm table
console.log('\n# per-arm table (sums over windows; PF from summed gross P/L)');
const header = ['arm', 'params', 'windows', 'orders(filled)', 'fills', 'closed', 'wins', 'WR%', 'PF', 'net USD', 'fees USD', 'rebate USD', 'maker:taker est', 'rejections', 'open@end'];
console.log(header.join('  '));
const summary = {};
for (const arm of runArms) {
  const rows = results.filter((r) => r.arm === arm.name);
  const ok = rows.filter((r) => !r.error);
  const s = {
    windows: rows.length, errors: rows.length - ok.length,
    orders: ok.reduce((a, r) => a + r.metrics.orders, 0),
    ordersFilled: ok.reduce((a, r) => a + r.metrics.ordersFilled, 0),
    fills: ok.reduce((a, r) => a + r.metrics.fills, 0),
    closed: ok.reduce((a, r) => a + r.metrics.closed, 0),
    wins: ok.reduce((a, r) => a + r.metrics.wins, 0),
    grossProfitUsd: ok.reduce((a, r) => a + r.metrics.grossProfitUsd, 0),
    grossLossUsd: ok.reduce((a, r) => a + r.metrics.grossLossUsd, 0),
    netPnlUsd: ok.reduce((a, r) => a + r.metrics.netPnlUsd, 0),
    feesUsd: ok.reduce((a, r) => a + r.metrics.feesUsd, 0),
    makerRebateUsd: ok.reduce((a, r) => a + r.metrics.makerRebateUsd, 0),
    rejected: ok.reduce((a, r) => a + r.metrics.ordersRejected, 0),
    openPositions: ok.reduce((a, r) => a + r.metrics.openPositions, 0),
    rejectionCauses: {},
  };
  for (const r of ok) for (const [k, v] of Object.entries(r.metrics.rejectionCauses ?? {})) s.rejectionCauses[k] = (s.rejectionCauses[k] ?? 0) + v;
  s.winRatePct = s.closed ? (100 * s.wins) / s.closed : 0;
  s.profitFactor = s.grossLossUsd > 0 ? s.grossProfitUsd / s.grossLossUsd : null;
  const makerFeeEquiv = s.makerRebateUsd / 0.2;
  s.makerTakerEst = feeModel === 'official' && makerFeeEquiv + s.feesUsd > 0
    ? `${Math.round((100 * makerFeeEquiv) / (makerFeeEquiv + s.feesUsd))}:${Math.round((100 * s.feesUsd) / (makerFeeEquiv + s.feesUsd))}`
    : 'n/a';
  summary[arm.name] = s;
  console.log([
    arm.name, JSON.stringify(arm.params), `${s.windows}(${s.errors} err)`,
    `${s.orders}(${s.ordersFilled})`, s.fills, s.closed, s.wins, r2(s.winRatePct),
    r4(s.profitFactor), r4(s.netPnlUsd), r4(s.feesUsd), r4(s.makerRebateUsd), s.makerTakerEst,
    JSON.stringify(s.rejectionCauses), s.openPositions,
  ].join('  '));
}

writeFileSync(join(OUT_DIR, 'results.json'), JSON.stringify({
  generatedAtMs: Date.now(), plan: 'plan.json', corpusProvenance, feeModel, makerRebate,
  summary, results,
}, null, 2) + '\n');

const errCount = results.filter((r) => r.error).length;
console.log(`\n# reports: ${OUT_DIR}/<arm>/<window>.json; results.json + plan.json alongside`);
if (!keepCorpus) {
  for (const w of runWindows) rmSync(join(SCRATCH, 'corpus', `${w.name}.jsonl`), { force: true });
}
if (errCount) {
  console.error(`\nINVALID/FAILED runs: ${errCount}. Their rows are marked ERROR and excluded from every sum — fix the cause before reading any zero as "no signal".`);
  process.exitCode = 1;
}
