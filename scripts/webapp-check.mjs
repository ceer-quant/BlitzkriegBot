#!/usr/bin/env node
/**
 * webapp-check.mjs — the `ui:webapp` gate.
 *
 * Covers the E8-3 acceptance line 「Vue 构建产物存在 + 快照渲染非空 + token 鉴权
 * 双向 ACCEPT/REJECT」, plus the panel-auth hardening that followed it:
 *
 *   [0] The Vue build artifact exists AND is what the gateway actually serves
 *       (not the fallback built-in HTML) — checked by fetching `/panel` and
 *       matching it against `dist/index.html` on disk.
 *   [1] The Tauri shell crate compiles (cargo check -p blitzkrieg-webapp).
 *   [2] blitzkrieg_ui_kit stays GUI-free: no tauri in its Cargo.toml/deps.
 *   [3] Auth, both directions, over a real socket:
 *         REJECT — no session, bad password, forged token, revoked token
 *         ACCEPT — good credentials, and the issued session on query/header
 *       plus the CORS policy (foreign Origin 403, loopback 200).
 *   [4] Snapshot render is non-empty: the JSON carries real engine state, and
 *       the headless AppViewModel (the Tauri seam) renders a non-empty view.
 *   [4b] The desktop command seam (E8-d) reaches a live core end-to-end:
 *       desktop_snapshot → AppView JSON, desktop_command → CommandOutcome.
 *   [5] The CSRF fix: a cross-site GET cannot reach a lifecycle verb, and the
 *       panel is locked even when no credentials were configured.
 *
 * Exit 0 on PASS, 1 on FAIL.
 */
