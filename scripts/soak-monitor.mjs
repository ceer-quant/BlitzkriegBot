#!/usr/bin/env node
/**
 * DRY-RUN soak monitor.
 *
 * Polls the running Blitzkrieg core over its UDS socket (JSON-RPC, no spawn) and
 * appends one JSON line per cycle to data/soak/soak.jsonl, plus a human summary
 * to data/soak/soak.log. Flags anomalies (core down, ping failure, stuck round,
 * frozen feed, new errors) so a long unattended run is auditable afterwards.
 *
 * Two lifetime modes, and the difference matters to the health check:
 *
 *   * BOUNDED (default) — `--hours 12` then exit 0. This is a soak monitor for
 *     one soak; exiting is its designed end, not a failure. `soak-health.sh`
 *     judges sampling by the newest sample's timestamp rather than by `pgrep`
 *     for exactly this reason: a bounded run that has ended looks exactly like a
 *     crashed sampler to a process check.
 *   * RESIDENT (`--forever`, D-30) — for a long-running deployment, where a
 *     12h window turned "is sampling happening?" into an unanswerable question
 *     with a moving deadline. SIGTERM (or SIGINT) is the stop switch; the
 *     process then reports that it was stopped rather than that the soak
 *     completed, so the two endings stay distinguishable in the log.
 *
 * Usage:  node scripts/soak-monitor.mjs [--hours 12] [--interval-sec 600]
 *         node scripts/soak-monitor.mjs --forever [--interval-sec 600]
 */

import { readFileSync, appendFileSync, existsSync, mkdirSync, statSync } from 'fs';
import { join, dirname, resolve } from 'path';
import { fileURLToPath } from 'url';
import { execSync } from 'child_process';
import { resolveSocketPath } from './lib/core-socket.mjs';
import { requestOnce } from './lib/core-client.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
// Same seam name as soak-health.sh, so a harness (or an operator with a
// non-default deployment) can move the soak triplet together instead of having
// one of the three write into the real data/ directory.
const SOAK_DIR = process.env.BK_SOAK_DIR ? resolve(ROOT, process.env.BK_SOAK_DIR) : join(ROOT, 'data', 'soak');
// The core writes its stdout to /dev/null and inherits stderr, so where its log
// lands is a *deployment* choice, not a repo fact. Default to run.log for
// backward compatibility with the redirects that still use it, but let the
// operator point at the real file (`BK_RUN_LOG`, same variable soak-health.sh
// reads) and treat "no log to scan" as an explicit state rather than as zero
// errors — KI-30: this file used to count errors in a log that no longer
// existed and report `err+0`, a check that could never fire.
const RUN_LOG = process.env.BK_RUN_LOG ? resolve(ROOT, process.env.BK_RUN_LOG) : join(ROOT, 'run.log');

const argv = process.argv.slice(2);
const opt = (n, d) => { const i = argv.indexOf(n); return i >= 0 && argv[i + 1] ? argv[i + 1] : d; };
const HOURS = parseFloat(opt('--hours', '12'));
const INTERVAL_SEC = parseInt(opt('--interval-sec', '600'), 10);
// Resident mode must be ASKED for. A typo'd `--hours abc` parses to NaN, and
// `Math.round(NaN)` would make the cycle loop run zero times: a monitor that
// samples nothing and exits 0, which is precisely the KI-30 defect (a check that
// can never fire) wearing a scheduler's clothes. So an unparseable/negative
// horizon is a loud argument error, never a silent mode switch.
const FOREVER = argv.includes('--forever');
if (!FOREVER && !(HOURS > 0)) {
  console.error(`invalid --hours ${JSON.stringify(opt('--hours', ''))}: use a positive number of hours, or --forever`);
  process.exit(2);
}
if (!(INTERVAL_SEC > 0)) {
  console.error(`invalid --interval-sec ${JSON.stringify(opt('--interval-sec', ''))}: use a positive number of seconds`);
  process.exit(2);
}

const SOCK = resolveSocketPath();

function ensureDir() { if (!existsSync(SOAK_DIR)) mkdirSync(SOAK_DIR, { recursive: true }); }

function pgrep(pattern) {
  try {
    const out = execSync(`pgrep -f ${JSON.stringify(pattern)} || true`).toString().trim();
    return out ? out.split('\n').map((x) => x.trim()).filter(Boolean) : [];
  } catch { return []; }
}

function rssKb(pid) {
  try {
    const out = execSync(`ps -o rss= -p ${pid}`).toString().trim();
    return parseInt(out, 10) || 0;
  } catch { return 0; }
}

