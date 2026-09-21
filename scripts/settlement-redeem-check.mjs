#!/usr/bin/env node
/**
 * Settlement & redemption acceptance on the REAL binary (issue #175).
 *
 * A settled position is money the chain owes us: the market resolves, the
 * position closes at its payout, and the amount sits in a RECEIVABLE until a
 * redemption lands it in cash. #175 shipped that whole path (settle → receivable
 * → redeem → cash, idempotent across restarts) but no CI gate ever drove it, so
 * nothing outside the unit tests noticed if it stopped working. This gate drives
 * it end to end over the IPC wire, against a real dry-mode core, and asserts:
 *
 *   1. **No venue egress, structurally.** Dry mode never starts the venue order
 *      executor — even when well-formed live credentials are in the environment
 *      (the adversarial setup `scripts/readonly-egress-check.mjs` uses) — and
 *      every settlement it books says so in its own record: `source = core-dry`,
 *      redemption `txHash = dry-simulated`. No chain hash, no venue order id, in
 *      the process or in the durable journal.
 *   2. **The receivable is real money, and it becomes cash exactly once.** A
 *      position on the winning side settles at `shares × payoutPerShare`, the
 *      cash ledger moves by exactly that amount, and the claim leaves the
 *      receivable. With a redemption held back (the failure hook below) the same
 *      amount is visible as `receivableUsd` while the cash has NOT moved.
 *   3. **Idempotency is observable, not just claimed.** The claim is confirmed
 *      once: after a window longer than the retry backoff the balance has not
 *      moved again, the claim is gone from the book, and the durable journal
 *      holds exactly one `redeemed` line for it. A `note_confirmed` that stopped
 *      removing the claim would re-dispatch it every backoff and credit twice —
 *      that mutation is what this assertion exists to catch.
 *   4. **The retry clock and the stop rule.** A failed attempt is stamped at
 *      dispatch, so the claim is not re-dispatched while an attempt is in flight
 *      (`attempts` holds steady through the backoff window) — the core-side half
 *      of the live executor's `REDEEM_MAX_IN_FLIGHT`. The retry then lands after
 *      the backoff and credits once. A `manual` failure stops the automatic
 *      attempts for good (`nextAttemptMs = i64::MAX`, `retryableRedemptions = 0`,
 *      the receivable held) — the redemption-side counterpart of
 *      `SWEEP_FAILURE_FREEZE`'s "consecutive failures → stop", whose own
 *      threshold is pinned here through the read-only `engine.stats.reconcile`
 *      block.
 *
 * Failure injection uses `--dry-redeem-fail N` / `--dry-redeem-manual`, dry-only
 * test hooks that route a real `RedemptionFailure` through the production path
 * (`note_failure` → backoff → retry). Without them the retry and stop rules are
 * unreachable from outside, because in dry mode the simulated redemption always
 * succeeds; with them, `--dry-redeem-fail 0` (the default) is byte-for-byte
 * today's behaviour and live mode never reads them.
 *
 * Everything runs in a scratch dir on a private socket: no production data, no
 * network, dry mode only. Live is never reachable from here.
 *
 * Usage: node scripts/settlement-redeem-check.mjs
 *   (needs target/release/blitzkrieg-core built)
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { CoreClient, rpc } from './lib/core-client.mjs';
import { checkCoreProvenance, coreBinaryPath } from './lib/core-provenance.mjs';
import { requireFreshStrategyDylibs } from './lib/strategy-dylib-freshness.mjs';
import { mkdtempSync, readFileSync, existsSync, statSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';

const BIN = coreBinaryPath();
const ROUND_SEC = 10;
const SEED = 1000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!existsSync(BIN)) {
  console.error(`missing binary: ${BIN} (cargo build --release --workspace --locked)`);
  process.exit(2);
}

// The winning side is entered by `trend_follow` (the chase leg lifts a rising
// ask), and which token wins is decided by the core from the mirrored book — so
// the leg under test is a cdylib, and #207 applies here as everywhere.
requireFreshStrategyDylibs({ gate: 'settlement-redeem-check', require: ['trend_follow_strategy'] });

// Adversarial egress setup, copied from scripts/readonly-egress-check.mjs:
// well-formed fake credentials in the environment, inherited by every core this
// gate spawns. Dry mode must start no executor even with them present. Nothing
// here is a real secret and no network is used.
process.env.POLYMARKET_PRIVATE_KEY =
  '0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d';
process.env.POLYMARKET_FUNDER_ADDRESS = '0x0000000000000000000000000000000000000001';
process.env.CLOB_API_URL = 'http://127.0.0.1:1';

let failures = 0;
function check(name, cond, detail = '') {
  if (cond) console.log(`  ok   ${name}`);
  else {
    failures++;
    console.log(`  FAIL ${name}${detail ? ` ${detail}` : ''}`);
  }
}
const num = (v) => (v == null ? null : Number(v));
const round = (n) => Number(Number(n).toFixed(6));

/**
 * Wait until the captured stderr matches `re`, or `ms` elapse — then return
 * whatever was captured. The core writes its boot banner to stderr before it
 * announces READY over the socket, but those are different channels and the
 * reader can lag, so a single read at READY is a race: it is what made the
 * `socketMode` check red on one run and green on the next. Returning the buffer
 * on timeout (rather than throwing) keeps the failure message useful.
 *
 * Poll for the WHOLE thing you are about to assert, never for a prefix of it.
 * This function returns the instant `re` matches, so a prefix pattern hands the
 * caller a buffer that may hold only the first half of the fact it wants — and
 * the banner is written one format fragment per `write(2)` on unbuffered
 * stderr, so `socketMode=` can land one chunk before `0600` does.
 */
