#!/usr/bin/env node
/**
 * Walk-forward parameter sweep (E15 / #97), productized.
 *
 * Replays one frozen event archive through the production kernel
 * (`blitzkrieg-core --backtest`) once per (candidate × fold), then does the
 * walk-forward arithmetic on the reported metrics — the replay IS the engine,
 * the selection is pure post-processing, so every number in the report comes
 * from the same code path a live run would use.
 *
 * Protocol:
 *   1. The archive files (chronological, glob-expanded) are split into
 *      `--folds` contiguous, equal-EVENT-COUNT folds in one streaming pass.
 *      Equal count, not equal wall time: feed gaps would otherwise leave some
 *      folds nearly empty.
 *   2. Rolling walk-forward: fold i (0..N-2) is the selection window — pick
 *      the candidate with the best `--select` metric there; fold i+1 is that
 *      pick's validation window. Windows are disjoint, so a pick only
 *      "generalizes" if it wins again on data it never saw.
 *   3. Every candidate is replayed on EVERY fold, so the full fold×candidate
 *      matrix lands in the report and any selection rule can be re-run
 *      offline without another sweep.
 *
 * Resume-safe: a run whose report file already parses is skipped, so an
 * interrupted sweep continues where it stopped. The candidate grid is the
 * cartesian product of the repeatable `--param key=v1,v2` flags; each key
 * maps to exactly one CLI flag (PARAM_FLAGS below) — an unknown key is an
 * error, never a silent pass-through. Baseline discipline: pass the shipped
 * configuration in the grid (it is the reference every pick is judged
 * against); the sweep never edits strategy code or ships a winner itself.
 *
 * Example:
 *   node scripts/walk-forward-sweep.mjs \
 *     --archive 'data/archive/*.jsonl' --strategy my_leg --folds 5 \
 *     --param trend_entry_factor=0.98,0.88 \
 *     --out data/evolution/sweeps/20260920
 */

import { spawn } from './lib/child-guard.mjs';
import { join, dirname, resolve } from 'path';
import { fileURLToPath } from 'url';
import { createReadStream, createWriteStream, existsSync, mkdirSync, readFileSync, writeFileSync, statSync, readdirSync, unlinkSync, rmdirSync } from 'fs';
import readline from 'readline';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
const BIN = join(ROOT, 'target', 'release', 'blitzkrieg-core');

// Param key → CLI flag (the standard config chain: CoreConfig → engine_config;
// see main.rs "strategy knobs"). This table is the whole coupling surface.
const PARAM_FLAGS = {
  trend_entry_factor: '--spread-arb-entry-factor',
  entry_min_obi: '--spread-arb-min-obi',
  entry_max_spread_pct: '--spread-arb-max-spread-pct',
  entry_dip_max_pct: '--spread-arb-dip-max-pct',
  entry_bounce_min_pct: '--spread-arb-bounce-min-pct',
  entry_bounce_window_sec: '--spread-arb-bounce-window-sec',
  trend_confirm_sec: '--trend-confirm-sec',
};

// The strategies every replay asks for — and the set the kernel's startup
// self-check (#265) is validated against. ONE list, so the sweep cannot ask for
// a different number than it verifies: a replay whose kernel resolved fewer
// strategies than this ran with no trading logic at all, and its 0-trade report
// must never be read as "the strategy had no signal". That misreading is exactly
// what #265 cost once (a deployment binary started outside the repo refused all
// three libraries, printed three FAILED lines and one WARN, and then reported
// zero trades on every fold).
//
// It is an argument rather than a constant because the kernel registers no
// strategy of its own: the name has to be one the operator actually shipped.
// `--strategy <name>` (repeatable) is required, and an empty list is an error
// for the same reason #265 makes the refusal loud.
const STRATEGIES = [];
for (let i = 0; i < process.argv.length; i++) {
  if (process.argv[i] === '--strategy' && process.argv[i + 1] && !process.argv[i + 1].startsWith('--')) {
    STRATEGIES.push(process.argv[i + 1]);
  }
}

