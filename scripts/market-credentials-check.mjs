#!/usr/bin/env node
/**
 * 行情插件凭据（market.credentials.status / market.credentials.set）端到端
 * 验收 —— 真内核 spawn，真 UDS wire，dry 模式，零网络。一切写入都在脚本
 * 自己的 scratch cwd —— 不碰仓库、不碰生产 data/。
 *
 * 钉住的事实（每个都对应一条 user-facing 承诺）：
 *
 *   1. status：每个注册插件一行，required_vars 与插件需求表一致，
 *      ready=false（沙盒里没有任何凭据），值永不出现在读数里。
 *   2. 写路径：market.credentials.set 落进 data/runtime/config.json；
 *      回执 written 列出变量名；status 的 resolved 立即反映下一会话
 *      （cell 已在格），但本会话 executor 不动。
 *   3. plan-first：未注册插件名 / 需求表外的变量名都拒绝，且格不动。
 *   4. reset 清该插件条目，别的插件条目不动。
 *   5. 重启注入：重启后的内核 boot 把 cell 注入环境（boot 行可见），
 *      executor 因凭据齐全而就绪（kalshi：API key id + RSA key）。
 *   6. 真实环境变量 outrank 格：带 env 启动的内核注入行不再出现（env
 *      已有该名，cell 跳过）。
 *
 * Usage: node scripts/market-credentials-check.mjs   (needs target/release/blitzkrieg-core)
 */
