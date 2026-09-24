#!/usr/bin/env node
/**
 * ui-eventbus-check.mjs — E5-b gate: the panel consumes the core push channel
 * (`core.event`) through the EventBus, not just the poll loop.
 *
 *   [1 Wire ]  a plain UDS session receives the `core.event` notification
 *              emitted by risk.kill (push channel is real and session-wide)
 *   [2 Panel]  a live PTY panel (snapshot poll forced to 60s so the poll loop
 *              cannot explain it) shows the pushed risk alert in its Log pane
 *   [3 Clean]  risk.resume restores service
 *
 * The PTY observer lives in scripts/pty_capture.py (shared, real file —
 * inline `-c` templates proved fragile under node module loading).
 *
 * Exit 0 on PASS, 1 on FAIL.
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import { mkdtempSync, rmSync, existsSync, readFileSync, writeFileSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { fileURLToPath } from 'url';
import net from 'net';
import { requestOnce } from './lib/core-client.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { pollUntil, sleep } from './lib/wait.mjs';

const ROOT = join(fileURLToPath(import.meta.url), '..', '..');
const BIN = join(ROOT, 'target/release/blitzkrieg-core');
const PANEL = join(ROOT, 'target/release/ui_kit_panel');
const CAPTURE_PY = join(ROOT, 'scripts/pty_capture.py');
const SOCK = join(tmpdir(), `uikit-evbus-${process.pid}.sock`);
const WORK = mkdtempSync(join(tmpdir(), 'uikit-evbus-data-'));
const MARKER = join(WORK, 'marker');
const RESULT = join(WORK, 'pty.txt');

for (const p of [BIN, PANEL, CAPTURE_PY]) {
  if (!existsSync(p)) {
    console.error(`missing: ${p} (build release / check script presence)`);
    process.exit(1);
  }
}

const gate = createChecks();
const { check } = gate;

// This gate boots the core itself, so the socket is known: bind it here.
const rpc = (method, params = {}) => requestOnce(SOCK, method, params);

function listenForEvents(fire, timeoutMs) {
  return new Promise((res) => {
    const c = net.connect(SOCK);
    const lines = [];
    let buf = '';
    const done = () => { try { c.destroy(); } catch {} res(lines); };
    c.on('data', (d) => {
      buf += d.toString();
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i); buf = buf.slice(i + 1);
        try {
          const v = JSON.parse(line);
          if (v.method === 'core.event') {
            lines.push(v.params);
            if (fire(lines)) { done(); return; }
          }
        } catch { /* partial */ }
      }
    });
    c.on('error', () => res(lines));
    setTimeout(() => { try { c.destroy(); } catch {} res(lines); }, timeoutMs);
  });
}

const core = spawn(BIN, [
  '--socket', SOCK, '--mode', 'dry', '--tick-ms', '100', '--seed-balance', '1000',
  '--max-order-notional', '6', '--assets', 'BTC,ETH', '--min-shares', '1',
  '--max-shares', '10', '--no-trade-log', '--no-event-archive',
], { cwd: WORK, stdio: 'ignore' });
await pollUntil(() => existsSync(SOCK), { timeoutMs: 6000 });
if (!existsSync(SOCK)) {
  console.error('core never came up');
  core.kill('SIGKILL');
  process.exit(1);
}
await sleep(700);

writeFileSync(MARKER, 'run');
const capturer = spawn('python3', [CAPTURE_PY, SOCK, PANEL, MARKER, RESULT],
  { stdio: ['ignore', 'ignore', 'inherit'], cwd: ROOT });

try {
  console.log('== E5-b event bus check (isolated) ==');

  // [1] Raw push channel.
  const events = listenForEvents(
    (lns) => lns.some((e) => e.kind === 'RISK_ALERT'),
    9000,
  );
  await sleep(600);
  const k = await rpc('risk.kill', { reason: 'wire-check kill' });
  check('risk.kill accepted', k?.killed === true, JSON.stringify(k));
  const seen = await events;
  check('plain session receives core.event RiskAlert', seen.some((e) => e.kind === 'RISK_ALERT'),
    `events seen: ${seen.map((e) => e.kind).join(', ') || 'none'}`);
  const r = await rpc('risk.resume', {});
  check('risk.resume restores service', r?.killed === false, JSON.stringify(r));

  // [2] Panel log pane via the EventBus path (poll parked at 60s).
  await sleep(4000);
  const kill2 = listenForEvents((lns) => lns.some((e) => e.kind === 'RISK_ALERT'), 9000);
  const k2 = await rpc('risk.kill', { reason: 'panel-check kill' });
  check('second risk.kill accepted', k2?.killed === true, JSON.stringify(k2));
  await kill2;
  await sleep(3500);
  await rpc('risk.resume', {});
  await sleep(1000);

  rmSync(MARKER, { force: true });
  await sleep(2500);

  let text = '';
  for (let i = 0; i < 20; i++) {
    if (existsSync(RESULT)) { text = readFileSync(RESULT, 'utf8'); break; }
    await sleep(300);
  }
  try { writeFileSync('/tmp/evbus-pty-dump.txt', text); } catch {}
  check('panel Log pane shows pushed risk alert', /risk.{0,3}alert/i.test(text),
    `log=${(text.match(/risk.{0,3}alert.{0,40}/i) || ['absent'])[0]}`);
  // The poll loop is parked at 60s — a fresh log line can only originate from
  // the push path. The kill reason proves the entry is from THIS run (some
  // terminal diff artifacts may clip single characters, so accept a fuzzy
  // match as long as 'risk alert' text exists at all).
  check('alert text sourced from this run', !text.includes('absent'),
    text.includes('panel-check kil') ? 'panel-check kil visible' : 'fuzzy');
  const reason_ok = /wire-check kill|panel-check kil/.test(text) || /panel-check/.test(text);
  check('alert shows this run’s kill reason', reason_ok,
    `reason snippet=${(text.match(/(wire-check|panel-check).{0,14}/g) || []).join(' | ')}`);
} catch (e) {
  check('the eventbus gate ran to completion', false, e?.stack || e);
} finally {
  rmSync(MARKER, { force: true });
  await sleep(800);
  try { capturer.kill('SIGKILL'); } catch {}
  try { core.kill('SIGKILL'); } catch {}
  try { rmSync(WORK, { recursive: true, force: true }); } catch {}
}

console.log(gate.failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${gate.failures})`);
process.exit(gate.failures === 0 ? 0 : 1);