// Ops knobs shared by every run: the named strategies alone, no discovery, no
// logs, the same documented seed the other replay gates use.
// `--allow-zero-strategies` is deliberately NOT passed: the sweep wants the
// refusal to be loud.
const OPS_KNOBS = [
  '--engine',
  ...STRATEGIES.flatMap((s) => ['--enable-strategy', s]),
  '--no-discovery',
  '--no-trade-log',
  '--no-order-log',
  '--no-position-log',
  '--seed-balance', '1000',
  '--max-order-notional', '6',
];

// ── args ────────────────────────────────────────────────────────────────────
const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const archivesArg = flag('--archive');
const foldsN = Number(flag('--folds') ?? 4);
const selectMetric = flag('--select') ?? 'netPnlUsd';
const tickMs = flag('--backtest-tick-ms') ?? '50';
const tailMs = flag('--backtest-tail-ms') ?? '0';
const runTimeoutMin = Number(flag('--run-timeout-min') ?? 20);
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..*/, '').replace('T', '-') + 'Z';
const outDir = resolve(flag('--out') ?? join(ROOT, 'data', 'evolution', 'sweeps', stamp));
const keepFolds = args.includes('--keep-folds');
const dryRun = args.includes('--dry-run');

const paramArgs = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--param') paramArgs.push(args[i + 1]);
}
if (!archivesArg) {
  console.error('usage: --archive <glob|file>[,more…] --strategy <name> --param key=v1,v2 [--folds N] [--select netPnlUsd] [--out dir] [--dry-run]');
  process.exit(2);
}
if (STRATEGIES.length === 0) {
  console.error('--strategy <name> is required: the kernel registers no strategy of its own, so a sweep ' +
    'without one replays archives through a kernel with no trading logic and reports zero trades on every fold.');
  process.exit(2);
}
if (!Number.isInteger(foldsN) || foldsN < 1 || foldsN > 24) { console.error('--folds must be an integer in 1..24'); process.exit(2); }
const SELECTABLE = new Set(['netPnlUsd', 'winRatePct', 'profitFactor', 'payoff', 'closed']);
if (!SELECTABLE.has(selectMetric)) { console.error(`--select must be one of ${[...SELECTABLE].join('|')}`); process.exit(2); }

