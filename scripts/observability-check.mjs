#!/usr/bin/env node
/**
 * Observability acceptance on the REAL binary (issues #180, #184).
 *
 * Two audit findings, one gate, because they are two halves of the same
 * question an operator asks during an incident: "why is nothing happening, and
 * where would I have seen it?"
 *
 * #180 — a refused order carried no reason. `orders.place` answered a bare
 * `{"status":"REJECTED"}` for a dry/read-only taker whose book could not fill
 * it, nothing emitted an error, and the panel's `lastError` slot was written by
 * one unrelated path (venue rejections) — so it stayed empty or stale while
 * orders were being refused. This gate drives all three refusal kinds a caller
 * can hit and asserts they are (a) structurally distinct and (b) the SAME
 * reason the panel-visible `engine.stats.lastError` reports, with a fresh
 * timestamp, within a second.
 *
 * #184 — the default log filter kept only ERROR (`EnvFilter::from_default_env()`
 * with no RUST_LOG), so a production run's warn/info breadcrumbs were filtered
 * out before they were written. This gate asserts the shipped default records
 * INFO and WARN, that the boot banner says which level is in force, and that an
 * explicit RUST_LOG still overrides it (the escape hatch must keep working).
 *
 * Both are asserted on the REAL release binary over the real IPC channel: the
 * unit tests beside the code prove the wiring, this proves a deployed process
 * behaves that way.
 *
 * Usage: node scripts/observability-check.mjs
 *   (needs target/release/blitzkrieg-core built: cargo build --release --workspace --locked)
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { CoreClient, rpc } from './lib/core-client.mjs';
import { checkCoreProvenance, coreBinaryPath } from './lib/core-provenance.mjs';
import { spawn } from './lib/child-guard.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { mkdtempSync, existsSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import { sleep } from './lib/wait.mjs';

const BIN = coreBinaryPath();
const SEED = 1000;
if (!existsSync(BIN)) {
  console.error(`missing binary: ${BIN} (cargo build --release --workspace --locked)`);
  process.exit(2);
}

const gate = createChecks();
const { check } = gate;
const num = (v) => (v == null ? null : Number(v));

/**
 * Wait until the captured stderr matches `re`, or `ms` elapse — then return
 * whatever was captured. Same two-channel race the settlement gate documents:
 * the banner goes to stderr while READY arrives over the socket, so a single
 * read at READY is not evidence.
 *
 * NOTE: `CoreClient.lastStderr` keeps the LAST 2000 characters, so this is only
 * usable for a line emitted AFTER the request that preceded it (the refusal log
 * line, the boot banner in a run with almost no other output). Anything emitted
 * during a normal boot is asserted through [`bootLogs`] instead, which keeps
 * every byte.
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
 * Boot a core for its LOG OUTPUT ALONE, capturing stderr in full, and return it.
 *
 * Why not `CoreClient`: its stderr window is the last 2000 characters, and at the
 * default INFO level a boot writes more than that before the first RPC — the
 * banner has already scrolled out by the time a socket client can read it (which
 * is exactly how the first version of this gate went red on a green build). This
 * helper collects every byte, waits for the lines under test or a deadline, and
 * reaps the child (through the same exit guard every gate uses).
 *
 * `env` is applied OVER the ambient environment, and `RUST_LOG` is always
 * removed first: "the default level" is only the default when nothing sets it,
 * so a CI that exports RUST_LOG globally cannot make this pass vacuously.
 *
 * `--event-archive` is on purpose: opening the archive is a deterministic
 * INFO-level line at boot, which is what separates "the default is INFO" from
 * "the default is WARN". `--enable-strategy <unknown>` is the deterministic WARN.
 *
 * `--allow-zero-strategies` is REQUIRED beside that unknown name since #265: a
 * boot whose only explicitly requested strategy resolves to nothing now REFUSES
 * to start (the same "an unknown argument is never silently ignored" principle,
 * applied to strategy names), and a refusal never reaches the INFO archive line
 * this gate asserts on. The flag is the gate saying out loud that it asked for a
 * name on purpose and wants the WARN, not a boot.
 */