// Guarded spawn: this gate starts both a gateway and a core directly, so an
// interrupted run used to leave them behind with PPID=1.
import { spawnSync, spawn } from './lib/child-guard.mjs';
import { mkdtempSync, rmSync, existsSync, readFileSync, readdirSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import net from 'net';

const ROOT = process.cwd();
const BIN = join(ROOT, 'target/release/blitzkrieg-core');
const WEB = join(ROOT, 'target/release/ui_kit_web');
const SOCK = join(tmpdir(), `webapp-check-${process.pid}.sock`);
const WORK = mkdtempSync(join(tmpdir(), 'webapp-check-'));
const DIST = join(ROOT, 'ui/webapp/webui/dist');

let failures = 0;
const check = (name, cond, detail = '') => {
  if (!cond) failures++;
  console.log(`  ${cond ? 'ok  ' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// [0] The Vue build artifact must exist, be non-trivial, and be the thing the
//     server hands out. Asserting only that `dist/` exists would pass on a stale
//     or empty build; comparing bytes against what `/panel` returns is what ties
//     the artifact to the product.
const distIndex = join(DIST, 'index.html');
const distHtml = existsSync(distIndex) ? readFileSync(distIndex, 'utf8') : '';
check('Vue build artifact exists (ui/webapp/webui/dist/index.html)',
  distHtml.length > 200, existsSync(distIndex) ? `${distHtml.length} bytes` : 'missing — run `npm run build` in ui/webapp/webui');
check('artifact is a real bundle, not the Vite placeholder',
  /<div id="app">/.test(distHtml) && /<script[^>]+src=/.test(distHtml) && /assets\//.test(distHtml));
const assetsDir = join(DIST, 'assets');
const bundles = existsSync(assetsDir) ? readdirSync(assetsDir) : [];
check('artifact ships hashed JS+CSS assets',
  bundles.some((f) => f.endsWith('.js')) && bundles.some((f) => f.endsWith('.css')),
  `${bundles.length} assets`);

// [0b] The liquid-glass blur must survive minification.
//
// The CSS shipped `-webkit-backdrop-filter` *after* the standard property; the
// minifier treats the two as the same declaration, keeps the last one, and the
// resulting rule had no effect in Chromium — the glass was silently flat, with
// no error anywhere to notice. Assert the *built* CSS still carries the
// unprefixed property, per rule.
{
  const cssFile = bundles.find((f) => f.endsWith('.css'));
  const css = cssFile ? readFileSync(join(assetsDir, cssFile), 'utf8') : '';
  const rule = (sel) => {
    const i = css.indexOf(sel + '{');
    return i < 0 ? '' : css.slice(i, css.indexOf('}', i));
  };
  for (const sel of ['.glass', '.glass-header']) {
    const body = rule(sel);
    check(`${sel} keeps the unprefixed backdrop-filter after minification`,
      /[^-]backdrop-filter:/.test(body.replace(/-webkit-backdrop-filter:[^;]*;?/g, '')),
      body.slice(0, 90) || 'rule missing');
  }
  check('the header glass is blurred harder than a card (liquid-glass tier)',
    /blur\(4\dpx\)/.test(rule('.glass-header')), rule('.glass-header').slice(0, 70));
}

// [1] Tauri crate compiles — its own nested workspace (excluded from the root
//     Cargo workspace), so Linux CI without the GTK headers never touches it.
//     Check on macOS where the GTK deps build fine; assert scaffold files on
//     any OS.
const TAURI_DIR = join(ROOT, 'ui/webapp/src-tauri');
if (process.platform === 'darwin') {
  const tauriCheck = spawnSync('cargo', ['check'], { cwd: TAURI_DIR, encoding: 'utf8' });
  check('tauri crate compiles (own workspace)', tauriCheck?.status === 0, tauriCheck?.stderr?.slice(-140));
} else {
  check('tauri crate compiles (own workspace)',
    existsSync(join(TAURI_DIR, 'Cargo.toml')) && existsSync(join(TAURI_DIR, 'src/main.rs')),
    'non-darwin: scaffold files asserted');
}

// [2] ui_kit has zero GUI deps.
const kitToml = readFileSync(join(ROOT, 'ui/ui_kit/Cargo.toml'), 'utf8');
check('blitzkrieg-ui-kit has no tauri dep', !kitToml.includes('tauri'));

// [3] Gateway auth via a live ui_kit_web with user/password credentials.
const USER = 'gate-admin';
const PASSWORD = 'gate-pass-9f3a';
const web = spawn(WEB, ['--socket', SOCK, '--addr', '127.0.0.1:18997', '--manage'], {
  cwd: WORK, stdio: ['ignore', 'pipe', 'pipe'],
  env: { ...process.env, BLITZKRIEG_PANEL_USER: USER, BLITZKRIEG_PANEL_PASSWORD: PASSWORD },
});
const gotReady = web.stdout[Symbol.asyncIterator]();
let readyLine = '';
for await (const chunk of gotReady) {
  readyLine += chunk.toString();
  if (/listening on/.test(readyLine)) break;
}
check('configured credentials are used as-is (no generated password)',
  /credentials from env/.test(readyLine) && !/password:\s+[0-9a-f]{32}/.test(readyLine),
  readyLine.slice(-160));

// Live core for snapshot through the same token gate. `--engine` matters: it is
// what the supervisor starts a real session with (`--engine --feed-ws`), and
// without it the core answers `engine.stats` with nothing, which would make a
// "snapshot is non-empty" assertion vacuous.
const core = spawn(BIN, ['--socket', SOCK, '--mode', 'dry', '--tick-ms', '100', '--seed-balance', '1000', '--engine', '--no-trade-log', '--no-event-archive'], { cwd: WORK, stdio: 'ignore' });
for (let i = 0; i < 50 && !existsSync(SOCK); i++) await sleep(100);
await sleep(300);

async function httpReq(port, method, path, { headers = '', body = '' } = {}) {
  return new Promise((res) => {
    const c = net.connect(port, '127.0.0.1');
    let b = '';
    c.on('connect', () =>
      c.write(`${method} ${path} HTTP/1.1\r\nHost: t\r\n${headers}Content-Length: ${body.length}\r\nConnection: close\r\n\r\n${body}`));
    c.on('data', (d) => (b += d));
    c.on('end', () => res(b));
    c.on('error', () => res(b));
    setTimeout(() => { try { c.end(); } catch {} res(b); }, 3000);
  });
}
const httpGet = (path, headers = '') => httpReq(18997, 'GET', path, { headers });
const status = (raw) => Number(raw.split('\r\n')[0]?.split(' ')[1]);

try {
  // ── REJECT direction ──────────────────────────────────────────────────────
  check('no session → 401', (await httpGet('/api/snapshot')).startsWith('HTTP/1.1 401'));
  const badLogin = await httpReq(18997, 'POST', '/api/login', {
    body: JSON.stringify({ user: USER, password: 'wrong-pass' }),
  });
  check('wrong password → 401', status(badLogin) === 401);
  check('forged token → 401', status(await httpGet('/api/snapshot?token=' + 'a'.repeat(40))) === 401);

  // ── ACCEPT direction ──────────────────────────────────────────────────────
  const login = await httpReq(18997, 'POST', '/api/login', {
    body: JSON.stringify({ user: USER, password: PASSWORD }),
  });
  const token = /"token":"([0-9a-f]{40})"/.exec(login)?.[1] ?? '';
  check('good credentials → 40-hex token', token.length === 40, token ? '' : login.slice(0, 120));
  const ok = await httpGet(`/api/snapshot?token=${token}`);
  check('session query → 200 snapshot', status(ok) === 200 && /connected|lastError/.test(ok));
  check('session header → 200', status(await httpGet('/api/snapshot', `X-Auth-Token: ${token}\r\n`)) === 200);
  check('session bearer → 200', status(await httpGet('/api/snapshot', `Authorization: Bearer ${token}\r\n`)) === 200);

  // ── CORS policy ───────────────────────────────────────────────────────────
  check('foreign Origin → 403', status(await httpGet(`/api/snapshot?token=${token}`, 'Origin: http://evil.example\r\n')) === 403);
  check('loopback Origin → 200', status(await httpGet(`/api/snapshot?token=${token}`, 'Origin: http://127.0.0.1\r\n')) === 200);
  check('lookalike host is not loopback → 403',
    status(await httpGet(`/api/snapshot?token=${token}`, 'Origin: http://127.0.0.1.evil.example\r\n')) === 403);

  // ── Command surface ───────────────────────────────────────────────────────
  const st = await httpGet(`/api/command?cmd=${encodeURIComponent('status')}&token=${token}`);
  check('command with session → 200', status(st) === 200);
  check('command without session → 401', status(await httpGet('/api/command?cmd=status')) === 401);

  // The CSRF fix: a bare cross-site GET — the shape `<img src="…/api/command
  // ?cmd=stop">` produces — must not reach a lifecycle verb.
  check('cross-site GET /api/command?cmd=stop → 401',
    status(await httpGet('/api/command?cmd=stop')) === 401);

  // ── Liveness probe ────────────────────────────────────────────────────────
  const ping = await httpGet('/api/ping');
  check('/api/ping answers without a session', status(ping) === 200);
  check('/api/ping reports auth is required', /"authRequired":true/.test(ping));
  check('/api/ping leaks no state',
    !/balance|positions|tradeRows|strategyStats/.test(ping), ping.slice(0, 100));

  // ── Revocation ────────────────────────────────────────────────────────────
  await httpGet(`/api/logout?token=${token}`);
  check('revoked token → 401 afterwards', status(await httpGet(`/api/snapshot?token=${token}`)) === 401);

  // ── [0]/[4] The served panel IS the built Vue app, and the snapshot renders ─
  const panel = await httpGet('/panel');
  check('/panel → 200', status(panel) === 200);
  const panelBody = panel.slice(panel.indexOf('\r\n\r\n') + 4);
  check('served /panel is the built Vue bundle',
    /<div id="app">/.test(panelBody) && /assets\/index-/.test(panelBody),
    `${panelBody.length} bytes served`);
  check('served /panel is byte-identical to dist/index.html',
    panelBody.trim() === distHtml.trim(), `${panelBody.length} served vs ${distHtml.length} on disk`);

  // ── Brand assets survive the wire ─────────────────────────────────────────
  //
  // The panel ships binary PNG icons next to its text assets, and the static
  // route used to hand the file body through `String::from_utf8_lossy` — which
  // replaces every non-ASCII byte and so inflated a 3 KB icon to 5.5 KB of
  // mojibake. Compare bytes, not length: a wrong-but-similar size must fail too.
  const brand = ['favicon-32.png', 'favicon-16.png', 'apple-touch-icon.png'];
  for (const name of brand) {
    const disk = readFileSync(join(DIST, name));
    const res = await fetch(`http://127.0.0.1:18997/panel/${name}`);
    const wire = Buffer.from(await res.arrayBuffer());
    check(`${name} is served byte-identical (${disk.length} B)`,
      wire.length === disk.length && wire.equals(disk),
      `status=${res.status} type=${res.headers.get('content-type')} wire=${wire.length}`);
  }
  // The hashed logo the Vue bundle imports, whatever hash Vite gave it.
  const logoName = readdirSync(join(DIST, 'assets')).find((f) => /^logo-.*\.png$/.test(f));
  if (logoName) {
    const disk = readFileSync(join(DIST, 'assets', logoName));
    const res = await fetch(`http://127.0.0.1:18997/panel/assets/${logoName}`);
    const wire = Buffer.from(await res.arrayBuffer());
    check(`bundled ${logoName} is served as a real PNG (${disk.length} B)`,
      wire.equals(disk) && wire.subarray(1, 4).toString() === 'PNG',
      `status=${res.status} wire=${wire.length} magic=${wire.subarray(1, 4).toString()}`);
    // And the app actually references it, so the asset is not merely present.
    const js = readdirSync(join(DIST, 'assets')).find((f) => /^index-.*\.js$/.test(f));
    check('the bundle references the logo asset',
      !!js && readFileSync(join(DIST, 'assets', js), 'utf8').includes(logoName), `js=${js}`);
  } else {
    check('the bundle emits a hashed logo asset', false, 'no logo-*.png in dist/assets');
  }

  const body = ok.slice(ok.indexOf('\r\n\r\n') + 4);
  let snap = {};
  try { snap = JSON.parse(body); } catch {}
  // Non-empty render: real state, not a stub. These are the fields the Vue app
  // actually binds, so if they are populated the panel has something to draw.
  check('snapshot reports a live connection', snap.connected === true, String(snap.connected));
  check('snapshot carries a mode', typeof snap.mode === 'string' && snap.mode.length > 0, String(snap.mode));
  check('snapshot carries a balance block with a finite figure',
    snap.balance && Number.isFinite(Number(snap.balance.balance)),
    JSON.stringify(snap.balance));
  check('snapshot carries the dry seed (what the balance card reconciles against)',
    snap.balance?.seed != null, JSON.stringify(snap.balance?.seed));
  check('snapshot carries the render arrays',
    Array.isArray(snap.tradeRows) && Array.isArray(snap.strategyStats) && Array.isArray(snap.marketPlugins),
    `tradeRows=${snap.tradeRows?.length} strategyStats=${snap.strategyStats?.length} marketPlugins=${snap.marketPlugins?.length}`);
  check('snapshot carries engine counters (--engine)',
    snap.stats && typeof snap.stats === 'object' && Object.keys(snap.stats).length > 0,
    `statsKeys=${Object.keys(snap.stats ?? {}).length}`);
  check('snapshot carries the gateway block', snap.gateway && typeof snap.gateway === 'object', JSON.stringify(snap.gateway));
  check('--manage reports lifecycleEnabled', snap.gateway?.lifecycleEnabled === true, JSON.stringify(snap.gateway));
  // This harness spawns the core ITSELF, so it is the "adopted core" case: the
  // verbs are accepted, but stop cannot act on a core the gateway did not spawn.
  // The panel gates its buttons on both flags, so `managed:false` must reach the
  // wire.
  check('an adopted core reports managed:false', snap.gateway?.managed === false, JSON.stringify(snap.gateway));

  // Headless AppViewModel (the Tauri seam) renders offline-safe.
  const vmCheck = spawnSync('cargo',
    ['test', '--release', '-p', 'blitzkrieg-ui-kit', '--lib', 'app::tests::headless'],
    { cwd: ROOT, encoding: 'utf8' });
  check('headless AppViewModel renders offline-safe (ui_kit app adapter)',
    vmCheck?.status === 0, (vmCheck?.stderr ?? '').slice(-140));

  // Desktop command seam (E8-d): desktop_snapshot / desktop_command against a
  // REAL dry core — socket → IpcClient → AppViewModel JSON, socket →
  // Dispatcher → CommandOutcome. The test spawns its own core; the gate only
  // tells it where this gate's core binary lives. Darwin only: the tauri
  // crate is a nested workspace Linux never builds.
  if (process.platform === 'darwin') {
    const chainCheck = spawnSync('cargo', ['test', '--test', 'chain'], {
      cwd: TAURI_DIR, encoding: 'utf8', timeout: 420000,
      env: { ...process.env, BLITZKRIEG_CORE_BIN: BIN },
    });
    check('desktop_snapshot/desktop_command reach a live core (tauri seam)',
      chainCheck?.status === 0,
      ((chainCheck?.stderr ?? '') + (chainCheck?.stdout ?? '')).slice(-160));
  } else {
    check('desktop_snapshot/desktop_command reach a live core (tauri seam)',
      existsSync(join(TAURI_DIR, 'tests/chain.rs')),
      'non-darwin: test scaffold asserted');
  }

  // ── [5] No configured credentials → refuse to start ───────────────────────
  //
  // The configuration that was exploitable during acceptance: no env vars, so
  // the old code served `/api/command` openly on loopback. Credentials now come
  // from the environment only — the process must not invent a password, because
  // then it, and not the operator's secret store, decides who can stop the core.
  const web2 = spawn(WEB, ['--socket', SOCK, '--addr', '127.0.0.1:18998', '--manage'], {
    cwd: WORK, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, BLITZKRIEG_PANEL_USER: '', BLITZKRIEG_PANEL_PASSWORD: '' },
  });
  try {
    let out = '';
    let err = '';
    web2.stderr.on('data', (d) => { err += d.toString(); });
    const it2 = web2.stdout[Symbol.asyncIterator]();
    for await (const chunk of it2) {
      out += chunk.toString();
      if (/listening on/.test(out)) break;
    }
    const exited = await Promise.race([
      new Promise((res) => web2.on('exit', (code) => res(code))),
      sleep(3000).then(() => null),
    ]);
    const combined = out + err;
    check('no credentials configured → gateway refuses to start',
      exited === 2, `exit=${exited} out=${combined.slice(-200)}`);
    check('the refusal names both env vars it needs',
      /BLITZKRIEG_PANEL_USER/.test(combined) && /BLITZKRIEG_PANEL_PASSWORD/.test(combined),
      combined.slice(-200));
    check('no password is generated by the process',
      !/password:\s+[0-9a-f]{32}/.test(combined), combined.slice(-200));
    check('and it never claimed to be listening',
      !/listening on/.test(out));
    // Nothing is bound, so nothing can be reached on that port either. A refused
    // connection yields no bytes at all, so there is no status line to parse —
    // that absence *is* the pass condition. A live socket would answer 200/401.
    const deadRaw = await Promise.race([
      httpReq(18998, 'GET', '/api/snapshot'),
      sleep(3000).then(() => 'TIMEOUT'),
    ]);
    const dead = /^HTTP\/1\.1 \d{3}/.test(deadRaw) ? status(deadRaw) : -1;
    check('the refused gateway leaves nothing listening on its port',
      dead === -1, `bye=${JSON.stringify(deadRaw.slice(0, 40))}`);
  } finally {
    try { web2.kill('SIGKILL'); } catch {}
  }
} finally {
  try { core.kill('SIGKILL'); } catch {}
  try { web.kill('SIGKILL'); } catch {}
  try { rmSync(WORK, { recursive: true, force: true }); } catch {}
}

console.log(failures === 0 ? '\nRESULT: PASS' : `\nRESULT: FAIL (${failures})`);
process.exit(failures === 0 ? 0 : 1);
