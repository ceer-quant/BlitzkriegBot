#!/usr/bin/env node
/**
 * E14 memory baseline / re-measure — peak RSS of a dry core under a
 * sustained synthetic book load, and the GATE on it (#206).
 *
 * Reproduces the load shape the live feed produces, WITHOUT a market plugin:
 *   - one round with 40 markets (`engine.markets`),
 *   - 40 tokens, one full L2 snapshot of 20 levels per side per iteration
 *     (`engine.book` — the same choke point the Rust-native feed drives),
 *     rotated across the token set at the loop's ~20 Hz cadence,
 *   - the IPC maintenance loop ticking at the production cadence (50 ms).
 *
 * Peak and final RSS are sampled from `ps` every ~250 ms and archived as JSON,
 * so a later E14 run is directly comparable (acceptance: "内存从基线到目标").
 *
 * WHY THIS IS A GATE AND NOT A REPORT (#206): the archived `docs/perf/*.json`
 * numbers had no reader — nothing failed when memory grew, and both soak gates
 * check liveness and progress only. A measurement nobody judges is a snapshot,
 * not an acceptance criterion. This script now decides:
 *
 *   --max-peak-rss-mb N   peak RSS must stay ≤ N MB. Portable across platforms
 *                         (it is a bound, not a comparison), so it is the half
 *                         that can run on a CI runner.
 *   --baseline F          the archived measurement to compare against.
 *   --max-ratio R         peak must stay ≤ R × the baseline's peak (default
 *                         1.15 = the −15% E14 target read as a growth budget).
 *                         Only comparable when the baseline came from the SAME
 *                         platform: `ps` RSS is the kernel's accounting of a
 *                         different allocator (macOS libmalloc vs glibc), so a
 *                         cross-platform ratio is noise. A baseline that names
 *                         another platform is NOT applied and says so on stdout
 *                         — silently skipping is how a gate goes green for weeks.
 *
 * A breach exits 1 and prints peak RSS, final RSS, the baseline's peak and the
 * rule that failed. `--self-test` runs the SAME verdict against fixtures with
 * hand-computed answers, including ones that MUST fail: a gate that cannot fail
 * carries no signal, which is the defect this repo keeps re-finding.
 *
 * The archive default is `target/perf/` (gitignored): writing the measurement
 * into a tracked `docs/perf/*.json` is what made `dhat_profile.rs` dirty the
 * worktree on every run. Overwriting a TRACKED path now needs
 * `--allow-tracked-out` — the archived baselines are human-curated.
 *
 * Usage:
 *   node scripts/e14-memory-baseline.mjs --seconds 30
 *   node scripts/e14-memory-baseline.mjs --seconds 15 --max-peak-rss-mb 96
 *   node scripts/e14-memory-baseline.mjs --baseline docs/perf/e14-mem-baseline.json \
 *        --max-ratio 1.15 --max-peak-rss-mb 96
 *   node scripts/e14-memory-baseline.mjs --self-test
 * Exit: 0 within budget · 1 a budget was exceeded · 2 the measurement could not run
 */
import { spawn } from './lib/child-guard.mjs';
import net from 'node:net';
import { execSync, execFileSync } from 'node:child_process';
import { join, resolve, dirname, relative, isAbsolute } from 'node:path';
import { tmpdir } from 'node:os';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { sleep } from './lib/wait.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = join(ROOT, 'target', 'release', 'blitzkrieg-core');

