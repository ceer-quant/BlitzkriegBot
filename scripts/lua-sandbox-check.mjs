#!/usr/bin/env node
/**
 * lua:sandbox-check — DEV_V0_3 §16.4 gate (E30 / #336, §6.3 / §6.4).
 *
 * The contract: a Lua 5.4 strategy runs inside a CLOSED sandbox (§6.3) and a
 * SIGNED package (§6.4), beside the dylib stack, and the kernel survives
 * every way a script can misbehave:
 *
 *   §6.3 quotas — a 16 MiB memory ceiling and a 1e6 VM-instruction budget
 *   per host-side callback. Exhaustion is POISON: the flag is set first, the
 *   error second; pcall swallows the error, never the poison; every later
 *   call from that sandbox is refused; the kernel raises exactly ONE
 *   RISK_ALERT naming the strategy and keeps running (quarantine, not a
 *   crash). A plain Lua error (nil `require`, nil `os`) is a deterministic
 *   bug, NOT poison — the host keeps calling, no alert.
 *
 *   §6.4 packaging — manifest.json with a MANDATORY sha256 (the refusal
 *   names BOTH expected and actual), directory name == manifest.name (a
 *   package with two identities is refused), `bk_evaluate` required. The
 *   startup scan is the LOG-ONLY handshake site.
 *
 * The real verdict drives a REAL dry core over UDS with BOTH stacks mounted:
 * the momentum_alpha dylib (the E24/E27 reference) and FIVE Lua packages —
 * the official `lua_momentum` example plus four hostile fixtures:
 *   A `sb_escape` — touches the forbidden surface (`require`/`os`/`debug`
 *     are nil) → a deterministic Lua error every call, never poison, no
 *     RISK_ALERT, host keeps going;
 *   B `sb_loop`  — `while true do end` → instruction budget → poison;
 *   C `sb_pcall` — the same loop wrapped in pcall → STILL poison;
 *   D `sb_grow`  — one 17 MiB `string.rep` over the 16 MiB ceiling → poison.
 * plus two packages loaded over the IPC `strategy.load` (the ENFORCE site)
 * that must be REFUSED:
 *   F `sb_tamper` — the manifest sha256 does not match the file on disk;
 *     the refusal carries both hex digests;
 *   G `sb_clash`  — directory name ≠ manifest name (two identities).
 *
 * The judge (`judgeTrace`) is the same function the self-test and the teeth
 * exercise: the teeth feed it the OUTPUT of the two broken implementations
 * the issue names — a budget breach that never set the poison flag, and a
 * tampered package that loaded anyway — and demand red naming the vocabulary.
 *
 * Usage:
 *   node scripts/lua-sandbox-check.mjs              # the real verdict
 *   node scripts/lua-sandbox-check.mjs --self-test  # judge fixtures, no binary
 *   node scripts/lua-sandbox-check.mjs --teeth      # must go red
 * Exit: 0 pass / 1 verdict failure / 2 environment missing.
 */

import { spawn, execFileSync } from './lib/child-guard.mjs';
import { CoreClient } from './lib/core-client.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { waitForSocket, sleep, pollUntil } from './lib/wait.mjs';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { existsSync, mkdtempSync, readdirSync, rmSync, unlinkSync, writeFileSync, mkdirSync, copyFileSync, cpSync } from 'fs';
import { createHash } from 'crypto';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = process.env.BK_CORE_BIN || join(ROOT, 'target', 'release', 'blitzkrieg-core');
const EXAMPLES = join(ROOT, 'user_layer', 'examples');
const DYLIB_EXT = process.platform === 'win32' ? 'dll' : process.platform === 'darwin' ? 'dylib' : 'so';
const OFFICIAL_DIR = join(ROOT, 'user_layer', 'strategies_lua', 'lua_momentum');
const OFFICIAL_NAME = 'lua_momentum';
const REFERENCE_NAME = 'momentum_alpha';

