#!/usr/bin/env node
/**
 * ui-plugin-check.mjs — E5-a acceptance: the UI Kit gateway exposes the plugin
 * command surface (strategies / strategy on|off / extensions / extension on|off
 * / markets) against a live core on a private socket.
 *
 * Covers #32's acceptance:
 *   - reads return the three registries (strategies, extensions, market plugins
 *     with capability flags + active marker)
 *   - a runtime toggle round-trips: strategy parity on → listed on → off again →
 *     listed off (boot state untouched)
 *   - unknown names / malformed verbs are rejected cleanly
 *   - every verb still works with NO core (graceful degradation)
 *
 * The strategy registry this gate reads is filled by a LOADED CDYLIB, not by the
 * kernel: the kernel registers no strategy of its own, so a core started here
 * with no `--strategy-dir` would list nothing and the toggle round-trip would
 * have nothing to flip. `--strategy-dir` therefore points at the reference
 * implementation (`user_layer/parity_strategy`), which CI builds for this gate.
 *
 * Isolation: private UDS, scratch workdir, dry mode, no logs/archives.
 * Exit 0 on PASS, 1 on FAIL.
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { existsSync } from 'fs';

const ROOT = process.cwd();
const BIN = join(ROOT, 'target/release/blitzkrieg-core');
const WEB = join(ROOT, 'target/release/ui_kit_web');
const REF_DYLIB_DIR = join(ROOT, 'user_layer/parity_strategy/target/release');
const REF_STRATEGY = 'parity'; // the name the reference cdylib registers
// CI runs this on Linux; a hardcoded `.dylib` would only ever pass on macOS.
const REF_DYLIB = join(
  REF_DYLIB_DIR,
  `libparity_strategy.${process.platform === 'darwin' ? 'dylib' : process.platform === 'win32' ? 'dll' : 'so'}`,
);
const SOCK = join(tmpdir(), `uikit-plugins-${process.pid}.sock`);
const WORK = mkdtempSync(join(tmpdir(), 'uikit-plugins-data-'));
const PORT = 18993;
const BASE = `http://127.0.0.1:${PORT}`;

for (const p of [BIN, WEB]) {
  if (!existsSync(p)) {
    console.error(`missing binary: ${p} (run: cargo build --release)`);
    process.exit(1);
  }
}
if (!existsSync(REF_DYLIB)) {
  console.error(`missing reference cdylib: ${REF_DYLIB}`);
  console.error('  (run: cd user_layer/parity_strategy && cargo build --release --locked)');
  console.error('  the strategy registry below is only non-empty because a cdylib is loaded —');
  console.error('  the kernel registers no strategy of its own.');
  process.exit(1);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function cmd(text) {
  const url = `${BASE}/api/command?cmd=${encodeURIComponent(text)}`;
  const res = await fetch(url, { headers: TOKEN ? { 'X-Auth-Token': TOKEN } : {} });
  return res.json();
}

let gateway = null;
let failures = 0;
let TOKEN = '';
function check(name, cond, detail = '') {
  const tag = cond ? 'ok  ' : 'FAIL';
  if (!cond) failures++;
  console.log(`  ${tag} ${name}${detail ? ` — ${detail}` : ''}`);
}

async function waitFor(label, fn, timeoutMs = 20000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const v = await fn();
      if (v) return v;
    } catch { /* retry */ }
    await sleep(200);
  }
  throw new Error(`timeout waiting for ${label}`);
}

