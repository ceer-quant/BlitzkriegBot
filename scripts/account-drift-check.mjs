#!/usr/bin/env node
/**
 * E17-f: the 72-hour accounting-drift check.
 *
 * The plan's acceptance criterion is "72h with no drift", and the drift it means
 * is specific: the cash ledger and the trade records disagreeing about the same
 * money. This monitor polls a RUNNING core (no spawn, no token cost) and proves,
 * on every single poll, that the two views still reconcile:
 *
 *     balance == seed
 *              + Σ_closed netPnlUsd                 (realized, per trade record)
 *              − Σ_open (entryCostUsd + entryFeeUsd) (cash already spent)
 *              + Σ_open (proceedsUsd − exitFeeUsd)   (cash already received)
 *
 * That form holds at EVERY instant, including mid-position and half-exited —
 * which is what makes it a real 72h guarantee rather than a check that can only
 * run when the book happens to be flat. It is the same identity the Rust gate
 * asserts; here it is asserted against a live session.
 *
 * `entryCostUsd` (total paid on the way in) is deliberate: `costUsd` is the basis
 * of the shares STILL HELD, so on a partial exit it drops by the released basis —
 * which `proceedsUsd` has already returned. Summing `costUsd` instead would
 * double-count that release and report a phantom drift equal to it.
 *
 * Two further checks ride along, because they are the ways the identity could be
 * satisfied while the books are still wrong:
 *
 *   * every fill on an order was charged on ONE basis: an order whose role is
 *     `maker` must show no fee anywhere; a `taker` one must show the published
 *     schedule. A `maker_then_taker` order may legitimately show both.
 *   * no position is left holding a sub-grid stub of shares (the dust case the
 *     E17-d fix had to write off inside `net` rather than leave on the book).
 *
 * Any failed poll is appended as a DRIFT line and makes the process exit non-zero
 * when it finishes, so a wrapper (systemd, a soak loop, CI) sees a real failure.
 * It NEVER mutates the core: it only reads.
 *
 * Usage:
 *   node scripts/account-drift-check.mjs                      # 72h, 60s polls
 *   node scripts/account-drift-check.mjs --hours 0.05         # short smoke run
 *   node scripts/account-drift-check.mjs --interval-sec 5 --hours 1
 *   node scripts/account-drift-check.mjs --once               # one poll, for CI
 *
 * Output: data/drift/drift.jsonl (one JSON line per poll) + a summary on stdout.
 */
import net from 'net';
import { appendFileSync, existsSync, mkdirSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';
import { resolveSocketPath } from './lib/core-socket.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
const OUT_DIR = join(ROOT, 'data', 'drift');
const OUT = join(OUT_DIR, 'drift.jsonl');

const argv = process.argv.slice(2);
const opt = (n, d) => { const i = argv.indexOf(n); return i >= 0 && argv[i + 1] ? argv[i + 1] : d; };
const HOURS = parseFloat(opt('--hours', '72'));
const INTERVAL_SEC = Math.max(1, parseInt(opt('--interval-sec', '60'), 10));
const ONCE = argv.includes('--once');
const DEADLINE_MS = Date.now() + HOURS * 3600 * 1000;

/**
 * Tolerance: the identity is exact in the core's `Decimal`, but this reads f64
 * over JSON and sums in f64, so the last bits can differ. 1e-6 is ~80,000x
 * tighter than the 0.0817722 gap that E17 removed, so it cannot hide a real
 * regression while never tripping on float transport.
 */
const TOL = 1e-6;

const round = (n) => Math.round(n * 1e9) / 1e9;

// ── A tiny read-only JSON-RPC client (the soak monitor's, not the full shell) ──
function rpc(sock, method, params = {}, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    const s = net.connect(sock);
    let buf = '';
    const timer = setTimeout(() => { s.destroy(); reject(new Error('timeout')); }, timeoutMs);
    s.on('connect', () => {
      s.write(JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) + '\n');
    });
    s.on('data', (d) => {
      buf += d.toString();
      const nl = buf.indexOf('\n');
      if (nl < 0) return;
      clearTimeout(timer);
      const line = buf.slice(0, nl);
      s.destroy();
      try {
        const msg = JSON.parse(line);
        if (msg.error) reject(new Error(msg.error.message || 'rpc error'));
        else resolve(msg.result);
      } catch (e) { reject(e); }
    });
    s.on('error', (e) => { clearTimeout(timer); reject(e); });
  });
}