async function bootLogs(tag, { env = {}, waitFor = [/logLevel=/], ms = 8000 } = {}) {
  const workdir = mkdtempSync(join(tmpdir(), `bk-obs-log-${tag}-`));
  const childEnv = { ...process.env };
  delete childEnv.RUST_LOG;
  Object.assign(childEnv, env);
  const child = spawn(
    BIN,
    [
      '--socket', join(workdir, 'core.sock'),
      '--mode', 'dry',
      '--tick-ms', '50',
      '--seed-balance', String(SEED),
      '--max-order-notional', '5',
      '--no-trade-log', '--no-order-log', '--no-position-log',
      '--engine',
      '--no-discovery',
      '--enable-strategy', 'no_such_strategy_for_the_observability_gate',
      // #265: an explicit request that resolves to nothing is a REFUSAL now, so
      // the deliberate unknown name needs the acknowledgement flag or this boot
      // dies before the archive line. The WARN still lands (install_engine warns
      // per unknown name before the self-check runs); the gate wants the WARN.
      '--allow-zero-strategies',
      '--event-archive', join(workdir, 'events.jsonl'),
    ],
    { cwd: workdir, env: childEnv, stdio: ['ignore', 'pipe', 'pipe'] },
  );
  let stderr = '';
  child.stderr.on('data', (d) => { stderr += String(d); });
  const deadline = Date.now() + ms;
  while (Date.now() < deadline && !waitFor.every((re) => re.test(stderr))) await sleep(50);
  child.kill('SIGTERM');
  await new Promise((r) => child.once('exit', r));
  return stderr;
}

/**
 * Boot one dry core, hand its context to `body`, always tear down.
 *
 * `env` is applied to the child's environment around the spawn: `CoreClient`
 * passes `process.env` through, so the only way to drive RUST_LOG per session is
 * to set it here — and the DEFAULT sessions must have it ABSENT (that is the
 * condition under test), so the gate removes it explicitly rather than trusting
 * the ambient environment. Whether it was there is reported, so a CI that sets
 * RUST_LOG globally cannot make this gate pass vacuously.
 */
async function session(tag, { env = {}, extraArgs = [] } = {}, body) {
  const workdir = mkdtempSync(join(tmpdir(), `bk-obs-${tag}-`));
  const core = new CoreClient({
    binaryPath: BIN,
    socketPath: join(workdir, 'core.sock'),
    mode: 'dry',
    seedBalance: SEED,
    maxOrderNotional: 5,
    tickMs: 50,
    autoRestart: false,
    cwd: workdir,
    noTradeLog: true,
    noOrderLog: true,
    noPositionLog: true,
    extraArgs: [
      '--engine',
      '--no-discovery',
      // An unknown strategy name is a WARN at startup (`install_engine`), the
      // deterministic warn-level path this gate needs. It is harmless: the name
      // simply matches nothing — but since #265 a boot whose only requested
      // strategy resolves to nothing refuses to start, so the request must carry
      // `--allow-zero-strategies` to reach the WARN and the archive line.
      '--enable-strategy', 'no_such_strategy_for_the_observability_gate',
      '--allow-zero-strategies',
      '--event-archive', join(workdir, 'events.jsonl'),
      ...extraArgs,
    ],
  });
  const events = [];
  core.onEvent = (e) => events.push(e);
  const hadRustLog = 'RUST_LOG' in process.env;
  const saved = process.env.RUST_LOG;
  delete process.env.RUST_LOG;
  Object.assign(process.env, env);
  try {
    await core.start();
    return await body({ core, workdir, events, stderr: () => core.lastStderr, hadRustLog });
  } finally {
    await core.stop().catch(() => {});
    delete process.env.RUST_LOG;
    if (hadRustLog && saved !== undefined) process.env.RUST_LOG = saved;
  }
}

/** Place one taker leg and report what came back, without throwing. */
async function placeTaker(core, { tokenId, asset, price, size, key }) {
  try {
    const res = await rpc.placeOrder(core, {
      tokenId,
      conditionId: `cond-${tokenId}`,
      side: 'buy',
      mode: 'taker',
      price,
      size,
      internalKey: key,
      strategy: 'obs_gate',
      asset,
      direction: 'up',
      roundSlot: 1,
    });
    return { kind: 'result', res };
  } catch (e) {
    return { kind: 'error', code: e.coreCode, message: e.message };
  }
}

/**
 * The refusal reason the caller got, from either carrier: the `orders.place`
 * RESULT (a leg the kernel accepted and then refused) or the RPC ERROR (a leg
 * the kernel refused before submitting it). Same vocabulary, two transports —
 * which is exactly the "one error model" property #180 asks for.
 */
const reasonOf = (r) => (r.kind === 'result' ? r.res.message : r.message);
const codeOf = (r) => (r.kind === 'result' ? r.res.code : r.code);

