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
 * 口径 SYNC (issue #189): the kernel now runs this SAME anchored identity on its
 * own maintenance tick (`Core::run_accounting_audit`, 30s default) and blocks new
 * entries when it fails — see `core/blitzkrieg_core/src/reconcile.rs`. This script
 * stays the OUT-OF-PROCESS cross-check (it also covers a session whose kernel is
 * wedged, and the fill-role / dust legs below): the two must agree, so a change to
 * `expected_after` there or to the sums here has to be mirrored in the other.
 *
 * A LIVE core reports no seed (its cash is the venue's), so its identity is the
 * same equation shifted by the previous poll — telescoping the unknown opening
 * cash out entirely. From the second poll on, every poll asserts:
 *
 *     balance − anchor.balance
 *       == (realized − anchor.realized)     trade records say
 *        − (spent   − anchor.spent)         cash the open positions consumed
 *        + (received − anchor.received)     cash they already got back
 *
 * This is exactly as strong over a 72h window (the deltas chain back to the very
 * first observation) and it is honest about what it cannot check: a manual fill
 * the core folded in from reconcile moves real cash without a trade record, and
 * THAT is precisely what this delta is designed to surface.
 *
 * Realized is taken from `trades.summary.totalNetPnl` — the all-time running
 * total the core updates on every close — not from summing the history window,
 * which truncates at 500 rows and would report a phantom drift of the evicted
 * rows' net the moment the window rolled.
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
 * A drift is only reported once it PERSISTS: each failed poll is re-audited five
 * seconds later against the same anchor, and only a second failure counts. The
 * four concurrent reads are not one transaction, so a fill landing between them
 * can produce a one-poll phantom — that self-corrects and must not alarm a
 * wrapper. A drift that survives the recheck is appended as a DRIFT line and
 * makes the process exit non-zero when it finishes, so a wrapper (systemd, a
 * soak loop, CI) sees a real failure. It NEVER mutates the core: it only reads.
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
 * One accounting audit of the core. Returns {ok, residual, ...evidence}.
 * `residual` is how far the ledger sits from what the trade records and the
 * open cash flows say it should be. It must be ~0. `anchor` is the previous
 * audit's result (or null on the very first poll), which gives live cores their
 * identity baseline.
 */
async function audit(sock, anchor) {
  const [bal, tradesRes, posRes, sumRes] = await Promise.all([
    rpc(sock, 'ledger.balance'),
    rpc(sock, 'trades.history', { limit: 500 }),
    rpc(sock, 'positions.list'),
    rpc(sock, 'trades.summary'),
  ]);

  const trades = tradesRes.trades || [];
  const positions = posRes.positions || [];

  // A core older than E17 does not expose the per-position cash flows, and its
  // trade records predate the resolved role. Summing `undefined` would produce
  // NaN and a "drift" nobody can act on, so say what is actually wrong instead.
  // Rows carry the resolved role as `wasMakerEntry/wasMakerExit` (the camelCase
  // booleans); `entryRole` is a ClosedPosition field that is never serialized
  // into history rows, so the core is stale only if a row carries NEITHER
  // spelling. Each of the other fields can be absent on its own: an idle
  // pre-E17 core has no open positions, and a freshly-restarted one has no trades.
  const stale = positions.some((p) => p.entryCostUsd === undefined)
    || trades.some(
      (t) =>
        t.netPnlUsd === undefined ||
        (t.entryRole === undefined && t.wasMakerEntry === undefined)
    );
  if (stale) {
    const e = new Error(
      'the core on this socket predates E17 (positions lack entryCostUsd / trades lack ' +
      'netPnlUsd+wasMakerEntry) — restart it with a current build before auditing; its ' +
      'pre-E17 ledger cannot be checked against the post-E17 identity'
    );
    // Fatal, not a poll failure: retrying cannot fix a stale binary, and a 72h
    // loop that logs the same unfixable error every interval is just noise.
    e.fatal = true;
    throw e;
  }

  const summary = sumRes?.summary ?? null;
  const realized = summary
    ? Number(summary.totalNetPnl ?? 0)
    : trades.reduce((s, t) => s + Number(t.netPnlUsd ?? 0), 0);
  const spent = positions.reduce(
    (s, p) => s + Number(p.entryCostUsd ?? 0) + Number(p.entryFeeUsd ?? 0),
    0
  );
  const received = positions.reduce(
    (s, p) => s + Number(p.proceedsUsd ?? 0) - Number(p.exitFeeUsd ?? 0),
    0
  );

  const balance = Number(bal.balance);
  const seed = bal.seed === undefined || bal.seed === null ? null : Number(bal.seed);
  // Seeded core (dry/read-only): the global form. Live core: the form shifted by
  // the previous observation, which is exact by subtraction of two globals.
  const expected = seed !== null
    ? seed + realized - spent + received
    : anchor === null
      ? null
      : anchor.balance
        + (realized - anchor.realized)
        - (spent - anchor.spent)
        + (received - anchor.received);
  const residual = expected === null ? null : balance - expected;

  const problems = [];

  if (expected !== null && Math.abs(residual) > TOL) {
    problems.push(
      `drift: balance ${balance} vs expected ${round(expected)} (residual ${residual.toExponential(3)})`
    );
  }

  // Reserved cash must always be backed by a resting order, and a resting BUY
  // reserves exactly price*size — never a phantom hold.
  const orders = (await rpc(sock, 'orders.list')).orders || [];
  const liveBuys = orders.filter((o) => o.status === 'LIVE' || o.status === 'PARTIALLY_FILLED');
  const expectedReserved = liveBuys
    .filter((o) => o.side === 'buy')
    .reduce((s, o) => s + Number(o.price) * (Number(o.size) - Number(o.filledSize)), 0);
  if (Math.abs(Number(bal.reserved) - expectedReserved) > TOL) {
    problems.push(
      `reservation mismatch: reserved ${bal.reserved} vs unfilled notional ${round(expectedReserved)}`
    );
  }

  // Each order must be charged on ONE basis, the one its fills actually had.
  for (const t of trades) {
    const takerEntry = takerFeeUsd(Number(t.entryPrice), Number(t.shares));
    const takerExit = takerFeeUsd(Number(t.exitPrice), Number(t.shares));
    if (t.entryFeePct === 0 && t.exitFeePct === 0 && Number(t.feesUsd) !== 0) {
      problems.push(`trade ${t.id}: both legs are maker but ${t.feesUsd} was charged`);
    }
    if (t.wasMakerEntry && Number(t.entryFeePct) !== 0) {
      problems.push(`trade ${t.id}: maker entry carries a ${t.entryFeePct}% fee`);
    }
    if (!t.wasMakerEntry && Number(t.entryFeePct) === 0 && takerEntry > TOL) {
      problems.push(`trade ${t.id}: taker entry of ${takerEntry} was not charged`);
    }
    if (t.wasMakerExit && Number(t.exitFeePct) !== 0) {
      problems.push(`trade ${t.id}: maker exit carries a ${t.exitFeePct}% fee`);
    }
    if (!t.wasMakerExit && Number(t.exitFeePct) === 0 && takerExit > TOL) {
      problems.push(`trade ${t.id}: taker exit of ${takerExit} was not charged`);
    }
  }

  // No position may hold an untradeable sub-grid stub (1 tick = 0.01 shares).
  for (const p of positions) {
    if (Number(p.shares) > 0 && Number(p.shares) < 0.01) {
      problems.push(`position ${p.id}: sub-grid stub of ${p.shares} shares left open`);
    }
  }

  return {
    ok: problems.length === 0,
    problems,
    balance,
    reserved: Number(bal.reserved),
    expected: expected === null ? null : round(expected),
    residual: residual === null ? null : round(residual),
    realized: round(realized),
    spent: round(spent),
    received: round(received),
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
  // Anchor for the live-core identity: the previous poll's result. The deltas
  // between consecutive results chain across the whole window, so the first
  // poll only establishes the baseline (its live identity is not yet defined).
  let prev = null;

  for (;;) {
    let line;
    try {
      const r = await audit(sock, prev);
      polls++;
      if (r.residual !== null) maxResidual = Math.max(maxResidual, Math.abs(r.residual));
      let reported = r;
      if (!r.ok) {
        // One retry after a short pause against the same anchor: the four reads
        // are not a transaction, so a fill landing between them can produce a
        // one-poll phantom. Only a drift that persists is real.
        await new Promise((res) => setTimeout(res, 5000));
        const retry = await audit(sock, prev);
        polls++;
        if (retry.residual !== null) maxResidual = Math.max(maxResidual, Math.abs(retry.residual));
        reported = retry;
        if (!retry.ok) {
          failures++;
          for (const p of retry.problems) console.log(`  FAIL ${p}`);
        } else {
          console.log(`  (transient fetch race self-corrected; not counted)`);
        }
      }
      prev = reported;
      line = { ts: Date.now(), ...reported };
      if (polls % 60 === 1 || !reported.ok) {
        console.log(
          `  ${reported.ok ? 'ok  ' : 'DRIFT'} t=${new Date().toISOString()} ` +
          `balance=${reported.balance} expected=${reported.expected} residual=${reported.residual} ` +
          `open=${reported.open} trades=${reported.trades}`
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
