#!/usr/bin/env node
/**
 * smart_money_reverse backtest replay — the bit-exact falsification line and
 * the pair_discount_arb validation line over a frozen corpus.
 *
 * Why this exists: the 57-day minute-level corpus sweep (bk-session/smr-round/
 * whale-trigger-spec.md) judged the stabilize-then-bid shape at negative ROI
 * and fade-FOMO at 89.4% settlement WR for the chased side. That verdict ran on
 * 60s oracle snapshots; this script re-measures BOTH verdicts through the
 * production kernel (`blitzkrieg-core --backtest`), where every fill is
 * adjudicated by the live OME/fill-model/fee path a real order would meet.
 * It also arms the POSITIVE candidate the sweep found — pair-discount
 * collection (almach buys both legs after a crash 49%/48%) — so the delivery
 * decision has a bit-exact number for each side of the argument.
 *
 * Two lines, one corpus set:
 *
 *   smr  (falsification)  smart_money_reverse, crash_enabled=on +
 *                         spot_missing=pass (the frozen windows carry no spot
 *                         feed, so the underlying-asset veto must be released
 *                         explicitly — a book-only diagnostic, named as such),
 *                         `--maker-rebate` on: default / widest / strictest
 *                         grids, plus a fomo fade arm. Expected: zero fills or
 *                         negative PnL; anything else is a finding.
 *   pair (validation)     pair_discount_arb at max_pair_cost 0.985 (shipped) /
 *                         0.999 / 1.0 (diagnostic ceiling) with
 *                         `--maker-rebate`: does the post-crash ask-sum
 *                         discount window actually open, how often, at what PF.
 *
 * Corpus, two sources:
 *   default          the four PINNED one-hour windows of
 *                    scripts/lib/frozen-corpus.mjs (sha256-verified, book+round
 *                    only, 2026-09-19/20 capture).
 *   --archive <dir>  slices of the core live archive (data/archive rotated
 *                    segments, `events.<stamp>.jsonl`; 2026-09-30T01:28Z →
 *                    2026-11-01, ~41GB). Segments are named by their LAST
 *                    event, so segment i covers (name[i-1], name[i]] and a
 *                    window only scans the segments that can overlap it.
 *                    `--from <ISO> --hours N` or repeatable `--window n=AT`.
 *
 * The archive carries book+spot+round but NO `resolution` rows: without one a
 * position settles through the core's DRY settlement ladder (book-evidence
 * verdict, withheld when no token's evidence clears 0.5 — documented as
 * `core-dry` provenance, never as a market fact). `--resolve-from
 * <activity.jsonl>` restores the real verdicts first: REDEEM rows with
 * usdcSize>0 name the winning token (a losing token also redeems, at 0 —
 * whale-trigger-spec §0), and the slice's own round lines supply the losing
 * token and negRisk. Resolutions are stamped at the round's expiry instant and
 * merged into the slice in `at` order.
 *
 * Honesty rails, inherited:
 *   * a run whose stderr fails the #265 strategy startup self-check is marked
 *     INVALID and its zero trades are never read as "no signal";
 *   * packages are staged as throwaway copies with ONLY manifest defaults
 *     rewritten (`--param k=v` of the mad-dog template), so every arm is a
 *     pure manifest diff, reproducible from this file;
 *   * every number printed comes from a `--backtest-report` JSON, not from
 *     this script's arithmetic on assumptions.
 *
 * Usage:
 *   node scripts/smart-money-reverse-replay.mjs
 *   node scripts/smart-money-reverse-replay.mjs --line smr
 *   node scripts/smart-money-reverse-replay.mjs --archive '/Volumes/Hard Disk/BlitzkriegBot/data/archive' \
 *        --from 2026-10-15T17:00:00Z --hours 3
 *   node scripts/smart-money-reverse-replay.mjs --archive <dir> --from 2026-09-30T12:00:00Z --hours 3 \
 *        --resolve-from '/Volumes/Hard Disk/BlitzkriegBot/data/almach/activity-full.jsonl'
 *
 * Outputs under $BK_SMR_OUT (default /Volumes/Hard Disk/bk-session/smr-round/
 * backtest-smr-replay-<stamp>): plan.json, results.json, one core report per
 * arm × window. Scratch (staged packages, corpus slices) under the worktree's
 * target/smr-replay — never /tmp.
 */