async function waitForStderr(ctx, re, ms = 3000) {
  const deadline = Date.now() + ms;
  for (;;) {
    const captured = ctx.stderr();
    if (re.test(captured) || Date.now() >= deadline) return captured;
    await sleep(50);
  }
}

/**
 * Boot one dry core, hand its context to `body`, always tear down.
 *
 * `extraArgs` is where a test hook goes. The order log is a real file in the
 * scratch dir on purpose: the settlement journal (`settlements.jsonl`) is derived
 * from its directory, and the journal is half the evidence below.
 */
async function session(tag, extraArgs, body) {
  const workdir = mkdtempSync(join(tmpdir(), `bk-settle-${tag}-`));
  const orderLog = join(workdir, 'orders.jsonl');
  const core = new CoreClient({
    binaryPath: BIN,
    socketPath: join(workdir, 'core.sock'),
    mode: 'dry',
    seedBalance: SEED,
    maxOrderNotional: 50,
    tickMs: 50,
    autoRestart: false,
    cwd: workdir,
    noTradeLog: true,
    noPositionLog: true,
    extraArgs: [
      '--engine', '--no-discovery', '--no-event-archive', '--order-log', orderLog,
      // A round this short is the only way to make the declared expiry arrive
      // inside a gate's patience: the position's expiry is its round slot's end,
      // so `--round-sec 10` puts it at most 10 s out.
      '--round-sec', String(ROUND_SEC),
      // Round-window gates off: the entry is priced and sized by rules this gate
      // is not about, and with ~10 s rounds the timing gate would refuse it.
      '--min-round-age', '0', '--min-time-left', '0',
      // The exit policy must not close the position before its expiry: the
      // settlement path is the subject, and a taken profit would leave nothing to
      // settle.
      '--no-auto-exits', '--max-positions', '99',
      '--max-order-notional-pct', '100',
      '--enable-strategy', 'trend_follow',
      ...extraArgs,
    ],
  });
  const events = [];
  core.onEvent = (e) => events.push(e);
  try {
    await core.start();
    return await body({ core, orderLog, workdir, events, stderr: () => core.lastStderr });
  } finally {
    await core.stop().catch(() => {});
  }
}

/**
 * Phase-lock to the start of a round slot, declare one BTC market whose expiry is
 * that slot's end, and drive `trend_follow` into the UP side: a rising book is
 * what the chase leg buys, and UP is the token the dry resolution pays when its
 * mid is above 0.5.
 *
 * Returns the entry once the fill is booked: `{ pos, cashAfterEntry }`.
 */