// The §6.3 vocabulary, exactly as the issue pins it. Teeth E and the §6.3
// rule below must both carry the `poison flag not set` anchor; teeth F and
// the §6.4 rule both carry `sha256 mismatch not raised`.
const POISON_ANCHOR = 'poison flag not set';
const SHA_ANCHOR = 'sha256 mismatch not raised';

/* ── judges ───────────────────────────────────────────────────────────────
 * Pure functions over observed wire shapes. The real run, the self-test and
 * the teeth feed the SAME judges — that is what gives the teeth meaning. */

const ROW_KEYS = ['name', 'enabled', 'modes', 'compatible', 'incompatibleReason'];

/** §8.3 row shape: both stacks surface the SAME five fields, nothing else. */
export function judgeRowShape(row) {
  const problems = [];
  const keys = Object.keys(row ?? {}).sort();
  const want = [...ROW_KEYS].sort();
  if (keys.length !== want.length || keys.some((k, i) => k !== want[i])) {
    problems.push(`strategy.list row shape drifted: got [${keys.join(',')}] want [${want.join(',')}]`);
  }
  return problems;
}

/** A successful load receipt. `lua: true` demands the `(lua)` stack marker. */
export function judgeLoadReceipt(receipt, { name, version, lua }) {
  const problems = [];
  const r = String(receipt ?? '');
  if (!r.includes(`${name}@${version}`)) problems.push(`receipt does not name ${name}@${version}: ${r.slice(0, 120)}`);
  if (!r.includes('registered into the engine dispatch')) problems.push(`receipt is not a registration: ${r.slice(0, 120)}`);
  if (lua && !r.includes('(lua)')) problems.push(`lua receipt lost the (lua) stack marker: ${r.slice(0, 120)}`);
  if (!lua && r.includes('(lua)')) problems.push(`dylib receipt carries a (lua) marker: ${r.slice(0, 120)}`);
  if (!r.includes('(disabled')) problems.push(`scan receipt should register disabled: ${r.slice(0, 120)}`);
  return problems;
}

/** A refusal receipt: `Failed:` plus every fragment the operator needs. */
export function judgeRefusalReceipt(receipt, fragments) {
  const problems = [];
  const r = String(receipt ?? '');
  if (!r.startsWith('Failed:')) problems.push(`refusal does not start with Failed: ${r.slice(0, 160)}`);
  for (const f of fragments) {
    if (!r.includes(f)) problems.push(`refusal omits ${JSON.stringify(f)}: ${r.slice(0, 160)}`);
  }
  return problems;
}

/**
 * The §6.3 + §6.4 verdict over one observed run.
 *
 * `loads`  — [{name, expectedSha, actualSha, outcome: 'ok'|'refused', receipt}]
 * `calls`  — [{name, exhausted, poisonProven}]  (one per sandbox that hit a
 *            quota or was expected to; `poisonProven` is honest: it is set
 *            only when the kernel's RISK_ALERT for that strategy arrived —
 *            the alert is drained from the poison flag itself, so an alert
 *            PROVES the flag was set)
 * `alerts` — the RISK_ALERT messages the kernel raised.
 */
export function judgeTrace({ loads = [], calls = [], alerts = [] } = {}) {
  const problems = [];
  for (const l of loads) {
    if (l.outcome === 'ok' && l.expectedSha !== l.actualSha) {
      problems.push(`${SHA_ANCHOR}: ${l.name} LOADED with manifest sha ${l.expectedSha} over file sha ${l.actualSha}`);
    }
    if (l.outcome === 'refused' && l.expectedSha !== l.actualSha) {
      for (const v of [l.expectedSha, l.actualSha]) {
        if (!String(l.receipt ?? '').includes(v)) {
          problems.push(`sha256 refusal does not name both values for ${l.name}: missing ${v.slice(0, 16)}…`);
        }
      }
    }
  }
  for (const c of calls) {
    if (c.exhausted && !c.poisonProven) {
      problems.push(`${POISON_ANCHOR} after budget breach: ${c.name} kept a live sandbox (no RISK_ALERT, quarantine unproven)`);
    }
    if (c.poisonProven) {
      const mine = alerts.filter((a) => String(a).includes(c.name));
      if (mine.length === 0) problems.push(`poisoned without RISK_ALERT: ${c.name}`);
      else if (!mine.every((a) => String(a).includes('poisoned'))) {
        problems.push(`RISK_ALERT does not name the poison for ${c.name}: ${String(mine[0]).slice(0, 120)}`);
      }
    }
  }
  return problems;
}

