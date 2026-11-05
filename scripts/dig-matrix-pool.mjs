#!/usr/bin/env node
/**
 * dig-matrix-pool — archive the matrix mining pool (issues #914/#915 数据面).
 *
 * Reads arms-summary data produced outside the repo (the #914 63-arm rescan
 * in the operator's scratch tree) and re-emits it as the candidate pool in
 * the same field semantics as `scripts/cost-adjusted-edge.mjs` `--dig`
 * (issue 889: n / evMicro / wrWilsonLb / PF). It understands three input
 * shapes and fails closed when the input does not exist — it never invents
 * a row when data is missing.
 *
 *   arms-summary.json   a JSON array of arm records
 *                       {arm, closed, wins, losses, net, pf, feeSchedule,
 *                       slipTicks, ...} — one row per matrix arm
 *   out.json            a single core --backtest-report
 *                       (fields under `trades`, coordinate self-attestation
 *                       in `fillModel` / `feeSchedule`); the arm id is
 *                       taken from the parent directory name
 *   a run directory     consolidates the out.json of every shard under
 *                       <dir>/run (shard-name/out.json), arm id = shard
 *                       name; a shard whose out.json is missing is
 *                       reported as absent and skipped, never fabricated
 *
 * --dig semantics (2026-11-02 ruling 挖掘宽、采用严): this output is MINING
 * ONLY. Each row records the coordinate it was measured in (its `mode`,
 * self-attested from the report's own fill-model / fee-schedule fields) and
 * NO GO/NO-GO verdict exists on this surface — adoption verdicts belong to
 * the #854 gate and the operator alone. The pool is sorted by EV descending.
 *
 * Usage:
 *   node scripts/dig-matrix-pool.mjs --input <arms-summary.json | out.json | run-dir>
 *   node scripts/dig-matrix-pool.mjs --input ... --write docs/reports/<name>.json
 *
 * `--write` only accepts paths inside <repo>/docs/reports/ (stdout stays the
 * primary surface; `--write` is the archive blade).
 */