async function enterWinningPosition(core) {
  // Declaring mid-slot would leave too little runway before the expiry this gate
  // is waiting for.
  for (let i = 0; i < 100; i++) {
    if (Date.now() % (ROUND_SEC * 1000) < 800) break;
    await sleep(100);
  }
  const slot = Math.floor(Date.now() / 1000 / ROUND_SEC);
  await rpc.setMarkets(core, [{
    asset: 'BTC', conditionId: '0xc-BTC', questionId: '0xq-BTC',
    upTokenId: 'UP', downTokenId: 'DOWN', upPrice: 0.5, downPrice: 0.5,
    expiresAtMs: (slot + 1) * ROUND_SEC * 1000, roundSlot: slot,
    negRisk: true, question: 'BTC up/down',
  }]);
  for (let i = 0; i <= 14; i++) {
    const bid = Math.round((0.50 + (0.12 * i) / 14) * 100) / 100;
    await rpc.bookSnapshot(core, 'UP', [[bid, 100]], [[Math.round((bid + 0.01) * 100) / 100, 100]]);
    await sleep(40);
  }
  let pos = null;
  for (let i = 0; i < 100 && !pos; i++) {
    pos = ((await rpc.positions(core)).positions || [])[0] ?? null;
    if (!pos) await sleep(100);
  }
  if (!pos) return { pos: null, cashAfterEntry: null };
  return { pos, cashAfterEntry: num((await rpc.balance(core)).balance) };
}

/** Keep the winning side above 0.5: the dry resolution pays the highest mid. */
const holdWinningBook = (core) =>
  rpc.bookSnapshot(core, 'UP', [[0.60, 100]], [[0.61, 100]]);

/** Poll `pick(ctx)` until it returns something truthy, or give up. */
async function settle(core, pick, tries, stepMs = 200) {
  for (let i = 0; i < tries; i++) {
    const v = await pick(core);
    if (v) return v;
    await holdWinningBook(core).catch(() => {});
    await sleep(stepMs);
  }
  return null;
}

const settlementOf = async (core) => (await rpc.stats(core)).settlement || {};
const claimOf = async (core) => ((await settlementOf(core)).claims || [])[0] ?? null;

/** The durable journal lines, parsed. */
function journal(path) {
  if (!existsSync(path)) return [];
  return readFileSync(path, 'utf8')
    .split('\n')
    .filter((l) => l.trim())
    .map((l) => { try { return JSON.parse(l); } catch { return null; } })
    .filter(Boolean);
}
const journalAt = (ctx) => journal(join(ctx.workdir, 'settlements.jsonl'));

console.log('settlement & redemption on the real binary (issue #175)\n');
checkCoreProvenance(BIN, check);