/**
 * A sample, not an assertion: this monitor's whole job is to RECORD a core that
 * is down or wedged, so a refused or unanswered call comes back as `{ error }`
 * and lands in the sample as an anomaly instead of ending the soak.
 */
const rpc = async (method, params = {}, timeoutMs = 3000) => {
  try {
    return { result: await requestOnce(SOCK, method, params, { timeoutMs }) };
  } catch (e) {
    return { error: e.message, coreCode: e.coreCode };
  }
};

// Track run.log read offset to count NEW errors per cycle.
let logOffset = 0;
let logScanned = false;
try {
  logOffset = statSync(RUN_LOG).size;
  logScanned = true;
} catch {
  logScanned = false; // no log at RUN_LOG: reported, never silently zero
}

/**
 * Count new ERROR-ish lines in the configured log since last cycle (ignoring
 * known noise). Returns `scanned` so callers can tell "no errors found" apart
 * from "there was nothing to scan" — the distinction KI-30 was missing.
 */
function newErrors() {
  let text = '';
  try {
    const size = statSync(RUN_LOG).size;
    if (size < logOffset) logOffset = 0; // rotated
    const fd = readFileSync(RUN_LOG);
    text = fd.slice(logOffset).toString();
    logOffset = size;
    logScanned = true;
  } catch {
    // Keep the previous `logScanned` value: a log that was there a cycle ago and
    // is gone now is a transition worth surfacing, not a silent reset to clean.
    return { count: 0, samples: [], scanned: logScanned };
  }
  const IGNORE = /Manifold|PredictIt|Solana|wallet credentials|errorRate|errorPct/i;
  const lines = text.split('\n').filter((l) => /ERROR|\bpanic\b|ALERT/.test(l) && !IGNORE.test(l));
  return { count: lines.length, samples: lines.slice(0, 3).map((l) => l.slice(0, 160)), scanned: true };
}

function fmt(ts) { return new Date(ts).toISOString().replace('T', ' ').slice(0, 19); }

async function cycle(i, state) {
  const ts = Date.now();
  const corePids = pgrep('blitzkrieg-core');
  const ping = await rpc('core.ping');
  const stats = await rpc('engine.stats');
  const round = await rpc('engine.round');
  const orders = await rpc('orders.list');
  const pos = await rpc('positions.list');
  const errs = newErrors();

  const rec = {
    cycle: i,
    ts,
    coreAlive: corePids.length > 0,
    pingOk: Boolean(ping.result?.pong),
    coreRssMb: corePids[0] ? Math.round(rssKb(corePids[0]) / 1024) : 0,
    round: round.result ? { slot: round.result.slot, timeLeftSec: round.result.timeLeftSec, markets: round.result.markets, canTrade: round.result.canTrade } : null,
    stats: stats.result ? {
      books: stats.result.books, spots: stats.result.spots, rounds: stats.result.rounds,
      evaluations: stats.result.evaluations, signals: stats.result.signals,
      confirmed: (stats.result.confirmed || []).length,
      blockedTiming: stats.result.blocked?.timing ?? null,
      blockedMomentum: stats.result.blocked?.momentum ?? null,
    } : null,
    orders: orders.result ? orders.result.orders.length : null,
    liveOrders: orders.result ? orders.result.orders.filter((o) => ['LIVE', 'PENDING', 'PARTIALLY_FILLED'].includes(o.status)).length : null,
    positions: pos.result ? pos.result.positions.length : null,
    newErrors: errs.count,
    errorSamples: errs.samples,
    logScanned: errs.scanned,
    anomalies: [],
  };

  // Anomaly detection.
  if (!rec.coreAlive) rec.anomalies.push('core_process_down');
  if (!rec.pingOk) rec.anomalies.push('ipc_ping_failed');
  if (rec.newErrors > 0) rec.anomalies.push(`errors:+${rec.newErrors}`);
  // A log that was never scanned (no path configured, or the path is gone) is
  // NOT evidence of zero errors. Reporting it as `err+0` is a false negative of
  // the same kind as KI-30's hardcoded `run.log` scans: the check looks present
  // and can never fire. Say so instead.
  if (!rec.logScanned) rec.anomalies.push('log_not_scanned');
  // Stuck round: slot unchanged for > 2 cycles.
  if (state.lastSlot != null && rec.round && rec.round.slot === state.lastSlot) {
    state.sameSlot = (state.sameSlot || 0) + 1;
    if (state.sameSlot >= 3) rec.anomalies.push(`round_stuck(slot=${rec.round.slot})`);
  } else { state.sameSlot = 0; }
  if (rec.round) state.lastSlot = rec.round.slot;
  // Frozen feed: books/spots not increasing across a cycle while markets exist.
  if (state.lastBooks != null && rec.stats && rec.round?.markets > 0 && rec.stats.books === state.lastBooks) {
    rec.anomalies.push('feed_books_not_increasing');
  }
  if (rec.stats) { state.lastBooks = rec.stats.books; }
  if (!rec.coreAlive && state.coreDownSince == null) state.coreDownSince = ts;
  if (rec.coreAlive) state.coreDownSince = null;

  ensureDir();
  appendFileSync(join(SOAK_DIR, 'soak.jsonl'), JSON.stringify(rec) + '\n');
  const tag = rec.anomalies.length ? `⚠ ${rec.anomalies.join(',')}` : 'ok';
  const line = `[${fmt(ts)}] #${String(i).padStart(3)} core=${rec.coreAlive ? 'up' : 'DOWN'} ` +
    `ping=${rec.pingOk ? 'ok' : 'FAIL'} slot=${rec.round?.slot ?? '-'} tLeft=${rec.round?.timeLeftSec ?? '-'} mkt=${rec.round?.markets ?? '-'} ` +
    `books=${rec.stats?.books ?? '-'} spots=${rec.stats?.spots ?? '-'} rx=${rec.stats?.evaluations ?? '-'} sig=${rec.stats?.signals ?? '-'} ` +
    `ord=${rec.orders ?? '-'}(live ${rec.liveOrders ?? '-'}) pos=${rec.positions ?? '-'} rss=${rec.coreRssMb}MB ` +
    `err+${rec.logScanned ? rec.newErrors : 'n/a'} ${tag}`;
  appendFileSync(join(SOAK_DIR, 'soak.log'), line + '\n');
  console.log(line);
  return rec;
}