import { createHash } from 'crypto';
import { createReadStream, createWriteStream, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from 'fs';
import { createInterface } from 'readline';
import { gunzipSync } from 'zlib';
import { basename, dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';
import { spawn } from './lib/child-guard.mjs';
import { WINDOWS, LEAD_MS, corpusPath } from './lib/frozen-corpus.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..');
const BIN = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const SCRATCH = process.env.BK_SMR_SCRATCH || join(ROOT, 'target', 'smr-replay');
const DEFAULT_OUT =
  process.env.BK_SMR_OUT || join('/Volumes/Hard Disk/bk-session/smr-round', 'backtest-smr-replay-' + stamp());

function stamp() {
  return new Date().toISOString().replace(/[-:]/g, '').replace(/\..*/, '').replace('T', '-');
}

// ── the arms (the whole experimental design; each is a manifest-default diff) ─

const PACKAGES = {
  smr: { name: 'smart_money_reverse', dir: join(ROOT, 'user_layer', 'strategies_lua', 'smart_money_reverse') },
  pair: { name: 'pair_discount_arb', dir: join(ROOT, 'user_layer', 'strategies_lua', 'pair_discount_arb') },
};

const ARMS = {
  // 证伪线: the minute-level sweep says this shape is negative. The replay's
  // job is to confirm it where fills are bit-exact — default, thesis-ceiling
  // (widest gates), the sweep's least-bad subset (strictest), and the fade arm.
  smr: [
    { name: 'smr-default', params: { crash_enabled: 'on', spot_missing: 'pass' } },
    {
      name: 'smr-widest',
      params: {
        crash_enabled: 'on', spot_missing: 'pass',
        crash_drop_pct: '5', crash_window_sec: '300', settle_quiet_sec: '1',
        stabilize_depth_factor: '1', min_bid_depth: '0', min_time_left_sec: '0',
      },
    },
    {
      name: 'smr-strictest',
      params: {
        crash_enabled: 'on', spot_missing: 'pass',
        crash_drop_pct: '40', settle_quiet_sec: '30', stabilize_depth_factor: '2', min_bid_depth: '1000',
      },
    },
    { name: 'smr-fomo', params: { fomo_enabled: 'on', spot_missing: 'pass' } },
  ],
  // 验证线: the positive candidate. 0.985 is the shipped gate (it never opens
  // when the ask-sum sits at 1.01–1.03); 0.999 is the manifest max; 1.0 is the
  // diagnostic ceiling the loader accepts numerically (above it a "pair" costs
  // more than its $1 payoff by construction).
  pair: [
    { name: 'pair-default', params: { max_pair_cost: '0.985' } },
    { name: 'pair-0.999', params: { max_pair_cost: '0.999' } },
    { name: 'pair-1.0-diag', params: { max_pair_cost: '1.0' } },
  ],
};

// ── args ─────────────────────────────────────────────────────────────────────

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const has = (name) => args.includes(name);

const lineArg = flag('--line') ?? 'both';
if (!['smr', 'pair', 'both'].includes(lineArg)) {
  console.error(`--line must be smr|pair|both (got ${lineArg})`);
  process.exit(2);
}
const lines = lineArg === 'both' ? ['smr', 'pair'] : [lineArg];

const archiveDir = flag('--archive');
const fromArg = flag('--from');
const hoursArg = Number(flag('--hours') ?? 3);
const feeModel = flag('--fee-model') ?? 'official';
if (!['official', 'legacy_quadratic'].includes(feeModel)) {
  console.error(`--fee-model must be official|legacy_quadratic (got ${feeModel})`);
  process.exit(2);
}
const makerRebate = !has('--no-maker-rebate');
const tickMs = flag('--backtest-tick-ms') ?? '50';
const timeoutMin = Number(flag('--timeout-min') ?? 30);
const OUT_DIR = resolve(flag('--out') ?? DEFAULT_OUT);
const keepCorpus = has('--keep-corpus');
const onlyWindow = flag('--only');
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
if (!Number.isFinite(hoursArg) || hoursArg <= 0 || hoursArg > 72) {
  console.error('--hours must be a number in (0, 72]');
  process.exit(2);
}
if ((fromArg || customWindows.length) && !archiveDir) {
  console.error('--from/--window need --archive <dir>: the pinned frozen windows are fixed hours, not movable');
  process.exit(2);
}

// ── fail-friendly preflight: every missing piece named before anything runs ──

const missing = [];
if (!existsSync(BIN)) {
  missing.push(`core binary ${BIN} (build: cargo build --release -p blitzkrieg-core)`);
} else {
  try {
    if (!statSync(BIN).mode & 0o111) missing.push(`core binary ${BIN} is not executable`);
  } catch { /* stat failed but existsSync passed — leave it to the run */ }
}
for (const line of lines) {
  const pkg = PACKAGES[line];
  const manifestPath = join(pkg.dir, 'manifest.json');
  if (!existsSync(pkg.dir)) missing.push(`strategy package ${pkg.dir}`);
  else if (!existsSync(manifestPath)) missing.push(`package manifest ${manifestPath}`);
  else if (!existsSync(join(pkg.dir, JSON.parse(readFileSync(manifestPath, 'utf8')).entry))) {
    missing.push(`package entry ${join(pkg.dir, JSON.parse(readFileSync(manifestPath, 'utf8')).entry)}`);
  }
}
if (missing.length) {
  console.error('preflight FAILED — this run would be a fake green, refusing:');
  for (const m of missing) console.error(`  missing: ${m}`);
  process.exit(2);
}

// Pre-validate every arm's tunable names against the shipped manifest BEFORE
// the first replay, so a typo costs seconds, not a half-hour of replay.
for (const line of lines) {
  const manifest = JSON.parse(readFileSync(join(PACKAGES[line].dir, 'manifest.json'), 'utf8'));
  for (const arm of ARMS[line]) {
    const unknown = Object.keys(arm.params).filter((k) => !manifest.tunables?.[k]);
    if (unknown.length) {
      console.error(`arm ${arm.name}: unknown tunables ${unknown.join(', ')} (known: ${Object.keys(manifest.tunables ?? {}).join(', ')})`);
      process.exit(2);
    }
  }
}

mkdirSync(OUT_DIR, { recursive: true });
mkdirSync(join(SCRATCH, 'corpus'), { recursive: true });

// ── windows ──────────────────────────────────────────────────────────────────

let windows; // [{ name, at (ISO), spanMs }]
let corpusSource;
if (archiveDir) {
  const segs = listArchiveSegments(archiveDir);
  if (!segs.length) {
    console.error(`no rotated segments (events.<stamp>.jsonl) under ${archiveDir} — the live file events.jsonl is still being written and is deliberately excluded`);
    process.exit(2);
  }
  const requested = customWindows.length
    ? customWindows
    : fromArg
      ? [{ name: `archive-${stampOf(fromArg)}-${hoursArg}h`, at: fromArg, spanMs: hoursArg * 3_600_000 }]
      : (console.error('--archive needs --from <ISO> --hours N, or repeatable --window name=AT'), process.exit(2));
  windows = requested.map((w) => ({ ...w, spanMs: w.spanMs ?? 3_600_000 }));
  for (const w of windows) {
    if (Number.isNaN(Date.parse(w.at))) {
      console.error(`window ${w.name}: cannot parse time ${w.at}`);
      process.exit(2);
    }
  }
  corpusSource = { kind: 'archive', dir: archiveDir, segments: segs.length };
} else {
  windows = WINDOWS.map((w) => ({ name: w.name, at: w.at, spanMs: 3_600_000 }));
  corpusSource = { kind: 'frozen-pins', dir: join(ROOT, 'docs/reports/data/mean-reversion-gate') };
}
const runWindows = onlyWindow ? windows.filter((w) => w.name === onlyWindow) : windows;
if (!runWindows.length) {
  console.error(`--only ${onlyWindow}: no such window (have: ${windows.map((w) => w.name).join(', ')})`);
  process.exit(2);
}

function stampOf(iso) {
  return iso.replace(/[-:]/g, '').replace(/\.\d+Z$/, 'Z').replace('T', 'T').slice(0, 16);
}

/** sha256 of a file, streamed chunk-wise (byte-exact — no line splitting) —
 *  a 32-day slice must not be read whole. */
function sha256File(path) {
  return new Promise((res, rej) => {
    const h = createHash('sha256');
    const s = createReadStream(path);
    s.on('data', (c) => h.update(c));
    s.on('end', () => res(h.digest('hex')));
    s.on('error', rej);
  });
}

/** The rotated archive segments, chronological, live file excluded.
 *  Segment names carry the segment's LAST event time (EventArchive::rotate),
 *  so segment i covers (stamp[i-1], stamp[i]] — the basis of the overlap
 *  pruning that keeps a 3h window from scanning 41GB. */
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

// ── corpus materialization ───────────────────────────────────────────────────

const corpusProvenance = {}; // name → { path, sha256, lines, bytes, rounds, resolutions, redeemMisses, source }

async function materializeFrozen(w) {
  const pin = WINDOWS.find((x) => x.name === w.name);
  if (!pin) {
    console.error(`window ${w.name} is not a pinned frozen window`);
    process.exit(2);
  }
  const gz = corpusPath(ROOT, pin);
  if (!existsSync(gz)) {
    console.error(`missing corpus ${gz}: the pinned windows ship with the repo — restore them or run --archive mode instead`);
    process.exit(2);
  }
  const buf = gunzipSync(readFileSync(gz));
  const sha = createHash('sha256').update(buf).digest('hex');
  if (sha !== pin.sha256) {
    console.error(`${w.name}: corpus sha256 ${sha} != pinned ${pin.sha256} — refusing to report on a changed corpus`);
    process.exit(2);
  }
  const path = join(SCRATCH, 'corpus', `${w.name}.jsonl`);
  writeFileSync(path, buf);
  let lines = 0;
  for (let i = 0; i < buf.length; i++) if (buf[i] === 0x0a) lines++;
  corpusProvenance[w.name] = { path, sha256: sha, lines, source: 'frozen-pin (book+round only, spot absent by construction)' };
  return path;
}

/** One streaming pass over the overlapping segments; writes the raw slice and
 *  collects the window's round metadata (condition → tokens/negRisk/expiry)
 *  for the resolution back-fill. */
async function sliceArchiveWindow(w) {
  const t0 = Date.parse(w.at);
  const t1 = t0 + w.spanMs;
  const lead0 = t0 - LEAD_MS;
  const rawPath = join(SCRATCH, 'corpus', `${w.name}.raw.jsonl`);
  const rounds = new Map(); // conditionId → { upTokenId, downTokenId, negRisk, expiresAtMs }
  let lines = 0;
  const overlapping = listArchiveSegments(archiveDir).filter((s) => s.startMs < t1 && s.endMs > lead0);
  if (!overlapping.length) {
    throw new Error(
      `window ${w.name} (${new Date(t0).toISOString()} +${w.spanMs / 3_600_000}h) overlaps no segment of ${archiveDir}` +
      ` — archive covers ${new Date(listArchiveSegments(archiveDir)[0]?.endMs ?? 0).toISOString()} → ${new Date(listArchiveSegments(archiveDir).at(-1)?.endMs ?? 0).toISOString()}`,
    );
  }
  for (const seg of overlapping) {
    const rl = createInterface({ input: createReadStream(seg.file), crlfDelay: Infinity });
    const out = createWriteStream(rawPath, { flags: 'a' });
    let bytes = 0;
    const write = (line) => new Promise((res) => out.write(line + '\n', res));
    for await (const line of rl) {
      if (!line) continue;
      const at = Number(line.match(/"at":(\d+)/)?.[1]);
      if (!Number.isFinite(at) || at < lead0 || at >= t1) continue;
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
      bytes += line.length + 1;
    }
    out.end();
    console.log(`    slice ${w.name}: ${basename(seg.file)} → +${lines} lines`);
  }
  return { rawPath, rounds, lines, t0, t1 };
}

/** REDEEM(usdcSize>0) rows inside the replayed span → conditionId → winning
 *  token id. A losing token redeems at usdc 0 with an empty asset, so the
 *  first paying row names the winner; conflicting paying rows are counted. */
async function redeemWinners(span) {
  const winners = new Map(); // conditionId → { token, atMs, conflicts }
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

/** Merge the generated resolution rows into the raw slice in `at` order.
 *  Resolutions beyond the slice's last event are dropped (the replay clock
 *  never reaches them); they are counted, never silently lost. */
async function injectResolutions(w, span, rounds, winners) {
  let injected = 0, missingRound = 0, beyondEnd = 0, inLead = 0;
  const pending = [];
  for (const [conditionId, win] of winners) {
    const mk = rounds.get(conditionId);
    if (!mk) { missingRound++; continue; }
    const loser = win.token === mk.upTokenId ? mk.downTokenId : mk.upTokenId;
    const at = mk.expiresAtMs ?? Math.max(win.atMs, span.t0);
    if (at < span.t0) { inLead++; continue; } // expired before the window: no position can be open
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

// ── package staging (mad-dog shape: manifest defaults only, sha untouched) ──

function stagePackage(line, arm) {
  const pkg = PACKAGES[line];
  const root = mkdtempSync(join(SCRATCH, 'pkg-'));
  const dst = join(root, pkg.name);
  rmSync(dst, { force: true, recursive: true });
  cpSyncLocal(pkg.dir, dst);
  if (Object.keys(arm.params).length) {
    const manifestPath = join(dst, 'manifest.json');
    const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
    for (const [k, v] of Object.entries(arm.params)) manifest.tunables[k].default = v;
    writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
  }
  return { luaDir: root, name: pkg.name };
}

function cpSyncLocal(src, dst) {
  mkdirSync(dst, { recursive: true });
  for (const f of readdirSync(src, { withFileTypes: true })) {
    const s = join(src, f.name), d = join(dst, f.name);
    if (f.isDirectory()) cpSyncLocal(s, d);
    else writeFileSync(d, readFileSync(s));
  }
}

// ── one replay ───────────────────────────────────────────────────────────────

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

function runOne(line, arm, w, corpusPathForRun, reportPath) {
  const pkg = PACKAGES[line];
  const staged = stagePackage(line, arm);
  return new Promise((res) => {
    const argv = [
      '--backtest', corpusPathForRun,
      '--lua-strategy-dir', staged.luaDir,
      '--enable-strategy', pkg.name,
      '--backtest-report', reportPath,
      '--backtest-tick-ms', tickMs, '--backtest-tail-ms', '0',
      ...OPS_KNOBS,
    ];
    if (corpusSource.kind === 'archive') argv.push('--backtest-fast'); // frozen books: redundant per-tick maintenance off; instants unchanged
    const child = spawn(BIN, argv, {
      cwd: OUT_DIR,
      stdio: ['ignore', 'ignore', 'pipe'],
      env: { ...process.env, BLITZKRIEG_STRATEGY_ALLOW_DIRS: staged.luaDir },
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
      rmSync(staged.luaDir, { recursive: true, force: true });
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
      const invalid = invalidReason(stderr, pkg.name, selfCheck);
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
  const makerFeeEquiv = num(rep.makerRebateUsd) / 0.2; // rebate = 0.20 × taker-fee-equivalent
  const takerFees = num(t.feesUsd);
  const makerShare = makerFeeEquiv + takerFees > 0 ? makerFeeEquiv / (makerFeeEquiv + takerFees) : null;
  return {
    closed: num(t.closed), wins: num(t.wins), losses: num(t.losses), flat: num(t.flat),
    winRatePct: num(t.winRatePct),
    profitFactor: t.profitFactor == null ? null : num(t.profitFactor),
    grossProfitUsd: num(t.grossProfitUsd), grossLossUsd: num(t.grossLossUsd),
    netPnlUsd: num(t.netPnlUsd), feesUsd: takerFees,
    maxDrawdownUsd: num(t.maxDrawdownUsd),
    makerRebateUsd: num(rep.makerRebateUsd),
    makerShareEst: feeModel === 'official' ? makerShare : null,
    orders: rep.orders?.orders ?? 0, ordersFilled: rep.orders?.filled ?? 0,
    fills: num(rep.fills), fillRatePct: num(rep.fillStats?.fillRatePct),
    placed: num(stratRow.ordersPlaced), rejected: num(stratRow.ordersRejected),
    rejectionCauses: stratRow.rejectionCauses ?? {},
    blockedTiming: num(rep.blocked?.timing), blockedMomentum: num(rep.blocked?.momentum),
    openPositions: rep.openPositions ?? 0, openNotionalUsd: num(rep.openNotionalUsd),
    events: num(rep.sourceStats?.events), spotEvents: num(rep.sourceStats?.spotEvents),
    malformed: num(rep.sourceStats?.malformedLines), outOfOrder: num(rep.sourceStats?.outOfOrderEvents),
    startAtMs: rep.startAtMs, endAtMs: rep.endAtMs, virtualMs: num(rep.virtualMs),
    feeSchedule: rep.feeSchedule?.name,
  };
}

// ── run the matrix ───────────────────────────────────────────────────────────

console.log(`# smart-money-reverse replay — lines=${lines.join('+')} corpus=${corpusSource.kind}`);
console.log(`# fee-model=${feeModel} maker-rebate=${makerRebate} tick-ms=${tickMs}`);
console.log(`# out: ${OUT_DIR}`);

writeFileSync(join(OUT_DIR, 'plan.json'), JSON.stringify({
  generatedAtMs: Date.now(), lines, feeModel, makerRebate, tickMs, archiveDir: archiveDir ?? null,
  resolveFrom, corpusSource, arms: Object.fromEntries(lines.map((l) => [l, ARMS[l]])),
  windows: runWindows, opsKnobs: OPS_KNOBS,
}, null, 2) + '\n');

// Materialize the corpus ONCE (pinned windows verify here; archive slices are
// shared by every arm — an arm must never see different bytes than another).
for (const w of runWindows) {
  if (corpusSource.kind === 'frozen-pins') {
    await materializeFrozen(w);
    console.log(`  corpus ${w.name}: pinned window OK (${(corpusProvenance[w.name].lines).toLocaleString()} lines, sha verified)`);
  } else {
    console.log(`  corpus ${w.name}: slicing archive…`);
    let span;
    try { span = await sliceArchiveWindow(w); } catch (e) {
      console.error(`  corpus ${w.name}: ${e.message}`);
      process.exit(2);
    }
    let inj = { injected: 0, missingRound: 0, beyondEnd: 0, inLead: 0, total: 0 };
    if (resolveFrom.length) {
      const winners = await redeemWinners(span);
      inj = await injectResolutions(w, span, span.rounds, winners);
      console.log(`    resolutions: ${inj.injected} injected of ${inj.total} redeem-verdicts` +
        ` (no-round-line ${inj.missingRound}, beyond-window ${inj.beyondEnd}, pre-window ${inj.inLead})`);
    } else {
      // no back-fill: the raw slice IS the corpus; unsettled positions ride the core-dry ladder
      rmSync(join(SCRATCH, 'corpus', `${w.name}.jsonl`), { force: true });
      renameSync(span.rawPath, join(SCRATCH, 'corpus', `${w.name}.jsonl`));
      console.log(`    resolutions: none injected (--resolve-from absent) — unsettled positions ride the core-dry settlement ladder`);
    }
    const finalPath = join(SCRATCH, 'corpus', `${w.name}.jsonl`);
    const stat = statSync(finalPath);
    corpusProvenance[w.name] = {
      path: finalPath,
      sha256: await sha256File(finalPath),
      lines: span.lines, bytes: stat.size, rounds: span.rounds.size,
      resolutionsInjected: inj.injected, redeemVerdictsMissingRound: inj.missingRound,
      resolutionsBeyondWindow: inj.beyondEnd,
      source: `archive slice of ${archiveDir} (${new Date(span.t0 - LEAD_MS).toISOString()} → ${new Date(span.t1).toISOString()}, lead ${LEAD_MS / 60000}min)`,
    };
    console.log(`    corpus ${w.name}: ${corpusProvenance[w.name].lines.toLocaleString()} lines, ${(stat.size / 1e6).toFixed(1)} MB, ${span.rounds.size} rounds, sha ${corpusProvenance[w.name].sha256.slice(0, 12)}…`);
  }
}

const results = []; // { line, arm, window, metrics | error }
for (const line of lines) {
  for (const arm of ARMS[line]) {
    const armDir = join(OUT_DIR, arm.name);
    mkdirSync(armDir, { recursive: true });
    console.log(`\n# arm ${arm.name}  params=${JSON.stringify(arm.params)}`);
    for (const w of runWindows) {
      const reportPath = join(armDir, `${w.name}.json`);
      if (existsSync(reportPath)) {
        try {
          const cached = JSON.parse(readFileSync(reportPath, 'utf8'));
          console.log(`  ${w.name}: (resume) closed=${cached.trades?.closed ?? 0}`);
          results.push({ line, arm: arm.name, params: arm.params, window: w.name, metrics: extractMetrics(cached) });
          continue;
        } catch { /* stale report — rerun */ }
      }
      const corpusPathForRun = join(SCRATCH, 'corpus', `${w.name}.jsonl`);
      if (!existsSync(corpusPathForRun)) {
        console.error(`  ${w.name}: MISSING corpus ${corpusPathForRun} — materialization failed earlier; refusing to fake a zero`);
        results.push({ line, arm: arm.name, params: arm.params, window: w.name, error: 'corpus missing' });
        continue;
      }
      const r = await runOne(line, arm, w, corpusPathForRun, reportPath);
      if (r.error) {
        console.log(`  ${w.name}: ERROR — ${r.error}`);
        results.push({ line, arm: arm.name, params: arm.params, window: w.name, error: r.error });
        continue;
      }
      const m = extractMetrics(r.report);
      results.push({ line, arm: arm.name, params: arm.params, window: w.name, metrics: m });
      console.log(`  ${w.name}: closed=${m.closed} WR=${r2(m.winRatePct)}% PF=${r4(m.profitFactor)} net=${r4(m.netPnlUsd)} fees=${r4(m.feesUsd)} rebate=${r4(m.makerRebateUsd)} open=${m.openPositions}`);
    }
  }
}

// ── per-arm table ────────────────────────────────────────────────────────────

const mkCol = (m, key) => (m.error ? 'ERROR' : m.metrics[key]);
console.log('\n# per-arm table (sums over windows; PF from summed gross P/L)');
const header = ['arm', 'params', 'windows', 'closed', 'wins', 'WR%', 'PF', 'net USD', 'fees USD', 'rebate USD', 'maker:taker est', 'fills', 'orders(filled/placed)', 'open@end', 'events'];
console.log(header.join('  '));
const summary = {};
for (const line of lines) {
  for (const arm of ARMS[line]) {
    const rows = results.filter((r) => r.arm === arm.name);
    const ok = rows.filter((r) => !r.error);
    const s = {
      windows: rows.length, errors: rows.length - ok.length,
      closed: ok.reduce((a, r) => a + r.metrics.closed, 0),
      wins: ok.reduce((a, r) => a + r.metrics.wins, 0),
      grossProfitUsd: ok.reduce((a, r) => a + r.metrics.grossProfitUsd, 0),
      grossLossUsd: ok.reduce((a, r) => a + r.metrics.grossLossUsd, 0),
      netPnlUsd: ok.reduce((a, r) => a + r.metrics.netPnlUsd, 0),
      feesUsd: ok.reduce((a, r) => a + r.metrics.feesUsd, 0),
      makerRebateUsd: ok.reduce((a, r) => a + r.metrics.makerRebateUsd, 0),
      fills: ok.reduce((a, r) => a + r.metrics.fills, 0),
      ordersFilled: ok.reduce((a, r) => a + r.metrics.ordersFilled, 0),
      placed: ok.reduce((a, r) => a + r.metrics.placed, 0),
      openPositions: ok.reduce((a, r) => a + r.metrics.openPositions, 0),
      events: ok.reduce((a, r) => a + r.metrics.events, 0),
      blockedTiming: ok.reduce((a, r) => a + r.metrics.blockedTiming, 0),
      blockedMomentum: ok.reduce((a, r) => a + r.metrics.blockedMomentum, 0),
    };
    s.winRatePct = s.closed ? (100 * s.wins) / s.closed : 0;
    s.profitFactor = s.grossLossUsd > 0 ? s.grossProfitUsd / s.grossLossUsd : null;
    const makerFeeEquiv = s.makerRebateUsd / 0.2;
    s.makerTakerEst = feeModel === 'official' && makerFeeEquiv + s.feesUsd > 0
      ? `${Math.round((100 * makerFeeEquiv) / (makerFeeEquiv + s.feesUsd))}:${Math.round((100 * s.feesUsd) / (makerFeeEquiv + s.feesUsd))}`
      : 'n/a';
    summary[arm.name] = s;
    console.log(
      [
        arm.name, JSON.stringify(arm.params), `${s.windows}(${s.errors} err)`, s.closed, s.wins, r2(s.winRatePct),
        r4(s.profitFactor), r4(s.netPnlUsd), r4(s.feesUsd), r4(s.makerRebateUsd), s.makerTakerEst,
        s.fills, `${s.ordersFilled}/${s.placed}`, s.openPositions, s.events,
      ].join('  '),
    );
  }
}

writeFileSync(join(OUT_DIR, 'results.json'), JSON.stringify({
  generatedAtMs: Date.now(), plan: 'plan.json', corpusProvenance, feeModel, makerRebate,
  summary, results,
}, null, 2) + '\n');

const errCount = results.filter((r) => r.error).length;
console.log(`\n# reports: ${OUT_DIR}/<arm>/<window>.json; results.json + plan.json alongside`);
if (!keepCorpus && corpusSource.kind === 'archive') {
  // slices are large scratch; provenance (sha256) is recorded above
  for (const w of runWindows) rmSync(join(SCRATCH, 'corpus', `${w.name}.jsonl`), { force: true });
}
if (errCount) {
  console.error(`\nINVALID/FAILED runs: ${errCount}. Their rows are marked ERROR and excluded from every sum — fix the cause before reading any zero as "no signal".`);
  process.exitCode = 1;
}