/**
 * The BARE reason — no code prefix — from either carrier.
 *
 * The result carrier and the `engine.stats` slot both carry `code` beside a bare
 * `message`; the RPC error carries the code in `data.coreCode` and renders the
 * same error with `Display`, i.e. `<Variant>: <message>`. Stripping that prefix
 * is how the gate compares the two carriers at all, and the prefix shape itself
 * is asserted (below) rather than assumed.
 */
const bareReason = (r) => (r.kind === 'result' ? r.res.message : r.message.replace(/^[A-Za-z0-9]+: /, ''));
/** The `<Variant>` an RPC error prefixes its message with, or null. */
const rpcVariant = (r) => (r.kind === 'error' ? r.message.match(/^([A-Za-z0-9]+): /)?.[1] ?? null : null);
/** `RiskRejected` → `RISK_REJECTED`, the serde spelling of the same variant. */
const screaming = (v) => v.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toUpperCase();

/** Poll `engine.stats.lastError` until it reports `expected`, or the deadline. */
async function lastErrorWithin(core, expected, ms = 1000) {
  const deadline = Date.now() + ms;
  let seen = null;
  for (;;) {
    const stats = await rpc.stats(core).catch(() => null);
    seen = stats?.lastError ?? null;
    if (seen && seen.message === expected) return { ok: true, seen, waitedMs: ms - (deadline - Date.now()) };
    if (Date.now() >= deadline) return { ok: false, seen, waitedMs: ms };
    await sleep(25);
  }
}

console.log('observability on the real binary (issues #180, #184)\n');
checkCoreProvenance(BIN, check);

// ── 1. #184: the default filter records INFO/WARN, and says so ────────────────
if ('RUST_LOG' in process.env) {
  console.log('  --   note: RUST_LOG was set in this gate\'s environment; the default-level section removes it');
}
{
  const logs = await bootLogs('default', {
    waitFor: [
      /logLevel=/,
      /recording market-data archive/,
      /unknown strategy requested at startup/,
    ],
  });
  // The banner is the process's own claim about the filter it built; the line
  // assertions below are the fact. Both are checked, and against the literal
  // "info" rather than a constant that could drift with the thing it polices.
  check('the boot banner reached stderr (the log assertions below are not vacuous)',
    /socketMode=/.test(logs) && /logLevel=/.test(logs),
    logs.slice(0, 400) || '(stderr is empty)');
  check('the boot banner echoes the effective log level (default = info)',
    /logLevel=info\b/.test(logs), logs.match(/logLevel=\S+/)?.[0] ?? 'no logLevel in banner');
  check('the startup echo names the level AND where it came from',
    /log level info \(source: default\)/.test(logs),
    logs.match(/log level [^(]*\(source: [^)]*\)/)?.[0] ?? 'no level echo');
  // INFO: opening the event archive is an INFO line, so this is what separates
  // "the default is INFO" from "the default is WARN".
  check('an INFO line lands on stderr at the default level (the default is not WARN or ERROR)',
    /INFO/.test(logs) && /recording market-data archive/.test(logs),
    logs.split('\n').filter((l) => /archive/.test(l)).join('\n') || '(no archive line)');
  // WARN: the unknown strategy name is refused with a warn-level line.
  check('a WARN line lands on stderr at the default level (the filter is not ERROR)',
    /WARN/.test(logs) && /unknown strategy requested at startup/.test(logs),
    logs.split('\n').filter((l) => /unknown strategy/.test(l)).join('\n') || '(no warn line)');
}

// ── 2. #184: an explicit RUST_LOG still overrides the default ─────────────────
{
  const logs = await bootLogs('rustlog', { env: { RUST_LOG: 'error' }, waitFor: [/logLevel=/] });
  check('RUST_LOG overrides the default: the banner reports the effective level, not the default',
    /logLevel=error\b/.test(logs) && /log level error \(source: RUST_LOG\)/.test(logs),
    logs.match(/logLevel=\S+|log level [^(]*\(source: [^)]*\)/g)?.join(' | ') ?? 'nothing');
  // The override has to be a FILTER, not just a label: the INFO and WARN lines
  // that appeared at the default level must be gone at `error`. Both are emitted
  // during boot, so a full-stderr capture settles it.
  check('the INFO/WARN lines are filtered out under RUST_LOG=error (the override really filters)',
    !/recording market-data archive/.test(logs) &&
      !/unknown strategy requested at startup/.test(logs),
    logs.slice(-300));
}