const argv = process.argv.slice(2);
const flag = (name) => argv.includes(name);
const arg = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 ? Number(argv[i + 1]) || dflt : dflt;
};
// String-valued flags: only use the value when the flag was actually passed
// AND a value follows it — falling back to argv[0] (the node binary itself)
// is exactly how the first draft overwrote the interpreter with JSON.
const argStr = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : dflt;
};
const SECONDS = arg('--seconds', 30);
const TOKENS = arg('--tokens', 40);
const DEPTH = arg('--depth', 20);
const OUT = resolve(ROOT, argStr('--out', join('target', 'perf', 'e14-mem-run.json')));
const SELF_TEST = flag('--self-test');
const ALLOW_TRACKED_OUT = flag('--allow-tracked-out');
// Both rules are off unless asked for: the documented default form is a
// measurement (`--seconds 30` archives and prints), the gate is opt-in so an
// exploratory re-measure is not refused by a budget it did not name.
const MAX_PEAK_RSS_MB = argv.includes('--max-peak-rss-mb')
  ? Number(argStr('--max-peak-rss-mb', ''))
  : null;
const MAX_RATIO = argv.includes('--max-ratio') ? Number(argStr('--max-ratio', '')) : 1.15;
const BASELINE_PATH = argv.includes('--baseline')
  ? resolve(ROOT, argStr('--baseline', ''))
  : null;

if (OUT === ROOT || !OUT.startsWith(ROOT + '/')) {
  if (!SELF_TEST) {
    console.error(`FAIL: --out must resolve inside the repo, got ${OUT}`);
    process.exit(2);
  }
}

/** Is this repo-relative path under version control? */
function isTracked(abs) {
  const rel = isAbsolute(abs) ? relative(ROOT, abs) : abs;
  if (!rel || rel.startsWith('..')) return false;
  try {
    execFileSync('git', ['ls-files', '--error-unmatch', '--', rel], {
      cwd: ROOT,
      stdio: ['ignore', 'ignore', 'ignore'],
    });
    return true;
  } catch {
    return false;
  }
}

// ── The verdict: one place decides pass/fail, used by the run and the self-test ──
//
// Pure — no I/O, no process.exit — so `--self-test` exercises exactly the code a
// real run does. `result` is the measurement (or a hand-made fixture); `limits`
// carries {maxPeakRssMb, maxRatio, baseline}.
function verdict(result, limits) {
  const problems = [];
  const notes = [];
  const kbToMb = (kb) => kb / 1024;
  const peakKb = Number(result?.rssKbPeak ?? 0);
  const finalKb = result?.rssKbFinal === null ? null : Number(result?.rssKbFinal ?? 0);

  if (limits.maxPeakRssMb != null) {
    if (!(peakKb > 0)) {
      // A run that never sampled the process cannot be certified as within
      // budget: the earlier draft reported zero samples as 0 KB, i.e. green.
      problems.push(
        `no RSS sample was taken (peak ${result?.rssKbPeak}), so the --max-peak-rss-mb ` +
          `${limits.maxPeakRssMb} MB budget is unproven — the core died or never answered`,
      );
    } else if (kbToMb(peakKb) > limits.maxPeakRssMb) {
      problems.push(
        `peak RSS ${kbToMb(peakKb).toFixed(1)} MB > --max-peak-rss-mb ${limits.maxPeakRssMb} MB ` +
          `(over by ${(kbToMb(peakKb) - limits.maxPeakRssMb).toFixed(1)} MB; final RSS ` +
          `${finalKb === null ? 'unavailable' : kbToMb(finalKb).toFixed(1) + ' MB'})`,
      );
    }
  }

  if (limits.baseline != null) {
    const baselinePeakKb = Number(limits.baseline?.rssKbPeak ?? 0);
    const basePlatform = limits.baseline?.platform ?? null;
    if (basePlatform !== null && basePlatform !== result.platform) {
      // Loud, not silent: the ratio is not comparable across allocators, and a
      // skipped rule that prints nothing is indistinguishable from a pass.
      notes.push(
        `baseline platform is ${basePlatform}, this run is ${result.platform}: the ` +
          `--max-ratio ${limits.maxRatio} rule is NOT comparable across allocators and was not ` +
          'applied (the absolute --max-peak-rss-mb ceiling still applies)',
      );
    } else if (!(baselinePeakKb > 0)) {
      problems.push(`baseline ${limits.baselinePath} carries no usable rssKbPeak`);
    } else if (basePlatform === null) {
      notes.push(
        `baseline ${limits.baselinePath} predates platform recording: the --max-ratio ` +
          `${limits.maxRatio} rule assumed this run's platform (${result.platform})`,
      );
    } else if (kbToMb(peakKb) > kbToMb(baselinePeakKb) * limits.maxRatio) {
      problems.push(
        `peak RSS ${kbToMb(peakKb).toFixed(1)} MB is ` +
          `${(peakKb / baselinePeakKb).toFixed(3)}x the baseline ${kbToMb(baselinePeakKb).toFixed(1)} MB ` +
          `(${limits.baselinePath}, max ${limits.maxRatio}x; over by ` +
          `${(kbToMb(peakKb) - kbToMb(baselinePeakKb) * limits.maxRatio).toFixed(1)} MB)`,
      );
    }
  }

  return { ok: problems.length === 0, problems, notes };
}