/* ── self-test: judge fixtures, no binary ─────────────────────────────── */

function selfTest() {
  const shaA = 'a'.repeat(64);
  const shaB = 'b'.repeat(64);
  const goodLua = `${OFFICIAL_NAME}@0.1.0 (lua) registered into the engine dispatch (disabled)`;
  const goodDylib = `${REFERENCE_NAME}@0.3.0 registered into the engine dispatch (disabled)`;
  const rows = [
    ['a (lua) scan receipt reads clean', judgeLoadReceipt(goodLua, { name: OFFICIAL_NAME, version: '0.1.0', lua: true }), 0, null],
    ['a receipt that lost the (lua) marker is red', judgeLoadReceipt(goodLua.replace(' (lua)', ''), { name: OFFICIAL_NAME, version: '0.1.0', lua: true }), 1, '(lua)'],
    ['a dylib receipt without the marker reads clean', judgeLoadReceipt(goodDylib, { name: REFERENCE_NAME, version: '0.3.0', lua: false }), 0, null],
    ['a dylib receipt that grew a (lua) marker is red', judgeLoadReceipt(goodDylib.replace('dispatch', 'dispatch (lua)'), { name: REFERENCE_NAME, version: '0.3.0', lua: false }), 1, '(lua)'],
    ['a refusal naming every fragment reads clean', judgeRefusalReceipt('Failed: sha256 mismatch for strategy.lua: manifest expects `' + shaA + '`, actual is `' + shaB + '`', ['sha256 mismatch', shaA, shaB]), 0, null],
    ['a refusal that drops the actual digest is red', judgeRefusalReceipt('Failed: sha256 mismatch for strategy.lua: manifest expects `' + shaA + '`', ['sha256 mismatch', shaA, shaB]), 1, 'omits'],
    ['a §8.3 row with exactly the five fields reads clean', judgeRowShape({ name: 'x', enabled: false, modes: null, compatible: true, incompatibleReason: null }), 0, null],
    ['a row that grew a diagnostics field is red', judgeRowShape({ name: 'x', enabled: false, modes: null, compatible: true, incompatibleReason: null, diagnostics: {} }), 1, 'shape drifted'],
    ['a row that lost incompatibleReason is red', judgeRowShape({ name: 'x', enabled: false, modes: null, compatible: true }), 1, 'shape drifted'],
    ['a trace where every exhaustion poisoned with its alert reads clean', judgeTrace({
      loads: [{ name: OFFICIAL_NAME, expectedSha: shaA, actualSha: shaA, outcome: 'ok', receipt: goodLua }],
      calls: [
        { name: 'sb_loop', exhausted: true, poisonProven: true },
        { name: 'sb_grow', exhausted: true, poisonProven: true },
        { name: 'sb_escape', exhausted: false, poisonProven: false },
      ],
      alerts: ["lua strategy 'sb_loop' poisoned: instruction budget exceeded", "lua strategy 'sb_grow' poisoned: not enough memory"],
    }), 0, null],
    ['a tampered package that LOADED anyway is red', judgeTrace({
      loads: [{ name: 'sb_tamper', expectedSha: shaA, actualSha: shaB, outcome: 'ok', receipt: 'ok' }],
    }), 1, SHA_ANCHOR],
    ['a budget breach that never set the poison flag is red', judgeTrace({
      calls: [{ name: 'sb_loop', exhausted: true, poisonProven: false }],
    }), 1, POISON_ANCHOR],
    ['a poison whose RISK_ALERT names another strategy is red', judgeTrace({
      calls: [{ name: 'sb_grow', exhausted: true, poisonProven: true }],
      alerts: ["lua strategy 'sb_loop' poisoned: not enough memory"],
    }), 1, 'without RISK_ALERT'],
    ['a tampered load refused but naming only one digest is red', judgeTrace({
      loads: [{ name: 'sb_tamper', expectedSha: shaA, actualSha: shaB, outcome: 'refused', receipt: 'Failed: sha256 mismatch: manifest expects `' + shaA + '`' }],
    }), 1, 'both values'],
  ];
  let bad = 0;
  for (const [name, problems, wantProblems, mention] of rows) {
    const ok = wantProblems === 0 ? problems.length === 0
      : problems.length > 0 && (mention === null || problems.some((p) => p.includes(mention)));
    if (!ok) { bad += 1; console.error(`  FAIL ${name} — ${JSON.stringify(problems)}`); }
    else console.log(`  ok   ${name}`);
  }
  if (bad > 0) { console.error(`\nsandbox-check self-test: ${bad} of ${rows.length} fixtures failed`); process.exit(1); }
  console.log(`\nsandbox-check self-test: ${rows.length} fixtures passed`);
}