// ── 1 + 2 + 3. The happy path: settle, book the receivable, redeem to cash ────
await session('happy', [], async (ctx) => {
  const { core } = ctx;
  const ready = await rpc.ready(core);
  check('the core runs in dry mode (no venue is reachable from this process)',
    ready.mode === 'dry', `mode=${ready.mode}`);
  // The boot banner goes to stderr while READY arrives over the socket: two
  // channels, so the captured buffer can still be empty when READY lands. Poll
  // for it instead of reading once — a single read here made this check flaky
  // (seen red on a run whose only difference was a rebuilt kernel).
  const banner = await waitForStderr(ctx, /socketMode=0600/);
  check('the boot banner reached the captured stderr (the checks below are not vacuous)',
    /socketMode=/.test(banner), banner.slice(-200) || '(stderr is empty)');
  // The banner is the process's own claim; the mode on the node is the fact. Both
  // are asserted, and both against the literal 0600 rather than against a constant
  // that could drift with the thing it polices.
  check('the socket is owner-only, as the boot banner reports it',
    /socketMode=0600/.test(banner), banner.match(/socketMode=\S+/)?.[0] ?? 'no socketMode in banner');
  const nodeMode = statSync(core.socketPath).mode & 0o777;
  check('the socket node on disk is owner-only',
    nodeMode === 0o600, `mode=${nodeMode.toString(8).padStart(4, '0')} on ${core.socketPath}`);

  const { pos, cashAfterEntry } = await enterWinningPosition(core);
  if (!pos) {
    check('trend_follow entered the winning side', false, `stderr=${ctx.stderr().slice(-200)}`);
    return;
  }
  const shares = num(pos.shares);
  console.log(`  --   entered ${pos.id}: ${shares} shares of UP at ${pos.entryPrice}, ` +
    `expires in ${num(pos.expiresAtMs) - Date.now()} ms`);

  const settled = await settle(core, async (c) => {
    const s = await settlementOf(c);
    return s.redeemedClaims > 0 ? s : null;
  }, 150);
  if (!settled) {
    check('the market settled and its claim was redeemed', false,
      `settlement=${JSON.stringify(await settlementOf(core))}`);
    return;
  }
  const cashAfter = num((await rpc.balance(core)).balance);
  const lines = journalAt(ctx);
  const booking = lines.find((l) => l.kind === 'settlement')?.record;
  const payout = num(booking?.payoutUsd);

  check('the settlement booked the position exactly once',
    settled.settledPositions === 1 && settled.bookedThisSession === 1,
    `settled=${settled.settledPositions} booked=${settled.bookedThisSession}`);
  check("the payout is shares × the market's payout per share (1.00 for the winner)",
    booking != null && num(booking.payoutPerShare) === 1 && payout === shares,
    `record=${JSON.stringify(booking)}`);
  check('the booked amount became cash, exactly once',
    round(cashAfter - cashAfterEntry) === round(payout),
    `cash ${cashAfterEntry} → ${cashAfter} (entry cost ${round(shares * num(pos.entryPrice))}), payout ${payout}`);
  check('the claim left the receivable (settled-but-unredeemed would still show here)',
    num(settled.receivableUsd) === 0 && settled.redeemedClaims === 1 &&
      (settled.claims || []).length === 0,
    JSON.stringify(settled));

  // (1) The record itself says the settlement was local and the redemption was
  // not a chain transaction.
  check('the settlement record names the local dry source, not a venue',
    booking?.source === 'core-dry', `source=${booking?.source}`);
  const redeemed = lines.filter((l) => l.kind === 'redeemed');
  check('the redemption is recorded as simulated — no transaction hash exists',
    redeemed.length === 1 && redeemed[0].tx_hash === 'dry-simulated',
    JSON.stringify(redeemed));
  check('the durable journal holds no chain hash at all',
    !lines.some((l) => JSON.stringify(l).match(/0x[0-9a-f]{64}/)),
    lines.map((l) => l.kind).join(','));
  check('the durable order log holds no venue-bound order',
    journal(ctx.orderLog).every((r) => !r.venue_order_id));

  // (1) The egress check belongs HERE, at the end of a session the core has spent
  // ~20 s running, not at boot: `!/live order executor started/` read from a
  // possibly-empty buffer is vacuously true, which is the failure mode this gate
  // exists to catch elsewhere. The banner check above proves the buffer is
  // populated, so the absence below is a real absence.
  const finalStderr = ctx.stderr();
  check('no venue order executor was started, even with live credentials in the environment',
    /socketMode=/.test(finalStderr) && !/live order executor started/.test(finalStderr),
    finalStderr.slice(-200));

  // (3) Idempotency: watch for longer than the first retry backoff (5 s). If the
  // confirmed claim were not removed from the book, the dry auto-redemption would
  // dispatch it again as soon as its backoff elapsed and credit a second time.
  // The book is held flat while watching, so no new entry can muddy the balance.
  const watchStart = num((await rpc.balance(core)).balance);
  for (let i = 0; i < 14; i++) { await holdWinningBook(core); await sleep(500); }
  const after = await settlementOf(core);
  const watchEnd = num((await rpc.balance(core)).balance);
  check('a second confirmation never credits again (balance flat over >1 backoff window)',
    watchEnd === watchStart,
    `balance ${watchStart} → ${watchEnd} after 7 s`);
  check('the redeemed claim is still gone and nothing was re-booked',
    after.redeemedClaims === 1 && (after.claims || []).length === 0 &&
      after.bookedThisSession === 1 && num(after.receivableUsd) === 0,
    JSON.stringify(after));
  check('the journal still holds exactly one redemption for the claim',
    journalAt(ctx).filter((l) => l.kind === 'redeemed').length === 1);
});

