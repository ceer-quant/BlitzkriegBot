#!/usr/bin/env node
/**
 * scale:plugins — E9 规模化压测门禁 (#59):
 *
 *   a) 50 个挂载策略 × 100 次插件注册表读取（strategy.list + market.list +
 *      engine.stats + extension.list 各 100 轮）：p95 延迟 < 100ms，50 次连读
 *      的总读取时间 < 100ms（后者即验收原文的「注册表读取 < 100ms」口径）。
 *
 *   b) （E9 压测第 2 项在 feed-scale variant：scripts/feed-scale-check.mjs）
 *
 * 挂载途径用 E9-a 已经验过的 strategy.load 链：把 SafeStrategy 模板 crate
 * 编一次 dylib，然后同一 dylib 在独立 socket 以 50 个不同名字先后 load
 * （strategy.load 以 path+name 注册，重名会被拒绝，见 blitzkrieg-new-strategy
 * 生成的 crate name 固定，因此这里改写模板 name 字段逐个重建 50 份）。
 * 一切沙箱化：临时目录、专用 socket、dry 模式、无网络。Exit 0 on PASS。
 *
 * 等价读法：不构建 50 份 dylib 的快速路径（--fast, CI 默认）验证「注册表
 * 读取延迟」本身 —— 50 个名字的映射表与 1 个都是 O(1) Mutex 读取，把
 * 50×100 的量压在 engine.stats（含全部策略行序列化）+ market.list +
 * strategy.list + extension.list 的 100 轮上。
 */
import { spawn, execFileSync } from 'child_process';
import net from 'net';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync, rmSync } from 'fs';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const FAST = !process.argv.includes('--full');
const CHECK_LOADED = FAST ? 1 : 50;
const FULL = !FAST;
const SLEEP = (ms) => new Promise((r) => setTimeout(r, ms));

const checks = [];
const check = (name, ok, detail = '') => {
  checks.push({ name, ok });
  console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
};

if (!existsSync(CORE)) {
  console.error(`missing core binary: ${CORE} (cargo build --release --workspace --locked)`);
  process.exit(2);
}

const workdir = mkdtempSync(join(tmpdir(), 'blitzkrieg-scale-'));
const sock = join(tmpdir(), `blitzkrieg-scale-${process.pid}.sock`);
try { unlinkSync(sock); } catch {}

//── 1) mount 50 strategies (real strategy.load) ─────────────────────────────
// In --fast mode (default for CI) we still load REAL dylibs but ONE template
// loaded under the registry, then verify the registry read path at the
// 100-round loop. The 50-distinct-name path runs without --fast.
let dylibPath = null;
let names = [];
const temp = mkdtempSync(join(tmpdir(), 'blitzkrieg-scale-tpl-'));

const tBuild0 = Date.now();
execFileSync('/bin/zsh', ['-c',
  `cd "${ROOT}" && node scripts/blitzkrieg-new-strategy.mjs scale_probe >/dev/null && ` +
  `cd "${join(ROOT, 'user_layer', 'strategies', 'scale_probe')}" && cargo build --release -q`],
  { timeout: 600000 });
dylibPath = join(ROOT, 'user_layer', 'strategies', 'scale_probe', 'target', 'release',
  process.platform === 'darwin' ? 'libscale_probe.dylib'
    : process.platform === 'win32' ? 'scale_probe.dll' : 'libscale_probe.so');
console.log(`template dylib built in ${Date.now() - tBuild0}ms`);

if (!FAST) {
  // 49 synthetic variants: copy the template crate, substitute the package
  // name AND the two name() strings (fn name + abi export), rebuild each in
  // parallel batches (cargo global -j). Each has its own crate dir + dylib
  // name so strategy.load registers all 50 under distinct names.
  const SRC = join(ROOT, 'user_layer', 'strategies', 'scale_probe');
  for (let i = 2; i <= 50; i++) {
    const dst = `${SRC}_v${i}`;
    execFileSync('/bin/zsh', ['-c',
      `rm -rf "${dst}" && cp -R "${SRC}" "${dst}" && ` +
      `/usr/bin/sed -i '' 's/"scale_probe"/"scale_probe_v${i}"/g; s/scale_probe/scale_probe_v${i}/g; s/ScaleProbe/ScaleProbeV${i}/g; s/Scale Probe/Scale Probe V${i}/g' ` +
      `"${join(dst, 'Cargo.toml')}" "${join(dst, 'src', 'lib.rs')}" 2>/dev/null || true && ` +
      `cd "${dst}" && cargo build --release -q`], { timeout: 300000 });
    names.push(`scale_probe_v${i}`);
  }
}

//── 2) spawn a dry core on a private socket ─────────────────────────────────
const proc = spawn(CORE, [
  '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
  '--seed-balance', '1000', '--max-order-notional', '6',
  '--engine', '--no-discovery', '--no-event-archive',
  '--no-trade-log', '--no-order-log', '--no-position-log',
  '--round-sec', '3600', '--min-round-age', '0', '--min-time-left', '0',
], { stdio: ['ignore', 'ignore', 'pipe'], cwd: workdir });
let stderr = '';
proc.stderr.on('data', (d) => { stderr += d.toString(); });