/* ── teeth: the broken implementations' outputs must go red ───────────── */

function teeth() {
  // §16.6 discipline (as plugin-modes-check): feed the BROKEN implementation's
  // OUTPUT to the judge and expect red naming the vocabulary. E deletes the
  // sandbox's poison() call (budget breach errors but never quarantines);
  // F deletes the loader's sha256 comparison (a tampered package loads).
  const shaA = 'e38d43cf3e55aff2889be2951e3fab5a79f2a7ad4b5df9da9d48f51c84bdf3d0';
  const shaB = 'f38d43cf3e55aff2889be2951e3fab5a79f2a7ad4b5df9da9d48f51c84bdf3d0';
  const mutations = [
    { name: 'E: poison() deleted → a budget breach errors but the sandbox stays live',
      trace: { calls: [{ name: 'sb_loop', exhausted: true, poisonProven: false }], alerts: [] },
      mention: POISON_ANCHOR },
    { name: 'F: sha256 comparison deleted → a tampered package LOADS',
      trace: { loads: [{ name: 'sb_tamper', expectedSha: shaA, actualSha: shaB, outcome: 'ok', receipt: 'sb_tamper@0.1.0 (lua) registered into the engine dispatch (disabled)' }] },
      mention: SHA_ANCHOR },
    { name: 'F-b: refusal survives but drops the actual digest → the operator cannot audit',
      trace: { loads: [{ name: 'sb_tamper', expectedSha: shaA, actualSha: shaB, outcome: 'refused', receipt: `Failed: sha256 mismatch for strategy.lua: manifest expects \`${shaA}\`` }] },
      mention: 'both values' },
    { name: 'G: quarantine alert names the wrong strategy → the poison is unauditable',
      trace: { calls: [{ name: 'sb_grow', exhausted: true, poisonProven: true }], alerts: ["lua strategy 'sb_loop' poisoned: not enough memory"] },
      mention: 'without RISK_ALERT' },
  ];
  let missed = 0;
  for (const m of mutations) {
    const problems = judgeTrace(m.trace);
    const caught = problems.some((p) => p.includes(m.mention));
    console.log(`${caught ? '  ok  ' : '  FAIL'} teeth: ${m.name}`);
    if (caught) console.log(`       caught: ${problems.find((p) => p.includes(m.mention)).slice(0, 150)}`);
    else missed += 1;
  }
  if (missed === 0) {
    console.log('\nsandbox-check --teeth: every broken-sandbox output was caught — the gate has teeth');
    process.exit(0);
  }
  console.error(`\nsandbox-check --teeth: ${missed} mutation(s) slipped through green — the gate has no teeth`);
  process.exit(1);
}