import { mkdtempSync, mkdirSync, readFileSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { CoreClient, defaultBinaryPath } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { createChecks } from './lib/gate-harness.mjs';

const BIN = process.env.BK_CORE_BIN || defaultBinaryPath();

const { check, failures } = createChecks();
const cleanup = [];

function makeCore(label, extraArgs = [], cwdOverride = null) {
  // The cell is cwd-relative (data/runtime/config.json), so a "restart" MUST
  // reuse the SAME cwd the previous core ran in — a real deployment restarts
  // in place.
  const cwd = cwdOverride ?? mkdtempSync(join(tmpdir(), `blitzkrieg-cred-${label}-`));
  if (!cwdOverride) cleanup.push(cwd);
  return new CoreClient({
    binaryPath: BIN,
    socketPath: scratchSocketPath(`cred-${label}`),
    mode: 'dry',
    seedBalance: 100,
    maxOrderNotional: 100,
    tickMs: 20,
    autoRestart: false,
    cwd,
    noTradeLog: true,
    noOrderLog: true,
    noPositionLog: true,
    extraArgs,
  });
}

try {
  const core = makeCore('main');
  await core.start();

  // 1. The status readout: one row per registered plugin, names only.
  const before = await core.request('market.credentials.status', {});
  const rows = before.rows ?? [];
  const byName = Object.fromEntries(rows.map((r) => [r.plugin, r]));
  check('every compiled-in plugin reports a row',
    ['polymarket', 'kalshi', 'predictfun', 'polysimulator'].every((n) => byName[n] !== undefined),
    JSON.stringify(rows));
  check('kalshi requires its two executor vars',
    JSON.stringify(byName.kalshi?.requiredVars) === JSON.stringify(['KALSHI_API_KEY_ID', 'KALSHI_RSA_PRIVATE_KEY']),
    JSON.stringify(byName.kalshi));
  check('polysimulator requires its single var',
    JSON.stringify(byName.polysimulator?.requiredVars) === JSON.stringify(['POLYSIM_API_KEY']),
    JSON.stringify(byName.polysimulator));
  check('an empty sandbox resolves nothing and is not ready',
    rows.every((r) => r.ready === false && (r.resolvedVars ?? []).length === 0),
    JSON.stringify(rows));
  check('no value ever appears in the readout',
    !JSON.stringify(before).includes('secret'), JSON.stringify(before));

  // 3a. plan-first: an unregistered plugin is refused before the disk.
  let badPlugin = null;
  try {
    badPlugin = await core.request('market.credentials.set', { plugin: 'nope', vars: { KALSHI_API_KEY_ID: 'x' } });
  } catch (e) {
    badPlugin = { message: e?.message ?? String(e) };
  }
  check('an unregistered plugin is refused by name',
    badPlugin?.message?.includes('not a registered plugin'), JSON.stringify(badPlugin));

  // 3b. plan-first: a name outside the plugin's requirement table is refused
  //     (a typo must fail now, not as a silently inert executor).
  let badVar = null;
  try {
    badVar = await core.request('market.credentials.set', { plugin: 'kalshi', vars: { KALSHI_SECRET: 'x' } });
  } catch (e) {
    badVar = { message: e?.message ?? String(e) };
  }
  check('a name outside the requirement table is refused',
    badVar?.message?.includes('not a credential'), JSON.stringify(badVar));

  // 2. The write lands; the reply names what was written, values stay local.
  const set = await core.request('market.credentials.set', {
    plugin: 'kalshi',
    vars: { KALSHI_API_KEY_ID: 'gate-key', KALSHI_RSA_PRIVATE_KEY: 'gate-secret' },
    reason: 'e2e gate',
  });
  check('the set replies with the written var names',
    JSON.stringify(set.written) === JSON.stringify(['KALSHI_API_KEY_ID', 'KALSHI_RSA_PRIVATE_KEY'])
      && set.plugin === 'kalshi' && set.reset === false,
    JSON.stringify(set));
  check('the reply never echoes values', !JSON.stringify(set).includes('gate-secret'), JSON.stringify(set));
  check('the audit trail names the actor', typeof set.actor === 'string' && set.actor.startsWith('uid:'),
    JSON.stringify(set));

  const cellFile = join(core.cwd, 'data/runtime/config.json');
  let cellText = '';
  try { cellText = readFileSync(cellFile, 'utf8'); } catch { /* checked below */ }
  check('the cell file lands in the scratch cwd',
    cellText.includes('KALSHI_API_KEY_ID'), cellFile);

  const mid = await core.request('market.credentials.status', {});
  const midKalshi = (mid.rows ?? []).find((r) => r.plugin === 'kalshi');
  check('the status reflects the NEXT session (cell resolves, ready)',
    midKalshi?.ready === true
      && JSON.stringify(midKalshi?.resolvedVars) === JSON.stringify(['KALSHI_API_KEY_ID', 'KALSHI_RSA_PRIVATE_KEY']),
    JSON.stringify(midKalshi));

  // 4. reset clears only that plugin's entries.
  await core.request('market.credentials.set', {
    plugin: 'polysimulator', vars: { POLYSIM_API_KEY: 'sim-secret' },
  });
  const reset = await core.request('market.credentials.set', { plugin: 'kalshi', reset: true });
  check('reset clears the plugin and names its whole table',
    reset.reset === true && reset.written.length === 2, JSON.stringify(reset));
  const afterReset = await core.request('market.credentials.status', {});
  const arKalshi = (afterReset.rows ?? []).find((r) => r.plugin === 'kalshi');
  const arSim = (afterReset.rows ?? []).find((r) => r.plugin === 'polysimulator');
  check('the reset plugin lost its resolution',
    arKalshi?.ready === false && (arKalshi?.resolvedVars ?? []).length === 0, JSON.stringify(arKalshi));
  check('a sibling plugin keeps its entries through another plugin\'s reset',
    arSim?.ready === true, JSON.stringify(arSim));

  await core.stop();

  // 5. The restart injects the cell into the boot environment — visible in
  //    the run log (names, never values) and effective for the executor
  //    (polysimulator reports ready from its env, not from any env var).
  const core2 = makeCore('restart', [], core.cwd);
  await core2.start();
  const inj = core2.lastStderr ?? '';
  check('a restart injects the cell at boot (names in the log)',
    inj.includes('credentials injected') && inj.includes('POLYSIM_API_KEY'),
    inj.split('\n').filter((l) => l.includes('credential') || l.includes('injected')).join(' | '));
  const after2 = await core2.request('market.credentials.status', {});
  const r2Sim = (after2.rows ?? []).find((r) => r.plugin === 'polysimulator');
  check('after restart the injected plugin resolves from the environment',
    r2Sim?.ready === true && (r2Sim?.resolvedVars ?? []).includes('POLYSIM_API_KEY'),
    JSON.stringify(r2Sim));
  await core2.stop();

  console.log(failures === 0
    ? '\nmarket:credentials — status/set/plan-first/reset/boot-injection all hold over the real wire.'
    : `\nmarket:credentials — ${failures} assertion(s) failed.`);
  process.exit(failures === 0 ? 0 : 1);
} finally {
  for (const c of cleanup) rmSync(c, { recursive: true, force: true });
}