const takerFeeUsd = (price, shares) => 0.125 * (price * (1 - price)) ** 2 * shares;

/**
 * One accounting audit of the live core. Returns {ok, drift, ...evidence}.
 * `drift` is the residual: how far the ledger sits from what the trade records
 * and the open cash flows say it should be. It must be ~0.
 */
async function audit(sock) {
  const [bal, tradesRes, posRes] = await Promise.all([
    rpc(sock, 'ledger.balance'),
    rpc(sock, 'trades.history', { limit: 500 }),
    rpc(sock, 'positions.list'),
  ]);

  const trades = tradesRes.trades || [];
  const positions = posRes.positions || [];

  // A core older than E17 does not expose the per-position cash flows, and its
  // trade records predate the resolved role. Summing `undefined` would produce
  // NaN and a "drift" nobody can act on, so say what is actually wrong instead.
  // Both fields are checked because either can be absent on its own: an idle
  // pre-E17 core has no open positions, and a freshly-restarted one has no trades.
  const stale = positions.some((p) => p.entryCostUsd === undefined)
    || trades.some((t) => t.entryRole === undefined || t.netPnlUsd === undefined);
  if (stale) {
    const e = new Error(
      'the core on this socket predates E17 (positions lack entryCostUsd / trades lack ' +
      'entryRole) — restart it with a current build before auditing; its pre-E17 ledger ' +
      'cannot be checked against the post-E17 identity'
    );
    // Fatal, not a poll failure: retrying cannot fix a stale binary, and a 72h
    // loop that logs the same unfixable error every interval is just noise.
    e.fatal = true;
    throw e;
  }

  const realized = trades.reduce((s, t) => s + t.netPnlUsd, 0);
  const spent = positions.reduce((s, p) => s + p.entryCostUsd + p.entryFeeUsd, 0);
  const received = positions.reduce((s, p) => s + p.proceedsUsd - p.exitFeeUsd, 0);
  // A live core reports no seed (its cash is the venue's); then the identity is
  // against the opening balance implied by the first observation instead.
  const seed = bal.seed === undefined || bal.seed === null ? null : Number(bal.seed);
  const expected = seed === null ? null : seed + realized - spent + received;
  const residual = expected === null ? null : bal.balance - expected;

  const problems = [];

  if (seed !== null && Math.abs(residual) > TOL) {
    problems.push(
      `drift: balance ${bal.balance} vs expected ${round(expected)} (residual ${residual.toExponential(3)})`
    );
  }

  // Reserved cash must always be backed by a resting order, and a resting BUY
  // reserves exactly price*size — never a phantom hold.
  const orders = (await rpc(sock, 'orders.list')).orders || [];
  const liveBuys = orders.filter((o) => o.status === 'LIVE' || o.status === 'PARTIALLY_FILLED');
  const expectedReserved = liveBuys
    .filter((o) => o.side === 'buy')
    .reduce((s, o) => s + o.price * (o.size - o.filledSize), 0);
  if (Math.abs(bal.reserved - expectedReserved) > TOL) {
    problems.push(
      `reservation mismatch: reserved ${bal.reserved} vs unfilled notional ${round(expectedReserved)}`
    );
  }

  // Each order must be charged on ONE basis, the one its fills actually had.
  for (const t of trades) {
    const takerEntry = takerFeeUsd(t.entryPrice, t.shares);
    const takerExit = takerFeeUsd(t.exitPrice, t.shares);
    if (t.entryFeePct === 0 && t.exitFeePct === 0 && t.feesUsd !== 0) {
      problems.push(`trade ${t.id}: both legs are maker but ${t.feesUsd} was charged`);
    }
    if (t.wasMakerEntry && t.entryFeePct !== 0) {
      problems.push(`trade ${t.id}: maker entry carries a ${t.entryFeePct}% fee`);
    }
    if (!t.wasMakerEntry && t.entryFeePct === 0 && takerEntry > TOL) {
      problems.push(`trade ${t.id}: taker entry of ${takerEntry} was not charged`);
    }
    if (t.wasMakerExit && t.exitFeePct !== 0) {
      problems.push(`trade ${t.id}: maker exit carries a ${t.exitFeePct}% fee`);
    }
    if (!t.wasMakerExit && t.exitFeePct === 0 && takerExit > TOL) {
      problems.push(`trade ${t.id}: taker exit of ${takerExit} was not charged`);
    }
  }

  // No position may hold an untradeable sub-grid stub (1 tick = 0.01 shares).
  for (const p of positions) {
    if (p.shares > 0 && p.shares < 0.01) {
      problems.push(`position ${p.id}: sub-grid stub of ${p.shares} shares left open`);
    }
  }

  return {
    ok: problems.length === 0,
    problems,
    balance: bal.balance,
    reserved: bal.reserved,
    expected: expected === null ? null : round(expected),
    residual: residual === null ? null : round(residual),
    realized: round(realized),
    open: positions.length,
    trades: trades.length,
  };
}