/* ── the real verdict ─────────────────────────────────────────────────── */

async function bootCore(sock, args, env) {
  const proc = spawn(CORE, ['--socket', sock, '--mode', 'dry', '--tick-ms', '50',
    '--seed-balance', '1000', '--engine', '--no-discovery', '--no-event-archive',
    '--no-trade-log', '--no-order-log', '--no-position-log',
    '--round-sec', '3600', '--min-round-age', '0', '--min-time-left', '0', ...args],
    { stdio: ['ignore', 'pipe', 'pipe'], cwd: mkdtempSync(join(tmpdir(), 'bk-e30-lua-')), env });
  const output = { text: '' };
  proc.stdout?.on('data', (d) => { output.text += d; });
  proc.stderr?.on('data', (d) => { output.text += d; });
  await waitForSocket(sock, { timeoutMs: 15000 });
  const client = await CoreClient.connect({ socketPath: sock });
  await client.request('core.ready');
  return { proc, client, output };
}

function sha256Buf(buf) {
  return createHash('sha256').update(buf).digest('hex');
}

/** Write one hostile package: `strategy.lua` + a manifest whose sha256 is the
 * REAL digest of the file (so the loader's signature check passes and only
 * the RUNTIME behavior is hostile). */
function writePackage(dir, name, script, { shaOverride } = {}) {
  mkdirSync(dir, { recursive: true });
  const buf = Buffer.from(script, 'utf8');
  writeFileSync(join(dir, 'strategy.lua'), buf);
  const actual = sha256Buf(buf);
  writeFileSync(join(dir, 'manifest.json'), JSON.stringify({
    name, version: '0.1.0', api: '1.0', entry: 'strategy.lua', sha256: shaOverride ?? actual,
  }, null, 2) + '\n');
  return actual;
}