// ── 4a. The retry clock: an in-flight attempt is stamped, then retried ───────
await session('retry', ['--dry-redeem-fail', '1'], async (ctx) => {
  const { core } = ctx;
  const { pos, cashAfterEntry } = await enterWinningPosition(core);
  if (!pos) {
    check('trend_follow entered the winning side (retry session)', false,
      `stderr=${ctx.stderr().slice(-200)}`);
    return;
  }
  const shares = num(pos.shares);
  const claim = await settle(core, async (c) => {
    const s = await settlementOf(c);
    return s.pendingRedemptions > 0 ? s : null;
  }, 150);
  if (!claim) {
    check('the failed redemption left the claim pending', false,
      `settlement=${JSON.stringify(await settlementOf(core))}`);
    return;
  }
  const held = await claimOf(core);
  check('a failed redemption holds the payout as a receivable, not as cash',
    num(claim.receivableUsd) === shares && num(claim.manualRedemptions) === 0 &&
      claim.retryableRedemptions === 1,
    JSON.stringify(claim));
  check('the failure is reported, with the retry armed',
    String(held?.lastError ?? '').includes('dry-simulated redemption failure') &&
      num(held.nextAttemptMs) > Date.now(),
    JSON.stringify(held));
  // The armed delay IS the retry policy: `RETRY_BASE_MS` (5 s) for a first
  // failure. An arm that set "now", or that fell through to a zero backoff, would
  // retry in a hot loop — the thing the stamp exists to prevent. Both edges are
  // asserted, so a backoff of 0 or of a day is a red gate either way.
  const armedMs = num(held.nextAttemptMs) - Date.now();
  check('the retry is armed on the first backoff, not immediately and not never',
    armedMs >= 3500 && armedMs <= 5200, `nextAttemptMs in ${armedMs} ms`);
  check('the failure did not move the cash ledger',
    num((await rpc.balance(core)).balance) === cashAfterEntry,
    `balance=${(await rpc.balance(core)).balance}, after entry ${cashAfterEntry}`);

  // The core-side half of REDEEM_MAX_IN_FLIGHT: dispatching stamps the claim, so
  // the same claim is not dispatched again while the attempt is in flight. Sample
  // across the backoff window: a dispatch that forgot to stamp would re-attempt on
  // every tick (20 Hz here) and `attempts` would climb into the hundreds.
  //
  // The window is bounded by the CLOCK as well as by the sample count. A correct
  // core must show one attempt throughout, but the assertion is only honest while
  // the window closes before the 5 s backoff elapses: on a loaded runner fifteen
  // round trips could otherwise stretch past it, and the retry landing at 5 s would
  // look like a re-dispatch. 2.5 s is half the backoff, so the window cannot
  // overlap it however slow the runner is.
  const seen = new Set();
  const samples = [];
  const windowStart = Date.now();
  while (samples.length < 15 && Date.now() - windowStart < 2500) {
    const c = await claimOf(core);
    if (c) {
      seen.add(c.attempts);
      samples.push(`${c.attempts}@+${num(c.nextAttemptMs) - Date.now()}ms`);
    }
    await sleep(150);
  }
  check('an in-flight attempt is not re-dispatched (attempts holds across the backoff window)',
    seen.size === 1 && seen.has(1),
    `attempts seen: ${[...seen].join(',')} | ${samples.length} samples over ` +
      `${Date.now() - windowStart} ms | ${samples.slice(0, 4).join(' ')}`);

  // And the retry lands on its own, crediting once.
  const landed = await settle(core, async (c) => {
    const s = await settlementOf(c);
    return s.redeemedClaims > 0 ? s : null;
  }, 150);
  check('the armed retry landed after its backoff and the claim became cash',
    landed != null && num(landed.receivableUsd) === 0 && (landed.claims || []).length === 0 &&
      num((await rpc.balance(core)).balance) === round(cashAfterEntry + shares),
    `settlement=${JSON.stringify(landed)} balance=${(await rpc.balance(core)).balance}`);
  const redeemed = journalAt(ctx).filter((l) => l.kind === 'redeemed');
  check('the retry credited exactly once (one redemption in the journal)',
    redeemed.length === 1 && redeemed[0].tx_hash === 'dry-simulated',
    JSON.stringify(redeemed));
});