import { createHash } from 'crypto';
import { mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from 'fs';
import { basename, dirname, join, relative, resolve } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const REPORTS = join(ROOT, 'docs', 'reports');
const MICRO = 1_000_000n;
const DIG_Z = 1.959963984540054; // two-sided 95% normal / Wilson deviate, same as digArm
const OUT_FILE = 'out.json';

// --- argv -------------------------------------------------------------------
const argv = process.argv.slice(2);
const inputIdx = argv.indexOf('--input');
const writeIdx = argv.indexOf('--write');
if (inputIdx === -1 || !argv[inputIdx + 1]) {
  console.error('dig-matrix-pool: --input <arms-summary.json | out.json | run-dir> is required');
  process.exit(1);
}
if (writeIdx !== -1 && !argv[writeIdx + 1]) {
  console.error('dig-matrix-pool: --write needs a path under docs/reports/');
  process.exit(1);
}
const INPUT_PATH = resolve(argv[inputIdx + 1]);

// --- helpers (field semantics aligned with cost-adjusted-edge.mjs digArm) ---
/** Fixed-point USD (or USD float) → integer micro-USD, half-even via toFixed(6). */
function toMicro(usd) {
  if (usd === null || usd === undefined) return null;
  const s = Number(usd).toFixed(6); // NaN → "NaN", caught below
  if (s === 'NaN' || !/-?\d+\.\d{6}/.test(s)) return null;
  const neg = s.startsWith('-');
  const t = neg ? s.slice(1) : s;
  const [w, f] = t.split('.');
  let v = BigInt(w) * MICRO + BigInt(f);
  return neg ? -v : v;
}
/** Integer mean per-round pnl: divRound(netMicro, n), same half-up shape as the repo's divRound. */
function divRound(num, den) {
  return (num + den / 2n) / den;
}
/** Wilson lower bound on the win rate; null unless n >= 2 (same guard as digArm). */
function wilsonLb(wins, n) {
  if (!Number.isInteger(wins) || wins < 0) return null;
  if (!Number.isInteger(n) || n < 2) return null;
  const wr = wins / n;
  const denom = 1 + (DIG_Z * DIG_Z) / n;
  const centre = wr + (DIG_Z * DIG_Z) / (2 * n);
  const rad = DIG_Z * Math.sqrt((wr * (1 - wr)) / n + (DIG_Z * DIG_Z) / (4 * n * n));
  return Math.max(0, (centre - rad) / denom);
}
/** PF field: finite number → as-is, ±Infinity → 'inf', anything else → 'na'. */
function pfField(pf) {
  if (typeof pf === 'number') return Number.isFinite(pf) ? pf : 'inf';
  return typeof pf === 'string' ? 'na' : 'na';
}
/** Coordinate label built from the record's own self-attestation. */
function modeOf(attested) {
  const fill = attested.identity
    ? 'identity@0tick/0ms'
    : `slip${attested.slipTicks ?? '?'}tick@${attested.latency ?? '?'}ms`;
  return `mine:${fill}+${attested.feeSchedule ?? 'unknown-fee'}`;
}
/** Pool row in the exact dig-field semantics. */
function row(arm, nClosed, wins, netUsd, pfRaw, attested, corpus) {
  const netMicro = toMicro(netUsd);
  const n = Number.isInteger(nClosed) && nClosed >= 0 ? nClosed : 0;
  return {
    arm,
    mode: modeOf(attested),
    evMicro: netMicro === null || n < 1 ? null : divRound(netMicro, BigInt(n)),
    n,
    wilsonLb: wilsonLb(wins, n),
    pf: pfField(pfRaw),
    corpus,
  };
}

// --- input shapers -----------------------------------------------------------
/** Arms-summary record → row (the record self-attests its coordinate). */
function fromSummaryRecord(r) {
  return row(
    r.arm,
    r.closed,
    r.wins,
    r.net,
    r.pf,
    { identity: r.slipTicks === 0, slipTicks: r.slipTicks, feeSchedule: r.feeSchedule },
    'arms-summary.json (per-arm shards under run/)'
  );
}
/** Core out.json report → row (arm id = parent dir name for run/ shards). */
function fromOutJson(buf, armFallback) {
  let data;
  try {
    data = JSON.parse(buf.toString('utf8'));
  } catch {
    return null;
  }
  if (!data || typeof data !== 'object' || !data.trades) return null;
  const t = data.trades;
  const fm = data.fillModel || {};
  const identity =
    fm.takerSlippageTicks === 0 &&
    fm.takerLatencyMs === 0 &&
    fm.takerRttMs === 0 &&
    (fm.makerLatencyMs === undefined || fm.makerLatencyMs === 0) &&
    (fm.makerFillProbBps === undefined || fm.makerFillProbBps === 10000);
  const attested = {
    identity,
    slipTicks: fm.takerSlippageTicks,
    latency: fm.takerLatencyMs,
    feeSchedule: data.feeSchedule && data.feeSchedule.name,
  };
  const corpus = typeof data.source === 'string' ? data.source : 'unspecified';
  return row(armFallback, t.closed, t.wins, t.netPnlUsd, t.profitFactor, attested, corpus);
}

// --- main --------------------------------------------------------------------
function statPath(p) {
  try {
    return statSync(p);
  } catch (e) {
    console.error(`dig-matrix-pool: cannot read input ${p} (${e.code || e.name}) — failing closed, no data is synthesized`);
    process.exit(1);
  }
}

const st = statPath(INPUT_PATH);
let pool;
let inputSchema;
let sourceSha;
if (st.isDirectory()) {
  inputSchema = 'run-directory';
  const runDir = join(INPUT_PATH, 'run');
  let children;
  try {
    children = readdirSync(runDir, { withFileTypes: true });
  } catch (e) {
    console.error(`dig-matrix-pool: input directory has no run/ of shard dirs (${e.code || e.name})`);
    process.exit(1);
  }
  const shards = children.filter((c) => c.isDirectory()).map((c) => c.name).sort();
  const rows = [];
  for (const shard of shards) {
    const outPath = join(runDir, shard, OUT_FILE);
    let buf;
    try {
      buf = readFileSync(outPath);
    } catch (e) {
      console.error(`dig-matrix-pool: ${shard}/${OUT_FILE} unreadable (${e.code || e.name}) — recorded as absent, not fabricated`);
      continue;
    }
    const r = fromOutJson(buf, shard);
    if (r === null) {
      console.error(`dig-matrix-pool: ${shard}/${OUT_FILE} parses but carries no trades object — recorded as absent`);
      continue;
    }
    rows.push(r);
  }
  if (!rows.length) {
    console.error(`dig-matrix-pool: zero usable shards under ${runDir}`);
    process.exit(1);
  }
  const digest = createHash('sha256');
  for (const r of rows) digest.update(`${r.arm}\t${r.n}\t${String(r.evMicro)}\t${String(r.pf)}\n`);
  pool = rows;
  sourceSha = `${digest.digest('hex')}:rows-fingerprint-over-${shards.length}-shards`;
} else {
  const buf = readFileSync(INPUT_PATH);
  sourceSha = createHash('sha256').update(buf).digest('hex');
  let parsed;
  try {
    parsed = JSON.parse(buf.toString('utf8'));
  } catch {
    console.error(`dig-matrix-pool: ${INPUT_PATH} is not valid JSON`);
    process.exit(1);
  }
  if (Array.isArray(parsed)) {
    inputSchema = 'arms-summary';
    pool = parsed.filter((r) => r && typeof r.arm === 'string').map(fromSummaryRecord);
  } else if (parsed && parsed.trades) {
    inputSchema = 'core-out.json';
    const r = fromOutJson(buf, basename(dirname(INPUT_PATH)));
    pool = r === null ? [] : [r];
  } else {
    console.error('dig-matrix-pool: unrecognized input shape (need an arms-summary array, an out.json report, or a run directory)');
    process.exit(1);
  }
  if (!pool.length) {
    console.error('dig-matrix-pool: input parsed but yielded zero pool rows');
    process.exit(1);
  }
}

pool.sort((a, b) => {
  const av = a.evMicro === null ? -Infinity : Number(a.evMicro);
  const bv = b.evMicro === null ? -Infinity : Number(b.evMicro);
  if (av !== bv) return bv - av;
  return a.arm.localeCompare(b.arm);
});

/** BigInt evMicro serializes as a bare integer literal (the dig string
 * convention stays in cost-adjusted-edge's own report; JSON has no BigInt). */
const replacer = (_k, v) => (typeof v === 'bigint' ? v.toString() : v);

const out = {
  semantics: 'mining',
  note: '挖掘档候选池读数（#914/#915 数据面）：按 EV 降序，无 GO/NO-GO 判决——采用判决只属于 #854 门与运营者',
  modeSemantics: 'mode = 逐臂自证的成交/费率坐标（identity = 0 tick 滑点 0 延迟 touch 即成交 fill prob 100%）',
  pool,
  generatedAtMs: Date.now(),
  source: INPUT_PATH,
  sourceSha,
  inputSchema,
};

const json = `${JSON.stringify(out, replacer, 2)}\n`;
process.stdout.write(json);

if (writeIdx !== -1) {
  const writePath = resolve(argv[writeIdx + 1]);
  const rel = relative(REPORTS, writePath);
  if (rel.startsWith('..') || rel === '' || resolve(writePath) !== writePath) {
    console.error('dig-matrix-pool: --write path must resolve inside docs/reports/');
    process.exit(1);
  }
  mkdirSync(dirname(writePath), { recursive: true });
  writeFileSync(writePath, json);
  console.error(`dig-matrix-pool: archived ${pool.length} mining rows -> ${rel}`);
}