const HOSTILE = {
  // A: the forbidden surface is nil (§6.3 blacklist is a real security layer,
  // not documentation) — touching it is a deterministic Lua error, never a
  // poison. The host must keep calling; no RISK_ALERT may ever name it.
  sb_escape: `-- A: forbidden-surface probe; every touch is a plain nil error
function bk_evaluate(ctx)
  require('io')
  os.time()
  debug.getinfo(1)
  return {}
end
`,
  // B: the instruction budget — one tight loop burns 1e6 VM instructions.
  sb_loop: `-- B: instruction budget; the hook poisons first, errors second
function bk_evaluate(ctx)
  while true do end
  return {}
end
`,
  // C: pcall swallows the error, never the poison — the success-path recheck.
  sb_pcall: `-- C: pcall cannot swallow the poison (§6.3)
function bk_evaluate(ctx)
  pcall(function() while true do end end)
  return {}
end
`,
  // D: one 17 MiB allocation over the 16 MiB ceiling.
  sb_grow: `-- D: memory ceiling (16 MiB)
function bk_evaluate(ctx)
  local big = string.rep('x', 17 * 1024 * 1024)
  return {}
end
`,
};

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  if (argv.includes('--teeth')) return teeth();

  if (!existsSync(CORE)) {
    console.error(`missing core binary ${CORE}: cargo build --release --workspace --locked`);
    process.exit(2);
  }
  console.log('building user_layer/examples (momentum_alpha reference dylib)…');
  execFileSync('/bin/bash', ['-c', `cd "${EXAMPLES}" && cargo build --release --locked`], { stdio: 'ignore', timeout: 600000 });
  const dylib = (() => {
    const dir = join(EXAMPLES, 'target', 'release');
    try {
      for (const f of readdirSync(dir)) {
        if (f.startsWith(`lib${REFERENCE_NAME}`) && f.endsWith(`.${DYLIB_EXT}`)) return join(dir, f);
      }
    } catch { /* fall through */ }
    return null;
  })();
  if (!dylib) { console.error(`missing ${REFERENCE_NAME} dylib under user_layer/examples/target/release`); process.exit(2); }

  const gate = createChecks();
  const { check } = gate;
  const temp = mkdtempSync(join(tmpdir(), 'bk-e30-lua-pkg-'));
  const sock = join(tmpdir(), `bk-e30-lua-${process.pid}.sock`);
  const alerts = [];
  let proc = null;

  // The Lua scan dir: the official example (copied byte-for-byte so its
  // manifest signature still holds) plus the four hostile packages.
  const luaDir = join(temp, 'lua');
  cpSync(OFFICIAL_DIR, join(luaDir, OFFICIAL_NAME), { recursive: true });
  for (const [name, script] of Object.entries(HOSTILE)) {
    writePackage(join(luaDir, name), name, script);
  }
  // The IPC-refusal packages live OUTSIDE the scan dir: they must never be
  // auto-loaded, only refused at the ENFORCE site.
  const refuseDir = join(temp, 'refuse');
  const tamperScript = `function bk_evaluate(ctx) return {} end\n`;
  const tamperActual = writePackage(join(refuseDir, 'sb_tamper'), 'sb_tamper', tamperScript);
  const tamperExpected = (tamperActual.slice(0, -1) === '0' ? tamperActual.slice(0, -1) + '1' : tamperActual.slice(0, -1) + '0');
  // Overwrite the manifest with the WRONG (still-hex) digest: now the
  // signature on disk does not vouch for the file.
  writeFileSync(join(refuseDir, 'sb_tamper', 'manifest.json'), JSON.stringify({
    name: 'sb_tamper', version: '0.1.0', api: '1.0', entry: 'strategy.lua', sha256: tamperExpected,
  }, null, 2) + '\n');
  writePackage(join(refuseDir, 'sb_clash'), 'sb_real_name', `function bk_evaluate(ctx) return {} end\n`);
  // A clean package OUTSIDE the scan dir: the IPC strategy.load ENFORCE site
  // must return the (lua) registration receipt for it (acceptance ④).
  writePackage(join(refuseDir, 'sb_fresh'), 'sb_fresh', `function bk_evaluate(ctx) return {} end\n`);

  // The dylib stack in its own dir (the two stacks must never load each
  // other's artifacts; the allow-list covers the dylib root as in E27).
  const libs = join(temp, 'libs');
  mkdirSync(libs, { recursive: true });
  copyFileSync(dylib, join(libs, dylib.split('/').pop()));

  try {
    const boot = await bootCore(sock, ['--strategy-dir', libs, '--lua-strategy-dir', luaDir], {
      ...process.env,
      BLITZKRIEG_STRATEGY_ALLOW_DIRS: libs,
    });
    proc = boot.proc;
    try {
      // §6.4 — the startup scan (LOG-ONLY site): every package registers
      // disabled, the receipts name the `(lua)` stack, and the dylib stack
      // registers WITHOUT it (the stacks cannot load each other's artifacts).
      const lines = () => boot.output.text.split('\n');
      const scanLine = (needle) => lines().find((l) => l.includes('auto-load') && l.includes(needle)) ?? '';
      for (const name of [OFFICIAL_NAME, ...Object.keys(HOSTILE)]) {
        const line = scanLine(name);
        const m = line.match(/auto-load (ok|FAILED): (.*)$/);
        const receipt = m ? m[2] : '';
        const problems = m && m[1] === 'ok'
          ? judgeLoadReceipt(receipt, { name, version: '0.1.0', lua: true })
          : [`auto-load did not succeed for ${name}: ${line.slice(0, 160)}`];
        check(`§6.4 scan registered "${name}" (lua marker, disabled, signed)`, problems.length === 0, problems.join(' | '));
      }
      const dylibOk = lines().some((l) => l.includes(REFERENCE_NAME) && l.includes('registered into the engine dispatch') && !l.includes('(lua)'));
      check(`§6.4 scan registered the dylib "${REFERENCE_NAME}" with NO (lua) marker`, dylibOk,
        lines().filter((l) => l.includes(REFERENCE_NAME)).join(' ⏎ ').slice(0, 200));

      // §8.3 — strategy.list carries BOTH stacks in the SAME row shape.
      const sl = await boot.client.request('strategy.list');
      const rowsAll = sl.strategies ?? [];
      const official = rowsAll.find((r) => r.name === OFFICIAL_NAME);
      const reference = rowsAll.find((r) => r.name === REFERENCE_NAME);
      check('strategy.list carries the lua row and the dylib row', !!official && !!reference,
        rowsAll.map((r) => r.name).join(','));
      if (official && reference) {
        check('§8.3: both stacks surface the SAME five-field row shape',
          judgeRowShape(official).length === 0 && judgeRowShape(reference).length === 0,
          [...judgeRowShape(official), ...judgeRowShape(reference)].join(' | '));
        check('§6.4: the unsigned-past lua row reads compatible (undeclared modes)',
          official.compatible === true && official.modes === null, JSON.stringify(official));
      }

      // §8.2 — the enable handshake: both stacks enable (no declared-modes
      // refusal for the undeclared lua package; the 0.2 dylib as always).
      const enOff = await boot.client.request('strategy.enable', { name: OFFICIAL_NAME, enabled: true });
      check('§8.2: the official lua strategy enables', enOff.found === true && enOff.enabled === true && !enOff.reason, JSON.stringify(enOff).slice(0, 200));
      const enRef = await boot.client.request('strategy.enable', { name: REFERENCE_NAME, enabled: true });
      check('§8.2: the dylib reference strategy enables (0.2 zero-change)', enRef.found === true && enRef.enabled === true && !enRef.reason, JSON.stringify(enRef).slice(0, 200));

      // §6.4 — the IPC `strategy.load` ENFORCE site refuses the two bad
      // packages BEFORE registration … and returns the `(lua)` registration
      // receipt for a clean one (acceptance ④).
      const freshReceipt = await boot.client.request('strategy.load', { path: join(refuseDir, 'sb_fresh') });
      check('§6.4: a clean package loads over IPC with the (lua) receipt (acceptance ④)',
        judgeLoadReceipt(freshReceipt, { name: 'sb_fresh', version: '0.1.0', lua: true }).length === 0,
        String(freshReceipt).slice(0, 200));
      const tamperReceipt = await boot.client.request('strategy.load', { path: join(refuseDir, 'sb_tamper') });
      check('§6.4 F: a tampered sha256 is REFUSED naming both digests',
        judgeRefusalReceipt(tamperReceipt, ['sha256 mismatch', tamperExpected, tamperActual]).length === 0,
        String(tamperReceipt).slice(0, 220));
      const clashReceipt = await boot.client.request('strategy.load', { path: join(refuseDir, 'sb_clash') });
      check('§6.4 G: two identities (dir ≠ manifest name) are REFUSED',
        judgeRefusalReceipt(clashReceipt, ['two identities']).length === 0,
        String(clashReceipt).slice(0, 220));

      // §6.3 — the poison chain, live. Enable all four hostiles; every tick
      // invokes each sandbox. B/C/D must poison (one RISK_ALERT each); A must
      // error forever WITHOUT poisoning. The alert is drained from the poison
      // flag itself, so an arriving alert PROVES the flag was set.
      boot.client.onEvent = (p) => { if (p && p.kind === 'RISK_ALERT') alerts.push(String(p.message ?? '')); };
      for (const name of Object.keys(HOSTILE)) {
        const en = await boot.client.request('strategy.enable', { name, enabled: true });
        check(`§6.3: hostile "${name}" enables (the sandbox is the gate, not the handshake)`,
          en.found === true && en.enabled === true, JSON.stringify(en).slice(0, 160));
      }
      const wantPoison = ['sb_loop', 'sb_pcall', 'sb_grow'];
      await pollUntil(() => wantPoison.every((n) => alerts.some((a) => a.includes(n))), { timeoutMs: 8000 });
      await sleep(300); // grace so an A-poison (a bug) would also have surfaced

      // The §6.3/§6.4 aggregate verdict — the SAME judge the teeth exercise.
      const calls = [
        ...wantPoison.map((n) => ({ name: n, exhausted: true, poisonProven: alerts.some((a) => a.includes(n)) })),
        { name: 'sb_escape', exhausted: false, poisonProven: false },
      ];
      const traceProblems = judgeTrace({ calls, alerts });
      check('§6.3 trace verdict: every exhaustion poisoned, every poison alerted', traceProblems.length === 0, traceProblems.join(' | '));

      // Per-letter acceptance, on the live wire.
      check('B: `while true` poisons — RISK_ALERT names sb_loop with the budget',
        alerts.some((a) => a.includes('sb_loop') && /instruction budget/.test(a)),
        alerts.find((a) => a.includes('sb_loop'))?.slice(0, 200) ?? 'no alert');
      check('C: pcall cannot swallow the poison — RISK_ALERT names sb_pcall',
        alerts.some((a) => a.includes('sb_pcall') && /poisoned/.test(a)),
        alerts.find((a) => a.includes('sb_pcall'))?.slice(0, 200) ?? 'no alert');
      check('D: a 17 MiB allocation poisons — RISK_ALERT names sb_grow',
        alerts.some((a) => a.includes('sb_grow') && /poisoned/.test(a)),
        alerts.find((a) => a.includes('sb_grow'))?.slice(0, 200) ?? 'no alert');
      check('A: the forbidden-surface probe NEVER poisons — no RISK_ALERT names sb_escape',
        !alerts.some((a) => a.includes('sb_escape')), alerts.join(' ⏎ ').slice(0, 200));
      check('§6.3: the official example runs clean — no RISK_ALERT names lua_momentum',
        !alerts.some((a) => a.includes(OFFICIAL_NAME)), '');

      // Quarantine, not a crash: after the poison window the kernel still
      // answers, both stacks still listed, the process still alive.
      const sl2 = await boot.client.request('strategy.list');
      const names2 = (sl2.strategies ?? []).map((r) => r.name);
      check('§6.3: the kernel survives the poison window (list still answers, all six rows)',
        [OFFICIAL_NAME, REFERENCE_NAME, ...Object.keys(HOSTILE)].every((n) => names2.includes(n)),
        names2.join(','));
      check('§6.3: the core process is still alive (quarantine, not a crash)', proc.exitCode === null && proc.signalCode === null,
        `exitCode=${proc.exitCode} signal=${proc.signalCode}`);
      check('§6.3: exactly one RISK_ALERT per poisoned sandbox (the alert is one-shot)',
        wantPoison.every((n) => alerts.filter((a) => a.includes(n)).length === 1),
        alerts.map((a) => a.slice(0, 60)).join(' ⏎ ').slice(0, 220));
    } finally {
      boot.client.stop();
      proc.kill('SIGTERM');
      await sleep(300);
    }
  } finally {
    rmSync(temp, { recursive: true, force: true });
    try { unlinkSync(sock); } catch {}
  }

  const failed = gate.failures;
  console.log(`\nRESULT: ${failed === 0 ? 'PASS' : `FAIL (${failed})`} — §6.3 poison chain live (A never poisons, B/C/D poison once, kernel alive), §6.4 signatures and identities enforced, both stacks on one row shape`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`lua-sandbox-check interrupted: ${String(e.message || e).slice(0, 500)}`);
  process.exit(1);
});