// The refusal half of the same session needs the socket, so it runs on a real
// client — and under the same stricter filter, because the ERROR line has to
// survive it.
await session('rustlog', { env: { RUST_LOG: 'error' } }, async (ctx) => {
  const { core } = ctx;
  await rpc.ready(core);
  const err = await placeTaker(core, { tokenId: 'OBS-RUSTLOG', asset: 'OBS-RUSTLOG', price: 0.4, size: 5, key: 'obs-rustlog' });
  const captured = await waitForStderr(ctx, /order rejected/);
  check('an ERROR line still lands under RUST_LOG=error',
    /ERROR/.test(captured) && /order rejected/.test(captured), captured.slice(-300));
  check('the refusal still carries its reason under a stricter filter',
    codeOf(err) === 'WOULD_CROSS' && /taker_no_crossing_liquidity:/.test(reasonOf(err)),
    JSON.stringify(err));
});

// ── 3. #180: three refusal kinds, distinct reasons, one panel-visible slot ────
await session('reasons', {}, async (ctx) => {
  const { core, events } = ctx;
  await rpc.ready(core);

  // (a) Nothing crosses the limit. No book has been mirrored for this token, so
  //     the ask side is empty — a live FOK would be killed exactly here.
  const noCross = await placeTaker(core, { tokenId: 'OBS-A', asset: 'OBS-A', price: 0.4, size: 5, key: 'obs-a' });
  check('a taker that cannot cross the book comes back as a REJECTED result, not a bare status',
    noCross.kind === 'result' && noCross.res.status === 'REJECTED',
    JSON.stringify(noCross));
  check('  ... carrying the structured code and the no-crossing reason',
    codeOf(noCross) === 'WOULD_CROSS' && /^taker_no_crossing_liquidity:/.test(reasonOf(noCross)),
    JSON.stringify({ code: codeOf(noCross), message: reasonOf(noCross) }));

  const le1 = await lastErrorWithin(core, reasonOf(noCross));
  check('engine.stats.lastError updated to that exact reason within 1 s (the panel reads it)',
    le1.ok, `lastError=${JSON.stringify(le1.seen)} after ${le1.waitedMs} ms`);
  check('  ... with the same structured code and a real timestamp',
    le1.seen?.code === 'WOULD_CROSS' && num(le1.seen?.tsMs) > 0, JSON.stringify(le1.seen));
  const stats1 = await rpc.stats(core);
  // The legacy key is the same record, in its pre-#180 rendering (`<Code>:
  // <message>`, the Rust variant name — what the shipped panel banner shows).
  // Pinned on the wire, because that string is the compatibility contract.
  check('the legacy lastVenueError key the shipped panel reads keeps its pre-#180 shape and instant',
    stats1.lastVenueError?.message === `WouldCross: ${reasonOf(noCross)}` &&
      num(stats1.lastVenueError?.tsMs) === num(le1.seen?.tsMs),
    JSON.stringify(stats1.lastVenueError));
  check('the refusal also reached the event stream (emit_error, not a silent state change)',
    events.some((e) => e.kind === 'ERROR' && e.error?.message === reasonOf(noCross) &&
      e.error?.code === 'WOULD_CROSS'),
    JSON.stringify(events.filter((e) => e.kind === 'ERROR').slice(-2)));

  // (b) The book crosses but is too thin for the size — a DIFFERENT cause, and
  //     the reason has to say which one it was. The slot is polled right after
  //     each refusal, never at the end: the slot is the LAST error by
  //     definition, so a single poll after all three could only ever see the
  //     third one (and would let a stale slot pass).
  await rpc.bookSnapshot(core, 'OBS-B', [[0.35, 100]], [[0.40, 3]]);
  const thin = await placeTaker(core, { tokenId: 'OBS-B', asset: 'OBS-B', price: 0.4, size: 5, key: 'obs-b' });
  check('a taker into a too-thin crossing book is refused with the depth reason',
    thin.kind === 'result' && thin.res.status === 'REJECTED' &&
      codeOf(thin) === 'WOULD_CROSS' && /^taker_insufficient_depth:/.test(reasonOf(thin)),
    JSON.stringify({ code: codeOf(thin), message: reasonOf(thin) }));
  const le2 = await lastErrorWithin(core, reasonOf(thin));
  check('the slot moved to the depth reason (not stale from the previous refusal)',
    le2.ok && le2.seen?.code === 'WOULD_CROSS' && num(le2.seen?.tsMs) >= num(le1.seen?.tsMs),
    `before=${JSON.stringify(le1.seen)} after=${JSON.stringify(le2.seen)}`);

  // (c) The risk gate refuses before submission — the third kind, and the one
  //     that already had a structured carrier (the RPC error).
  await rpc.bookSnapshot(core, 'OBS-C', [[0.35, 100]], [[0.40, 100]]);
  const risky = await placeTaker(core, { tokenId: 'OBS-C', asset: 'OBS-C', price: 0.4, size: 100, key: 'obs-c' });
  check('a risk refusal comes back as an RPC error with its own code',
    risky.kind === 'error' && risky.code === 'RISK_REJECTED' &&
      /exceeds per-order cap/.test(risky.message),
    JSON.stringify(risky));
  const le3 = await lastErrorWithin(core, bareReason(risky));
  check('the slot moved to the RISK_REJECTED reason too (every refusal is recorded, not just liquidity ones)',
    le3.ok && le3.seen?.code === 'RISK_REJECTED' && num(le3.seen?.tsMs) >= num(le2.seen?.tsMs),
    JSON.stringify(le3.seen));
  check('the slot is the LAST error, so its timestamp advanced across the three refusals',
    num(le3.seen?.tsMs) > num(le1.seen?.tsMs),
    `ts ${num(le1.seen?.tsMs)} → ${num(le3.seen?.tsMs)}`);
  // The two carriers of the SAME error: the RPC error renders `<Variant>:
  // <message>` with the code in `data.coreCode`; the slot carries `code` beside
  // the bare message. Same variant, same reason — no second error model.
  check('the RPC error and the panel slot describe the same error, code and reason alike',
    rpcVariant(risky) !== null && screaming(rpcVariant(risky)) === codeOf(risky) &&
      bareReason(risky) === le3.seen?.message,
    JSON.stringify({ rpc: risky.message, slot: le3.seen }));

  // Distinctness is asserted on the CAUSE, not on the whole string: two
  // refusals of the same cause still differ by token id and size, so comparing
  // raw messages would let a hardcoded one-word reason pass. The cause is the
  // leading `<token>:` of a liquidity refusal, or the error code for anything
  // that reaches the caller through the risk gate — the machine-assertable part
  // of the reason, which is what a caller branches on.
  const causeOf = (r) => bareReason(r).match(/^[a-z][a-z0-9_]*:/)?.[0] ?? `code:${codeOf(r)}`;
  const causes = [causeOf(noCross), causeOf(thin), causeOf(risky)];
  check('the three refusal reasons are mutually distinct and machine-assertable',
    new Set(causes).size === 3 && causes.every((c) => c && c.length > 5),
    `${JSON.stringify(causes)} from ${JSON.stringify([bareReason(noCross), bareReason(thin), bareReason(risky)])}`);
  check('the risk reason is not dressed up as a liquidity one',
    !/taker_no_crossing_liquidity|taker_insufficient_depth/.test(bareReason(risky)),
    bareReason(risky));

  // The other direction: a healthy fill must not populate the slot, or the
  // panel banner would be lit permanently and mean nothing.
  await session('clean', {}, async (inner) => {
    await rpc.ready(inner.core);
    await rpc.bookSnapshot(inner.core, 'OBS-OK', [[0.35, 100]], [[0.40, 100]]);
    const filled = await rpc.placeOrder(inner.core, {
      tokenId: 'OBS-OK', conditionId: 'cond-OBS-OK', side: 'buy', mode: 'taker',
      price: 0.4, size: 5, internalKey: 'obs-ok', strategy: 'obs_gate',
      asset: 'OBS-OK', direction: 'up', roundSlot: 1,
    });
    check('an order the kernel fills leaves the error slot empty',
      filled.status === 'FILLED' && filled.code === undefined && filled.message === undefined,
      JSON.stringify(filled));
    const stats = await rpc.stats(inner.core);
    check('  ... and engine.stats.lastError is null, not an empty object',
      stats.lastError === null && stats.lastVenueError === null,
      JSON.stringify({ lastError: stats.lastError, lastVenueError: stats.lastVenueError }));
  });
});

console.log('');
if (gate.failures) {
  console.log(`observability: ${gate.failures} problem(s)`);
  process.exit(1);
}
console.log('  ok   a refused order carries its reason, and the panel slot tracks the last one');
console.log('  ok   the three refusal kinds are distinguishable without parsing prose');
console.log('  ok   the default log level records INFO/WARN and the banner says so');
console.log('  ok   RUST_LOG still overrides the default, as a real filter');
console.log('\nobservability: pass');