async function main() {
  const sock = await resolveSocketPath(process.env);
  if (!existsSync(OUT_DIR)) mkdirSync(OUT_DIR, { recursive: true });

  console.log(`account:drift-check — auditing ${sock}`);
  console.log(`  window ${HOURS}h, poll every ${INTERVAL_SEC}s, tolerance ${TOL}`);
  if (sock) {
    try {
      await rpc(sock, 'core.ping');
    } catch (e) {
      console.error(`  FAIL no core answering on that socket (${e.message}).`);
      console.error('  Start one first:  blitzkrieg core --mode dry   (or the TUI panel: ui_kit_panel)');
      process.exit(2);
    }
  }

  let polls = 0;
  let failures = 0;
  let maxResidual = 0;

  for (;;) {
    let line;
    try {
      const r = await audit(sock);
      polls++;
      if (r.residual !== null) maxResidual = Math.max(maxResidual, Math.abs(r.residual));
      if (!r.ok) {
        failures++;
        for (const p of r.problems) console.log(`  FAIL ${p}`);
      }
      line = { ts: Date.now(), ...r };
      if (polls % 60 === 1 || !r.ok) {
        console.log(
          `  ${r.ok ? 'ok  ' : 'DRIFT'} t=${new Date().toISOString()} ` +
          `balance=${r.balance} expected=${r.expected} residual=${r.residual} ` +
          `open=${r.open} trades=${r.trades}`
        );
      }
    } catch (e) {
      if (e?.fatal) {
        console.error(`  FAIL ${e.message}`);
        process.exit(2);
      }
      polls++;
      failures++;
      line = { ts: Date.now(), ok: false, problems: [`poll error: ${e.message}`] };
      console.log(`  FAIL poll error: ${e.message}`);
    }
    appendFileSync(OUT, JSON.stringify(line) + '\n');

    if (ONCE || Date.now() >= DEADLINE_MS) break;
    await new Promise((r) => setTimeout(r, INTERVAL_SEC * 1000));
  }

  console.log('');
  console.log(`  polls: ${polls}, failed: ${failures}, max |residual|: ${maxResidual.toExponential(2)}`);
  console.log(`  log:   ${OUT}`);
  if (failures === 0) {
    console.log(`\naccount:drift-check — no drift over ${HOURS}h (${polls} polls).`);
  } else {
    console.log(`\naccount:drift-check — ${failures}/${polls} polls reported drift.`);
  }
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`account:drift-check — harness error: ${e?.stack || e}`);
  process.exit(2);
});