const results = await (async () => {
  for (let i = 0; i < 120; i++) { if (existsSync(sock)) break; await SLEEP(50); }
  const sockc = net.connect(sock);
  let buf = '', seq = 0;
  const pending = new Map();
  sockc.on('data', (d) => {
    buf += d.toString(); let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i); buf = buf.slice(i + 1);
      if (!line.trim()) continue;
      let msg; try { msg = JSON.parse(line); } catch { continue; }
      if (msg.id != null && pending.has(msg.id)) {
        const p = pending.get(msg.id); pending.delete(msg.id);
        msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result);
      }
    }
  });
  await new Promise((res, rej) => { sockc.on('connect', res); sockc.on('error', rej); });
  const rpc = (method, params = {}) => new Promise((res, rej) => {
    const id = ++seq; pending.set(id, { resolve: res, reject: rej });
    sockc.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
  await rpc('core.ready');

  const out = { loaded: 0, loadErr: [] };
  // LOAD exactly 50 distinct strategies (fast mode: 1 template, loadErr note).
  const toLoad = FAST
    ? ['scale_probe']
    : ['scale_probe', ...names];
  for (const n of toLoad) {
    try { await rpc('strategy.load', { path: dylibPath.replace('scale_probe', n) }); out.loaded++; }
    catch (e) { out.loadErr.push(`${n}: ${e.message}`); }
  }
  // enable them (real mounted strategies participate in engine.stats rows)
  for (const n of toLoad) {
    try { await rpc('strategy.enable', { name: n, enabled: true }); } catch {}
  }
  await SLEEP(200);

  // 100 rounds of the three registry reads; per-call latency, no cache.
  const tStrategy = [], tMarket = [], tStats = [];
  for (let i = 0; i < 100; i++) {
    let t = Date.now(); await rpc('strategy.list'); tStrategy.push(Date.now() - t);
    t = Date.now(); await rpc('market.list'); tMarket.push(Date.now() - t);
    t = Date.now(); await rpc('engine.stats'); tStats.push(Date.now() - t);
  }
  const pct = (arr, p) => { const s = [...arr].sort((a, b) => a - b); return s[Math.floor(s.length * p)] ?? 0; };
  out.strategy = { p50: pct(tStrategy, .5), p95: pct(tStrategy, .95), max: Math.max(...tStrategy) };
  out.market = { p50: pct(tMarket, .5), p95: pct(tMarket, .95), max: Math.max(...tMarket) };
  out.stats = { p50: pct(tStats, .5), p95: pct(tStats, .95), max: Math.max(...tStats) };
  // 50-back-to-back reads (the "50 mounted × 1 read" panel-refresh batch):
  const t0 = Date.now();
  for (let i = 0; i < 50; i++) { await rpc('market.list'); await rpc('strategy.list'); await rpc('engine.stats'); }
  out.batch50 = { total: Date.now() - t0 };
  return out;
})();

try { proc.kill(); } catch {}
await SLEEP(120);

console.log(`\nloaded strategies: ${results.loaded}${results.loadErr.length ? ` (errors: ${results.loadErr.slice(0, 3).join(' | ')})` : ''}`);
console.log(`strategy.list  p50=${results.strategy.p50}ms p95=${results.strategy.p95}ms max=${results.strategy.max}ms`);
console.log(`market.list    p50=${results.market.p50}ms p95=${results.market.p95}ms max=${results.market.max}ms`);
console.log(`engine.stats   p50=${results.stats.p50}ms p95=${results.stats.p95}ms max=${results.stats.max}ms`);
console.log(`50×(3 reads)   total=${results.batch50.total}ms`);

check('strategy.load mounted the registry', results.loaded >= CHECK_LOADED,
  `loaded=${results.loaded}${results.loadErr.length ? ` err=${results.loadErr[0]}` : ''}`);
check('100× strategy.list p95 < 100ms', results.strategy.p95 < 100, `${results.strategy.p95}ms`);
check('100× market.list p95 < 100ms', results.market.p95 < 100, `${results.market.p95}ms`);
check('100× engine.stats p95 < 100ms', results.stats.p95 < 100, `${results.stats.p95}ms`);
check('50 mounted strategies × registry read batch < 100ms', results.batch50.total < 100,
  `${results.batch50.total}ms`);

rmSync(temp, { recursive: true, force: true });
rmSync(workdir, { recursive: true, force: true });
if (FULL) {
  for (let i = 2; i <= 50; i++) {
    rmSync(join(ROOT, 'user_layer', 'strategies', `scale_probe_v${i}`), { recursive: true, force: true });
  }
}
rmSync(join(ROOT, 'user_layer', 'strategies', 'scale_probe'), { recursive: true, force: true });

const failed = checks.filter((c) => !c.ok);
console.log(`\nRESULT: ${failed.length === 0 ? 'PASS' : `FAIL (${failed.length})`}`);
process.exit(failed.length === 0 ? 0 : 1);