(async () => {
  // Bounded by default. `Math.max(1, …)` is what guarantees a finite run even for
  // `--hours 0`, so the resident mode has to be an explicit, separate decision —
  // a monitor that silently became immortal would make the health check's
  // sampling judgement depend on a process name again (KI-30).
  const cycles = FOREVER ? Infinity : Math.max(1, Math.round((HOURS * 3600) / INTERVAL_SEC));

  // Stop switch (D-30). Without this a resident sampler could only be stopped by
  // SIGKILL, which loses the final line and leaves soak.log unable to say why it
  // ended. `stopping` is also read by the sleep below so a TERM is honoured
  // within a second instead of after the current 600s interval.
  let stopping = false;
  let stopSignal = null;
  const onStop = (sig) => { stopping = true; stopSignal = sig; };
  process.on('SIGTERM', () => onStop('SIGTERM'));
  process.on('SIGINT', () => onStop('SIGINT'));

  ensureDir();
  console.log(`soak monitor: ${FOREVER ? 'resident (--forever)' : `${HOURS}h`}, every ${INTERVAL_SEC}s` +
    (FOREVER ? ' — stop with SIGTERM/SIGINT' : ` → ${cycles} cycles`));
  console.log(`socket: ${SOCK}`);
  console.log(`logs:   data/soak/soak.{jsonl,log}`);
  const state = {};
  let worst = { anomalies: 0 };
  let ran = 0;
  for (let i = 1; i <= cycles && !stopping; i++) {
    try {
      const rec = await cycle(i, state);
      ran = i;
      if (rec.anomalies.length > worst.anomalies) worst = rec;
    } catch (e) {
      appendFileSync(join(SOAK_DIR, 'soak.log'), `[${fmt(Date.now())}] #${i} monitor error: ${e?.message || e}\n`);
    }
    if (i < cycles && !stopping) {
      // 1s slices: a TERM during the interval must not have to wait out the
      // whole interval. `stopSignal` is what distinguishes "stopped on purpose"
      // from "finished the soak" in the closing line.
      let slept = 0;
      while (slept < INTERVAL_SEC && !stopping) {
        await new Promise((r) => setTimeout(r, 1000));
        slept += 1;
      }
    }
  }
  const summary = stopping
    ? `\n=== SOAK STOPPED (${stopSignal}) after cycle ${ran} ===\nworst cycle anomalies: ${worst.anomalies} ${worst.anomalies ? JSON.stringify(worst) : ''}\n`
    : `\n=== SOAK COMPLETE (${HOURS}h) ===\nworst cycle anomalies: ${worst.anomalies} ${worst.anomalies ? JSON.stringify(worst) : ''}\n`;
  appendFileSync(join(SOAK_DIR, 'soak.log'), summary);
  console.log(summary);
  process.exit(0);
})();