function readBaseline(path) {
  if (!path) return null;
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch (e) {
    console.error(`FAIL: cannot read --baseline ${path}: ${e.message}`);
    process.exit(2);
  }
}

function limitsFor(baseline) {
  return {
    maxPeakRssMb: Number.isFinite(MAX_PEAK_RSS_MB) ? MAX_PEAK_RSS_MB : null,
    maxRatio: MAX_RATIO,
    baseline,
    baselinePath: BASELINE_PATH === null ? '(none)' : relative(ROOT, BASELINE_PATH),
  };
}

/**
 * `--self-test`: the SAME verdict, against fixtures whose answers are known by
 * hand. Includes fixtures that MUST fail — an all-green self-test would prove
 * nothing about a gate's ability to fail.
 */
function selfTest() {
  const fixtures = [
    {
      name: 'within budget, matching-platform baseline',
      result: { rssKbPeak: 14_800, rssKbFinal: 14_300, platform: 'darwin' },
      limits: {
        maxPeakRssMb: 96,
        maxRatio: 1.15,
        baseline: { rssKbPeak: 17_472, platform: 'darwin' },
        baselinePath: 'docs/perf/e14-mem-baseline.json',
      },
      want: true,
    },
    {
      name: 'peak over the absolute ceiling',
      result: { rssKbPeak: 200_000, rssKbFinal: 199_000, platform: 'linux' },
      limits: { maxPeakRssMb: 96, maxRatio: 1.15, baseline: null, baselinePath: '(none)' },
      want: false,
    },
    {
      name: 'peak exactly at the absolute ceiling',
      result: { rssKbPeak: 96 * 1024, rssKbFinal: 96 * 1024, platform: 'linux' },
      limits: { maxPeakRssMb: 96, maxRatio: 1.15, baseline: null, baselinePath: '(none)' },
      want: true,
    },
    {
      name: 'peak 1.20x the matching-platform baseline (max 1.15x)',
      result: { rssKbPeak: 20_966, rssKbFinal: 20_000, platform: 'darwin' },
      limits: {
        maxPeakRssMb: 96,
        maxRatio: 1.15,
        baseline: { rssKbPeak: 17_472, platform: 'darwin' },
        baselinePath: 'docs/perf/e14-mem-baseline.json',
      },
      want: false,
    },
    {
      name: 'peak 1.05x the matching-platform baseline (max 1.15x)',
      result: { rssKbPeak: 18_345, rssKbFinal: 18_000, platform: 'darwin' },
      limits: {
        maxPeakRssMb: 96,
        maxRatio: 1.15,
        baseline: { rssKbPeak: 17_472, platform: 'darwin' },
        baselinePath: 'docs/perf/e14-mem-baseline.json',
      },
      want: true,
    },
    {
      name: 'baseline from another platform: ratio not applied (note, not a pass)',
      result: { rssKbPeak: 900_000, rssKbFinal: 900_000, platform: 'linux' },
      limits: {
        maxPeakRssMb: null,
        maxRatio: 1.15,
        baseline: { rssKbPeak: 17_472, platform: 'darwin' },
        baselinePath: 'docs/perf/e14-mem-baseline.json',
      },
      want: true,
      wantNote: /NOT comparable across allocators/,
    },
    {
      name: 'no sample taken is NOT a pass',
      result: { rssKbPeak: 0, rssKbFinal: null, platform: 'linux' },
      limits: { maxPeakRssMb: 96, maxRatio: 1.15, baseline: null, baselinePath: '(none)' },
      want: false,
    },
  ];

  let failed = 0;
  for (const f of fixtures) {
    const v = verdict(f.result, f.limits);
    const noteOk = f.wantNote ? v.notes.some((n) => f.wantNote.test(n)) : true;
    const ok = v.ok === f.want && noteOk;
    if (!ok) failed++;
    console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${f.name}`);
    console.log(`       want ok=${f.want} got ok=${v.ok}${f.wantNote ? ' (note required)' : ''}`);
    for (const p of v.problems) console.log(`       problem: ${p}`);
    for (const n of v.notes) console.log(`       note:    ${n}`);
  }
  console.log('');
  if (failed > 0) {
    console.error(`e14-memory-baseline --self-test — ${failed}/${fixtures.length} fixtures wrong.`);
    return 1;
  }
  console.log(`e14-memory-baseline --self-test — ${fixtures.length} fixtures, verdicts as expected.`);
  return 0;
}

function rpcLine(c, obj) {
  return new Promise((res) => {
    c.write(JSON.stringify(obj) + LF, () => res());
  });
}

const rssOf = (pid) => {
  try {
    return Number(execSync(`ps -o rss= -p ${pid}`, { encoding: 'utf8' }).trim());
  } catch {
    return null;
  }
};

async function waitSocket() {
  for (let i = 0; i < 100; i++) {
    const ok = await new Promise((res) => {
      const c = net.connect(SOCK);
      c.on('connect', () => { c.end(); res(true); });
      c.on('error', () => res(false));
    });
    if (ok) return true;
    await sleep(100);
  }
  return false;
}

const WORK = mkdtempSync(join(tmpdir(), 'e14-mem-'));
const SOCK = join(WORK, 'mem.sock');
const LF = String.fromCharCode(10);

async function main() {
  const core = spawn(CORE,
    ['--socket', SOCK, '--mode', 'dry', '--engine', '--tick-ms', '50',
     '--no-event-archive', '--no-trade-log', '--no-order-log', '--no-position-log'],
    { cwd: WORK, stdio: ['ignore', 'ignore', 'inherit'] });
  const pid = core.pid;
  if (!await waitSocket()) {
    console.error('FAIL: core socket never appeared');
    process.exit(2);
  }

  const c = net.connect(SOCK);
  await new Promise((res) => c.on('connect', res));
  let buf = '';
  c.on('data', (d) => { buf += d.toString(); });

  const notify = (method, params) =>
    rpcLine(c, { jsonrpc: '2.0', id: 1, method, params });

  // One round, 40 markets, horizon far beyond the measurement window.
  const nowMs = Date.now();
  const mkts = Array.from({ length: TOKENS }, (_, i) => ({
    asset: `A${i}`,
    conditionId: `cond-${i}`,
    questionId: `q-${i}`,
    upTokenId: `tok-${i}`,
    downTokenId: `dn-${i}`,
    upPrice: '0.60',
    downPrice: '0.40',
    expiresAtMs: nowMs + 10 * 3600 * 1000,
    roundSlot: Math.floor(nowMs / 1000 / 900),
    negRisk: true,
    question: '?',
  }));
  await notify('engine.markets', { markets: mkts });

  const mid = 0.45;
  const bookBody = (i) => ({
    tokenId: `tok-${i}`,
    bids: Array.from({ length: DEPTH }, (_, l) => ({ price: (mid - (l + 1) * 0.01).toFixed(2), size: '100' })),
    asks: Array.from({ length: DEPTH }, (_, l) => ({ price: (mid + (l + 1) * 0.01).toFixed(2), size: '100' })),
    tsMs: nowMs,
  });

  // One full book per iteration, rotating tokens, for SECONDS seconds.
  const t0 = Date.now();
  const samples = [];
  let peak = 0;
  let peakAt = 0;
  while (Date.now() - t0 < SECONDS * 1000) {
    const b = bookBody(Math.floor(Math.random() * TOKENS));
    b.tsMs = Date.now();
    await notify('engine.book', b);
    if (Date.now() - t0 > 250 * samples.length) {
      const r = rssOf(pid);
      if (r !== null) {
        samples.push(r);
        if (r > peak) { peak = r; peakAt = Math.round((Date.now() - t0) / 1000); }
      }
    }
    await sleep(50);
  }
  const final = rssOf(pid);
  const result = {
    tokens: TOKENS, depth: DEPTH, seconds: SECONDS,
    rssKbPeak: peak, rssKbPeakAtSec: peakAt, rssKbFinal: final,
    // Recorded from #206 on: the ratio rule is only meaningful within one
    // platform, and a file that does not say which one it measured cannot be
    // used honestly by a later run on another.
    platform: process.platform,
    samples,
    at: new Date().toISOString(),
  };
  c.end();
  core.kill('SIGTERM');
  await sleep(300);
  try { rmSync(WORK, { recursive: true, force: true }); } catch {}

  const baseline = readBaseline(BASELINE_PATH);
  const limits = limitsFor(baseline);
  const v = verdict(result, limits);

  console.log(JSON.stringify(result, null, 2));
  console.log('');
  const mb = (kb) => (kb === null || kb === undefined ? 'unavailable' : `${(kb / 1024).toFixed(1)} MB`);
  console.log(`  platform ${result.platform} · ${result.tokens} tokens × depth ${result.depth} · ${result.seconds}s`);
  console.log(`  peak RSS  ${mb(result.rssKbPeak)} (at ${result.rssKbPeakAtSec}s)   final RSS ${mb(result.rssKbFinal)}`);
  if (limits.maxPeakRssMb != null) console.log(`  budget    peak ≤ ${limits.maxPeakRssMb} MB (--max-peak-rss-mb)`);
  if (baseline) {
    console.log(`  baseline  peak ${mb(baseline.rssKbPeak)} (${limits.baselinePath}` +
      `${baseline.platform ? `, ${baseline.platform}` : ', platform unrecorded'}), max ${limits.maxRatio}x`);
  }
  for (const n of v.notes) console.log(`  note: ${n}`);
  for (const p of v.problems) console.error(`  FAIL ${p}`);

  if (OUT !== ROOT && isTracked(OUT) && !ALLOW_TRACKED_OUT) {
    // The defect #206 fixed for the dhat profile, closed here too: a measurement
    // must not overwrite a curated, version-controlled baseline by default.
    console.error(`  FAIL ${relative(ROOT, OUT)} is tracked by git and holds a curated baseline.`);
    console.error('       Pick another --out (target/perf/ is gitignored) or pass --allow-tracked-out');
    console.error('       if you really intend to replace the archived baseline.');
    process.exit(1);
  }
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, JSON.stringify(result, null, 2));
  console.log(`archived → ${relative(ROOT, OUT)}`);

  if (!v.ok) {
    console.error(`\ne14-memory-baseline — memory budget exceeded (${v.problems.length} rule(s)).`);
    process.exit(1);
  }
  console.log('\ne14-memory-baseline — within budget.');
  process.exit(0);
}

if (SELF_TEST) {
  process.exit(selfTest());
}

main().catch((e) => {
  console.error('FAIL', e);
  process.exit(2);
});