// Expand archive globs (shell-free: the pattern may arrive quoted).
const archivePatterns = archivesArg.split(',').map((s) => s.trim()).filter(Boolean);
const archiveFiles = [];
for (const pat of archivePatterns) {
  if (pat.includes('*')) {
    const dir = resolve(ROOT, dirname(pat));
    const base = pat.split('/').pop();
    const rx = new RegExp('^' + base.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*') + '$');
    const found = readdirSync(dir).filter((f) => rx.test(f)).sort().map((f) => join(dir, f));
    if (!found.length) { console.error(`no files match ${pat} under ${dir}`); process.exit(2); }
    archiveFiles.push(...found);
  } else {
    const p = resolve(ROOT, pat);
    if (!existsSync(p)) { console.error(`archive not found: ${p}`); process.exit(2); }
    archiveFiles.push(p);
  }
}
if (!archiveFiles.length) { console.error('no archive files'); process.exit(2); }

// Candidate grid: cartesian product of the --param lists.
const grid = [];
{
  const dims = paramArgs.map((s) => {
    const eq = s.indexOf('=');
    if (eq < 0) { console.error(`--param expects key=v1,v2: ${s}`); process.exit(2); }
    const key = s.slice(0, eq).trim();
    if (!PARAM_FLAGS[key]) {
      console.error(`unknown param key ${key} (known: ${Object.keys(PARAM_FLAGS).join(', ')})`);
      process.exit(2);
    }
    const values = s.slice(eq + 1).split(',').map((v) => v.trim()).filter(Boolean);
    if (!values.length) { console.error(`--param ${key} has no values`); process.exit(2); }
    return { key, values };
  });
  let combos = [{}];
  for (const d of dims) {
    const next = [];
    for (const c of combos) for (const v of d.values) next.push({ ...c, [d.key]: v });
    combos = next;
  }
  grid.push(...combos);
}
if (!grid.length) { console.error('no candidates — pass at least one --param'); process.exit(2); }

const candidateId = (combo) =>
  Object.entries(combo).map(([k, v]) => `${k}=${v}`).sort().join('|') || 'shipped-defaults';
const candidateDir = (id) => id.replace(/[^A-Za-z0-9.=-]/g, '_').slice(0, 120);
const utc = (ms) => (ms == null ? '—' : new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z'));

// ── pass 1: count events (equal-count folds need the total first) ───────────
async function countEvents(file) {
  let n = 0;
  let first = null, last = null;
  const rl = readline.createInterface({ input: createReadStream(file), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line) continue;
    n++;
    const at = Number(line.match(/"at":(\d+)/)?.[1]);
    if (Number.isFinite(at)) {
      if (first == null) first = at;
      last = at;
    }
  }
  return { n, first, last };
}

console.log(`archives (${archiveFiles.length}):`);
let totalEvents = 0, archiveFirst = null, archiveLast = null;
for (const a of archiveFiles) {
  const { n, first, last } = await countEvents(a);
  totalEvents += n;
  if (first != null && (archiveFirst == null || first < archiveFirst)) archiveFirst = first;
  if (last != null && (archiveLast == null || last > archiveLast)) archiveLast = last;
  console.log(`  ${a} — ${n} events, ${utc(first)} → ${utc(last)} (${(statSync(a).size / 1e6).toFixed(1)} MB)`);
}
if (!totalEvents) { console.error('archives hold no events'); process.exit(2); }
const spanMs = archiveLast - archiveFirst;
const spanDays = spanMs / 86_400_000;
console.log(`coverage: ${totalEvents} events over ${spanDays.toFixed(2)} days (${utc(archiveFirst)} → ${utc(archiveLast)})`);

// ── pass 2: split into folds ────────────────────────────────────────────────
const foldsDir = join(outDir, 'folds');
mkdirSync(foldsDir, { recursive: true });
const foldPaths = [];
for (let i = 0; i < foldsN; i++) foldPaths.push(join(foldsDir, `fold-${i}.jsonl`));
const foldStats = Array.from({ length: foldsN }, () => ({ events: 0, firstAtMs: null, lastAtMs: null }));
const perFold = Math.ceil(totalEvents / foldsN);

if (!dryRun) {
  const streams = foldPaths.map((p) => createWriteStream(p));
  const write = (i, line) => new Promise((res) => streams[i].write(line + '\n', res));
  let idx = 0;
  for (const file of archiveFiles) {
    const rl = readline.createInterface({ input: createReadStream(file), crlfDelay: Infinity });
    for await (const line of rl) {
      if (!line) continue;
      const i = Math.min(foldsN - 1, Math.floor(idx / perFold));
      const at = Number(line.match(/"at":(\d+)/)?.[1]);
      const st = foldStats[i];
      st.events++;
      if (st.firstAtMs == null) st.firstAtMs = at;
      st.lastAtMs = at;
      await write(i, line);
      idx++;
    }
  }
  await Promise.all(streams.map((s) => new Promise((res) => s.end(res))));
  for (let i = 0; i < foldsN; i++) {
    console.log(`  fold ${i}: ${foldStats[i].events} events, ${utc(foldStats[i].firstAtMs)} → ${utc(foldStats[i].lastAtMs)} → ${foldPaths[i]}`);
  }
}

// ── run the replays ─────────────────────────────────────────────────────────
const runsDir = join(outDir, 'runs');
mkdirSync(runsDir, { recursive: true });
const num = (v) => {
  if (v == null) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
};
const r3 = (n) => (n == null ? '—' : (Math.round(n * 1000) / 1000).toString());

function extractMetrics(rep) {
  const t = rep.trades ?? {};
  const wins = num(t.wins) ?? 0, losses = num(t.losses) ?? 0;
  const gp = num(t.grossProfitUsd) ?? 0, gl = num(t.grossLossUsd) ?? 0;
  const payoff = wins > 0 && losses > 0 ? (gp / wins) / (gl / losses) : null;
  return {
    closed: num(t.closed) ?? 0,
    wins, losses,
    winRatePct: num(t.winRatePct),
    profitFactor: num(t.profitFactor),
    payoff,
    netPnlUsd: num(t.netPnlUsd),
    grossProfitUsd: gp,
    grossLossUsd: gl,
    feesUsd: num(t.feesUsd),
    orders: num(rep.orders?.orders),
    filled: num(rep.fills),
    openPositions: num(rep.openPositions),
    events: num(rep.sourceStats?.events),
    startAtMs: num(rep.startAtMs),
    endAtMs: num(rep.endAtMs),
  };
}

/**
 * The kernel's startup self-check line (#265):
 *   blitzkrieg-core: strategy startup self-check: requested=N resolved=M enabled=[a, b]
 * Returns null when the line is absent (a pre-#265 binary, or a log that was
 * lost) — which is itself a reason to distrust the run, not to accept it.
 */
function parseSelfCheck(stderr) {
  const m = stderr.match(/strategy startup self-check: requested=(\d+) resolved=(\d+) enabled=\[([^\]]*)\]/);
  if (!m) return null;
  return {
    requested: Number(m[1]),
    resolved: Number(m[2]),
    enabled: m[3].split(',').map((s) => s.trim()).filter(Boolean),
  };
}

/**
 * Why this run's log says the kernel had no usable strategy — or null when the
 * log is exactly what the sweep asked for. Every branch is a fact the kernel
 * printed; nothing is inferred from the trade count (a real strategy with no
 * signal also closes zero trades, and telling those two apart is the point).
 */
function invalidReason(stderr, selfCheck) {
  if (/unknown strategy requested/.test(stderr)) {
    return 'kernel log: unknown strategy requested (the library was not loaded)';
  }
  if (/refusing to start: none of the \d+ explicitly requested/.test(stderr)) {
    return 'kernel log: the kernel refused to start over unresolved strategies';
  }
  if (!selfCheck) {
    return 'kernel log: no strategy startup self-check line (binary predates #265?)';
  }
  if (selfCheck.resolved < STRATEGIES.length) {
    return `kernel log: requested ${selfCheck.requested} strategies but resolved ${selfCheck.resolved} (expected ${STRATEGIES.length})`;
  }
  const missing = STRATEGIES.filter((s) => !selfCheck.enabled.includes(s));
  if (missing.length) {
    return `kernel log: resolved but not enabled: ${missing.join(', ')}`;
  }
  return null;
}

/** One replay. Returns `{ report, invalid }` — `report` is null when the run was
 *  invalid, and `invalid` is the reason (never a silently filled zero). A run
 *  that fails for any OTHER reason still exits the sweep: the walk-forward
 *  report is only as good as its runs. */
function runBacktest(args2, reportPath) {
  return new Promise((res) => {
    if (existsSync(reportPath)) {
      try {
        res({ report: JSON.parse(readFileSync(reportPath, 'utf8')), invalid: null, cached: true });
        console.log('    (resume: cached report)');
        return;
      } catch { /* stale file — rerun */ }
    }
    console.log(`    $ blitzkrieg-core ${args2.join(' ')}`);
    const child = spawn(BIN, args2, { stdio: ['ignore', 'ignore', 'pipe'], cwd: outDir });
    let stderr = '';
    child.stderr.on('data', (d) => { stderr += d.toString(); });
    const watchdog = setTimeout(() => {
      console.error(`    TIMEOUT after ${runTimeoutMin} min — stopping the run group`);
      try { process.kill(-child.pid, 'SIGTERM'); } catch { try { child.kill('SIGTERM'); } catch {} }
    }, runTimeoutMin * 60_000);
    child.on('exit', (code) => {
      clearTimeout(watchdog);
      const invalid = invalidReason(stderr, parseSelfCheck(stderr));
      if (invalid) {
        // #265: this is not a crash, it is a kernel that ran (or refused to run)
        // without the strategies the sweep asked for. Mark the candidate, keep
        // the sweep going — a 0-trade report from this run would be a lie.
        for (const line of stderr.split('\n').filter((l) => /FAILED|unknown strategy|refusing to start/.test(l)).slice(0, 8)) {
          console.error(`      | ${line}`);
        }
        res({ report: null, invalid, cached: false });
        return;
      }
      if (code !== 0) {
        console.error(`backtest exited ${code}; refusing to write a report from a failed run`);
        console.error(stderr.split('\n').slice(-10).join('\n'));
        process.exit(1);
      }
      try {
        res({ report: JSON.parse(readFileSync(reportPath, 'utf8')), invalid: null, cached: false });
      } catch (e) {
        console.error(`cannot parse report ${reportPath}: ${e.message}`);
        process.exit(1);
      }
    });
  });
}

if (dryRun) {
  console.log('dry-run: would replay these candidates:');
  for (const combo of grid) console.log(`  ${candidateId(combo)}`);
  process.exit(0);
}

const results = []; // { candidate, fold, metrics, reportPath } — VALID runs only
const invalidRuns = []; // { candidate, fold, reason } — #265: no report is read from these
for (const combo of grid) {
  const id = candidateId(combo);
  const cdir = join(runsDir, candidateDir(id));
  mkdirSync(cdir, { recursive: true });
  const flags = Object.entries(combo).flatMap(([k, v]) => [PARAM_FLAGS[k], String(v)]);
  console.log(`candidate ${id}`);
  for (let f = 0; f < foldsN; f++) {
    const reportPath = join(cdir, `fold-${f}.json`);
    const run = await runBacktest(
      ['--backtest', foldPaths[f], ...OPS_KNOBS, ...flags,
        '--backtest-report', reportPath, '--backtest-tick-ms', tickMs, '--backtest-tail-ms', tailMs],
      reportPath,
    );
    if (run.invalid) {
      invalidRuns.push({ candidate: id, fold: f, reason: run.invalid });
      console.log(`    fold ${f}: INVALID — ${run.invalid}`);
      continue;
    }
    const rep = run.report;
    results.push({ candidate: id, combo, fold: f, metrics: extractMetrics(rep) });
    const m = extractMetrics(rep);
    console.log(`    fold ${f}: closed=${m.closed} WR=${r3(m.winRatePct)}% payoff=${r3(m.payoff)} PF=${r3(m.profitFactor)} net=${r3(m.netPnlUsd)}`);
  }
}

// Fold files can be huge; drop them unless the caller wants to re-run offline.
if (!keepFolds && !dryRun) {
  for (const p of foldPaths) { try { unlinkSync(p); } catch {} }
  try { if (readdirSync(foldsDir).length === 0) rmdirSync(foldsDir); } catch {}
}

// ── walk-forward arithmetic (over VALID runs only) ──────────────────────────
// An invalid run is not a zero: it is excluded here and reported separately, and
// a window with nothing valid left to pick from yields no pick rather than a
// winner-by-default.
const metricOf = (r) => r.metrics[selectMetric];
const better = (a, b) => (a == null ? false : b == null ? true : a > b);
const rowsOf = (fold) => results.filter((r) => r.fold === fold);

// Selection: on each fold i (0..N-2), the best candidate by the select metric.
const selections = [];
for (let i = 0; i < foldsN - 1; i++) {
  const rows = rowsOf(i);
  let pick = null;
  for (const r of rows) if (pick === null || better(metricOf(r), metricOf(pick))) pick = r;
  const validate = pick ? results.find((r) => r.fold === i + 1 && r.candidate === pick.candidate) : null;
  const bestValidate = rowsOf(i + 1)
    .reduce((acc, r) => (acc === null || better(metricOf(r), metricOf(acc)) ? r : acc), null);
  selections.push({
    trainFold: i,
    trainWindow: { firstAtMs: foldStats[i].firstAtMs, lastAtMs: foldStats[i].lastAtMs },
    pick: pick ? pick.candidate : null,
    trainMetric: pick ? metricOf(pick) : null,
    validateFold: i + 1,
    validateMetric: validate ? metricOf(validate) : null,
    bestValidateCandidate: bestValidate ? bestValidate.candidate : null,
    bestValidateMetric: bestValidate ? metricOf(bestValidate) : null,
    generalized: Boolean(pick && validate && bestValidate)
      && candidateId(pick.combo) === candidateId(bestValidate.combo),
  });
}
const picks = selections.map((s) => s.pick);
const stablePick = picks.length > 0 && picks.every((p) => p === picks[0]) ? picks[0] : null;
const shippedId = candidateId({});
const shippedRow = results.find((r) => r.candidate === shippedId);

const report = {
  generatedAtMs: Date.now(),
  selectMetric,
  folds: foldsN,
  perFoldEvents: perFold,
  archives: archiveFiles.map((a) => ({ path: a, sizeBytes: statSync(a).size })),
  coverage: {
    totalEvents, firstAtMs: archiveFirst, lastAtMs: archiveLast,
    spanDays: Number(spanDays.toFixed(4)),
    note: 'Thirty-day premise (E15): see spanDays — the report does not pretend the archive is 30 days.',
  },
  candidates: grid.map((c) => candidateId(c)),
  foldStats,
  results: results.map(({ candidate, fold, metrics }) => ({ candidate, fold, metrics })),
  selections,
  // #265: runs the kernel's own startup self-check marked unusable. They are
  // absent from `results` on purpose — a 0-trade report from a kernel that
  // loaded no strategy is not a data point.
  invalid: invalidRuns,
  expectedStrategies: STRATEGIES,
  verdict: {
    usable: invalidRuns.length === 0,
    picks,
    stablePick,
    shippedDefaultsCandidate: shippedId,
    shippedDefaultsPresent: Boolean(shippedRow),
  },
};

writeFileSync(join(outDir, 'walk-forward.json'), JSON.stringify(report, null, 2) + '\n');

// ── markdown ────────────────────────────────────────────────────────────────
const md = [];
md.push(`# Walk-forward sweep — ${utc(report.generatedAtMs)}`);
md.push('');
md.push(`- select metric: \`${selectMetric}\`; folds: ${foldsN} (~${perFold} events each, rolling walk-forward)`);
md.push(`- coverage: ${totalEvents} events, ${spanDays.toFixed(2)} days (${utc(archiveFirst)} → ${utc(archiveLast)})`);
md.push(`- candidates: ${grid.map((c) => `\`${candidateId(c)}\``).join(', ')}`);
if (invalidRuns.length) {
  const bad = [...new Set(invalidRuns.map((r) => r.candidate))];
  md.push(`- **INVALID runs: ${invalidRuns.length}** — the kernel's own startup self-check says the strategies were not running (${bad.map((c) => `\`${c}\``).join(', ')}). Those runs are excluded from every number below; their trade counts were never read (#265).`);
}
md.push('');
if (invalidRuns.length) {
  md.push('## Invalid runs (excluded)');
  md.push('');
  md.push('| candidate | fold | reason |');
  md.push('|---|---|---|');
  for (const r of invalidRuns) md.push(`| ${r.candidate} | ${r.fold} | ${r.reason} |`);
  md.push('');
  md.push(`Expected strategies per replay: ${STRATEGIES.map((s) => `\`${s}\``).join(', ')}. A run lands here when the kernel log shows the request unresolved (\`unknown strategy requested\`), when it refused to start, or when the startup self-check line is missing — never because it closed few trades.`);
  md.push('');
}
md.push('## Fold × candidate matrix');
md.push('');
md.push('| fold | window (UTC) | candidate | closed | WR% | payoff | PF | net USD |');
md.push('|---|---|---|---|---|---|---|---|');
for (let f = 0; f < foldsN; f++) {
  for (const cid of grid.map((c) => candidateId(c))) {
    const r = results.find((x) => x.fold === f && x.candidate === cid);
    const m = r?.metrics;
    const cell = invalidRuns.some((x) => x.fold === f && x.candidate === cid) ? 'INVALID' : (m?.closed ?? '—');
    md.push(`| ${f} | ${utc(foldStats[f].firstAtMs)} → ${utc(foldStats[f].lastAtMs)} | ${cid} | ${cell} | ${r3(m?.winRatePct)} | ${r3(m?.payoff)} | ${r3(m?.profitFactor)} | ${r3(m?.netPnlUsd)} |`);
  }
}
md.push('');
md.push('## Walk-forward selections (train fold i → validate fold i+1)');
md.push('');
md.push('| train fold | pick | train metric | validate fold | validate metric | best validate | generalized |');
md.push('|---|---|---|---|---|---|---|');
for (const s of selections) {
  md.push(`| ${s.trainFold} | ${s.pick ?? '—'} | ${r3(s.trainMetric)} | ${s.validateFold} | ${r3(s.validateMetric)} | ${s.bestValidateCandidate ?? '—'} (${r3(s.bestValidateMetric)}) | ${s.generalized ? 'yes' : 'no'} |`);
}
md.push('');
md.push('## Verdict');
md.push('');
if (invalidRuns.length) {
  md.push(`**This sweep is NOT usable as evidence.** ${invalidRuns.length} of ${foldsN * grid.length} replays ran without the strategies the sweep asked for (see the invalid-runs table). Fix the load path first — a kernel that starts with zero strategies produces 0-trade reports indistinguishable from "no signal", which is exactly the confusion this section exists to prevent.`);
} else if (stablePick) {
  md.push(`The same candidate wins every selection window: **${stablePick}**.`);
} else {
  md.push(`Selection is NOT stable across windows: picks were ${picks.map((p) => `\`${p}\``).join(', ')}. A parameter that only wins some windows is noise, not an improvement — do not adopt on this sweep alone.`);
}
if (shippedRow) {
  const shippedValidate = selections.map((s) => {
    const r = results.find((x) => x.fold === s.validateFold && x.candidate === shippedId);
    return r ? metricOf(r) : null;
  }).filter((v) => v != null);
  const pickValidate = selections.map((s) => s.validateMetric).filter((v) => v != null);
  const mean = (a) => a.reduce((x, y) => x + y, 0) / a.length;
  md.push('');
  md.push(`Mean validate ${selectMetric}: picked ${mean(pickValidate).toFixed(4)} vs shipped defaults ${mean(shippedValidate).toFixed(4)}.`);
} else {
  md.push('');
  md.push(`Note: the shipped-defaults candidate (${shippedId}) is not in the grid — pass it explicitly to anchor the comparison.`);
}
md.push('');
md.push('Data premise (E15 / #97): walk-forward needs a long, continuous shadow corpus. This archive spans the days listed above — if it is far below 30 days, the sweep is a method validation, not a 30-day verification.');
md.push('');
writeFileSync(join(outDir, 'walk-forward.md'), md.join('\n'));

console.log(`\nreport: ${join(outDir, 'walk-forward.json')}`);
console.log(`report: ${join(outDir, 'walk-forward.md')}`);
if (invalidRuns.length) {
  console.error(`\nINVALID: ${invalidRuns.length} replay(s) ran without the strategies the sweep asked for (${STRATEGIES.join(', ')}). The report says so and is not usable as evidence — a kernel that starts with zero strategies reports zero trades exactly like a strategy with no signal (#265).`);
  process.exitCode = 1;
}