// ── 4b. The stop rule: a manual failure ends the automatic attempts ──────────
await session('manual', ['--dry-redeem-fail', '5', '--dry-redeem-manual'], async (ctx) => {
  const { core } = ctx;
  const { pos, cashAfterEntry } = await enterWinningPosition(core);
  if (!pos) {
    check('trend_follow entered the winning side (manual session)', false,
      `stderr=${ctx.stderr().slice(-200)}`);
    return;
  }
  const shares = num(pos.shares);
  const claim = await settle(core, async (c) => {
    const s = await settlementOf(c);
    return s.manualRedemptions > 0 ? s : null;
  }, 150);
  if (!claim) {
    check('the manual failure is reported as manual', false,
      `settlement=${JSON.stringify(await settlementOf(core))}`);
    return;
  }
  const held = await claimOf(core);
  check('a manual failure stops the automatic attempts for good',
    held?.manual === true && num(held.nextAttemptMs) > 9e18 &&
      claim.retryableRedemptions === 0 && claim.manualRedemptions === 1,
    JSON.stringify({ held, retryable: claim.retryableRedemptions }));
  check('the manual claim stays in the receivable — the chain still owes it',
    num(claim.receivableUsd) === shares &&
      num((await rpc.balance(core)).balance) === cashAfterEntry,
    `receivable=${claim.receivableUsd} balance=${(await rpc.balance(core)).balance}`);

  // Longer than the first two backoffs would have been: a stop that only looked
  // like one would have re-attempted by now.
  await sleep(6000);
  const after = await settlementOf(core);
  const heldAfter = await claimOf(core);
  check('nothing was attempted again (same claim, same attempt count, cash untouched)',
    num(heldAfter?.attempts) === num(held?.attempts) && after.manualRedemptions === 1 &&
      num(after.receivableUsd) === shares && after.redeemedClaims === 0 &&
      num((await rpc.balance(core)).balance) === cashAfterEntry,
    JSON.stringify({ attempts: [held?.attempts, heldAfter?.attempts], receivable: after.receivableUsd }));
  check('no redemption was ever recorded for the manual claim',
    journalAt(ctx).filter((l) => l.kind === 'redeemed').length === 0);

  // SWEEP_FAILURE_FREEZE (E31-b): the freeze transition is only reachable from a
  // market plugin, so the threshold and the live streak are read-only in
  // `engine.stats.reconcile`. Pinning the number here means widening it silently
  // (3 → 5) is a red gate instead of a weaker safety net nobody notices; the
  // transition itself is covered by the unit tests beside `on_reconcile_failed`.
  const reconcile = (await rpc.stats(core)).reconcile;
  check('the reconcile freeze threshold is 3 consecutive sweep failures, and it is visible',
    num(reconcile?.freezeThreshold) === 3 && num(reconcile?.consecutiveSweepFailures) === 0,
    JSON.stringify(reconcile));
});

console.log('');
if (failures) {
  console.log(`settlement-redeem: ${failures} problem(s)`);
  process.exit(1);
}
console.log('  ok   dry mode starts no venue executor and books settlements as core-dry');
console.log('  ok   a settled payout is receivable money that becomes cash exactly once');
console.log('  ok   a confirmed claim is never re-credited (idempotent across the retry clock)');
console.log('  ok   a failed attempt is stamped and retried on its backoff; manual stops it');
console.log('\nsettlement-redeem: pass');