try {
  console.log('== UI Kit plugin manager check (isolated) ==');
  console.log(`   socket: ${SOCK}`);

  // [0] graceful degradation: all plugin verbs error cleanly with NO core.
  gateway = spawn(WEB, ['--socket', SOCK, '--addr', `127.0.0.1:${PORT}`, '--manage'], {
    cwd: ROOT,
    env: {
      ...process.env,
      UIKIT_CORE_BIN: BIN,
      UIKIT_CORE_CWD: WORK,
      // Quoted: this checkout's path contains a space, and the gateway splits
      // UIKIT_CORE_EXTRA_ARGS on whitespace outside quotes.
      UIKIT_CORE_EXTRA_ARGS: `--engine --feed-ws --strategy-dir "${REF_DYLIB_DIR}"`,
      DRY_RUN: 'true',
      // Gateway mode requires an explicit credential pair since #83.
      BLITZKRIEG_PANEL_USER: 'gate-admin',
      BLITZKRIEG_PANEL_PASSWORD: 'gate-pass-9f3a',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  gateway.stderr.on('data', (d) => process.stdout.write(`   [gw:err] ${d}`));
  // Since #83, /api/* requires a session — the probe gets 401 until we log in.
  // "Any HTTP response" is the listening signal, not `.ok`.
  await waitFor('gateway HTTP', async () => {
    const r = await fetch(`${BASE}/api/snapshot`);
    return r.status > 0 ? r : null;
  });
  const loginRes = await fetch(`${BASE}/api/login`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ user: 'gate-admin', password: 'gate-pass-9f3a' }),
  });
  const loginBody = await loginRes.text();
  TOKEN = /"token":"([0-9a-f]{40})"/.exec(loginBody)?.[1] ?? '';
  check('login issues a 40-hex token', TOKEN.length === 40, loginBody.slice(0, 120));

  const off = await cmd('strategies');
  check('strategies without core = clean error', off.ok === false && /not reachable/.test(off.message), off.message);
  const off2 = await cmd('markets');
  check('markets without core = clean error', off2.ok === false, off2.message);
  const off3 = await cmd('extension foo on');
  check('extension toggle without core = clean error', off3.ok === false, off3.message);

  // [1] start an isolated dry core whose only strategy is the reference cdylib.
  const started = await cmd('start BTC,ETH --dry-run');
  check('core started', started.ok === true, started.message);
  await waitFor('core connected', async () => {
    const r = await cmd('status');
    return r.ok && r.data?.connection?.connected ? r : null;
  });

  // [2] strategies list: exactly the loaded library — nothing built in.
  const st = await cmd('strategies');
  check('strategies ok', st.ok === true, st.message.split('\n')[0]);
  const rows = st.data.strategies ?? [];
  const names = rows.map((r) => r.name);
  check(`the loaded cdylib is listed: ${REF_STRATEGY}`, names.includes(REF_STRATEGY), names.join(','));
  check('the kernel registers no strategy of its own', names.join(',') === REF_STRATEGY, names.join(','));
  // Zero-default (the kernel couples to no strategy): a fresh boot lists every
  // registered strategy DISABLED; what trades is purely the operator's choice.
  check('fresh boot enables nothing', rows.filter((r) => r.enabled).map((r) => r.name).join(',') === '');

  // [3] runtime toggle round-trip: parity on → on → off → off.
  const t1 = await cmd(`strategy ${REF_STRATEGY} on`);
  check('strategy toggle on ok', t1.ok === true && t1.data?.found === true, JSON.stringify(t1.data));
  const st2 = await cmd('strategies');
  check('parity now on', st2.data.strategies.find((r) => r.name === REF_STRATEGY)?.enabled === true);
  const t2 = await cmd(`strategy ${REF_STRATEGY} off`);
  check('strategy toggle off ok', t2.ok === true, t2.message);
  const st3 = await cmd('strategies');
  check('parity off again', st3.data.strategies.find((r) => r.name === REF_STRATEGY)?.enabled === false);
  check('no strategy left enabled by the toggle', st3.data.strategies.every((r) => r.enabled === false));

  // [4] malformed / unknown are clean errors.
  const bad1 = await cmd('strategy no_such on');
  check('unknown strategy name rejected (found=false)', bad1.ok === true && bad1.data?.found === false, bad1.message);
  const bad2 = await cmd('strategie x on');
  check('unknown verb rejected', bad2.ok === false && /unknown command/.test(bad2.message), bad2.message);

  // [5] markets list: plugins with capability flags + active marker.
  const mk = await cmd('markets');
  check('markets ok', mk.ok === true && typeof mk.data.active === 'boolean', mk.message.split('\n')[0]);
  check('market plugin present (polymarket)', (mk.data.plugins ?? []).some((p) => /polymarket/i.test(p.name)), (mk.data.plugins ?? []).map((p) => p.name).join(','));
  const pm = (mk.data.plugins ?? []).find((p) => /polymarket/i.test(p));
  if (pm) {
    check('plugin capability flags are booleans', ['hasDataFeed', 'hasDiscovery', 'hasExecutor', 'enabled', 'active'].every((k) => typeof pm[k] === 'boolean'));
  }

  // [6] extensions list (may be empty in a fresh core, must not error).
  const ex = await cmd('extensions');
  check('extensions ok', ex.ok === true && Array.isArray(ex.data.extensions), ex.message.split('\n')[0]);

  // [7] stop; core is gone.
  const stopped = await cmd('stop');
  check('stop ok', stopped.ok === true, stopped.message);

  console.log(`\nRESULT: ${failures === 0 ? 'PASS' : `FAIL (${failures})`}`);
  process.exitCode = failures === 0 ? 0 : 1;
} catch (e) {
  console.error(`\nRESULT: FAIL — ${e.message}`);
  process.exitCode = 1;
} finally {
  try { gateway?.kill('SIGKILL'); } catch { /* noop */ }
  try { rmSync(WORK, { recursive: true, force: true }); } catch { /* noop */ }
}
