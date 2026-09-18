#!/usr/bin/env node
/**
 * scale:feed — E9 规模化压测第 2 项 (#59): 40 连接流式行情推送，丢包率必须为 0。
 *
 * 设计：spawn 一个专用 dry 核心（UDS bus 把 core.event 广播给每个 session），
 * 40 条并发 UDS 连接各自建立 session，主线程以 ~9Hz 轮流让每条连接提交一笔
 * 极小的 maker 单（unique internalKey）并立即撤销。每一次被内核接受的提交都会
 * 向 **全部 40 个 session** 扇出 OrderUpdate，所以零丢失判据是：
 *
 *   - 40 条连接全部存活（无一断开）；
 *   - 每条连接收到的事件数**完全相同**（broadcast 对每 session 建_PUSH，
 *     慢消费者会收到 lagged 错误并断开 —— 不会静默丢）；
 *   - 运行期间无 "lagged"；
 *   - 事件数 ≥ 被接受的提交数，且中途无任何提交被拒。
 *
 * 注意：事件中有意不存在原始 Book 推送，所以这里用真实下单产生的
 * OrderUpdate/Fill/PositionClosed 作为扇出负载（见下方下单循环的注释）。
 *
 * 默认 90 秒窗口（CI 友好）+ --full 走满 10 分钟（600s）验收口径。
 */
// Guarded spawn: a core this gate starts must not outlive it (see lib/child-guard.mjs).
import { spawn } from './lib/child-guard.mjs';
import net from 'net';
import { join, resolve, dirname } from 'path';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync, rmSync } from 'fs';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CORE = join(ROOT, 'target', 'release', 'blitzkrieg-core');
const FULL = process.argv.includes('--full');
const CONNS = 40;
const RUN_MS = FULL ? 600_000 : 90_000;
const HZ = 10;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const checks = [];
const check = (name, ok, detail = '') => {
  checks.push({ name, ok });
  console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`);
};

if (!existsSync(CORE)) {
  console.error(`missing core binary: ${CORE} (cargo build --release --workspace --locked)`);
  process.exit(2);
}

const workdir = mkdtempSync(join(tmpdir(), 'blitzkrieg-feedscale-'));
const sock = join(tmpdir(), `blitzkrieg-feedscale-${process.pid}.sock`);
try { unlinkSync(sock); } catch {}

const proc = spawn(CORE, [
  '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
  '--seed-balance', '1000', '--max-order-notional', '6',
  '--engine', '--no-discovery', '--no-event-archive',
  '--no-trade-log', '--no-order-log', '--no-position-log',
  '--round-sec', '3600', '--min-round-age', '0', '--min-time-left', '0',
], { stdio: ['ignore', 'ignore', 'pipe'], cwd: workdir });
let stderr = '';
proc.stderr.on('data', (d) => { stderr += d.toString(); });

await new Promise(async (res) => {
  for (let i = 0; i < 120; i++) { if (existsSync(sock)) break; await sleep(50); }
  res();
});

// ── 40 concurrent UDS sessions ──────────────────────────────────────────────
const clients = [];
let broken = 0;
for (let i = 0; i < CONNS; i++) {
  const sockc = net.connect(sock);
  sockc.on('error', () => { broken++; });
  clients.push(sockc);
  await sleep(30); // stagger accept
}

// Each client: JSONL reader with a dedicated buffer + counted events.
const bufs = clients.map(() => '');
const evCounts = clients.map(() => 0);
let lagged = 0;
const pendingMaps = clients.map(() => new Map());
const SEQ = Symbol('ready');
for (let i = 0; i < clients.length; i++) {
  const c = clients[i];
  c.on('data', (d) => {
    bufs[i] += d.toString();
    let idx;
    while ((idx = bufs[i].indexOf('\n')) >= 0) {
      const line = bufs[i].slice(0, idx); bufs[i] = bufs[i].slice(idx + 1);
      if (!line.trim()) continue;
      let msg; try { msg = JSON.parse(line); } catch { continue; }
      if (msg.method === 'core.event') evCounts[i]++;
      if (String(msg.error?.message || '').toLowerCase().includes('lagged')) lagged++;
      if (msg.id != null && pendingMaps[i].has(msg.id)) {
        const p = pendingMaps[i].get(msg.id); pendingMaps[i].delete(msg.id);
        p.resolve(msg);
      }
    }
  });
}

// handshake on client 0 (all sessions receive events independently).
const rpc0 = (method, params = {}) => new Promise((res) => {
  const id = `h${Date.now()}${Math.random()}`;
  pendingMaps[0].set(id, { resolve: res });
  clients[0].write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
});
const rpcOn = (seat, method, params = {}) => new Promise((res) => {
  const id = `s${seat}_${Math.random()}`;
  pendingMaps[seat].set(id, { resolve: res });
  clients[seat].write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
});
const rpcAll = (method, params = {}) => Promise.all(clients.map((c, i) => new Promise((res) => {
  const id = `a${i}_${Math.random()}`;
  pendingMaps[i].set(id, { resolve: res });
  c.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
})));

for (let i = 0; i < 100; i++) {
  try { await rpc0('core.ping'); break; } catch {}
  await sleep(50);
}
// All 40 sessions complete at least one round-trip (session really alive).
let alive = 0;
try { await rpcAll('core.ping'); alive = clients.length; } catch {}

// ── injection: real kernel orders fan OrderUpdate events to every session ───
// The IPC event bus carries OrderUpdate / Fill / PositionClosed / RiskAlert
// (there is deliberately no raw Book push). Each session cycles orders.place
// rounds with a UNIQUE internal_key; every accepted order produces an
// OrderUpdate notification on ALL 40 sessions via the broadcast bus. Zero
// loss = every session sees exactly the same number of core.event pushes and
// no lagged-drop errors (broadcast drops are loud, never silent).
const durSec = Math.round(RUN_MS / 1000);
console.log(`streaming ${CONNS} connections × ${durSec}s (unique-key order churn @ ~9Hz)`);

const startAt = Date.now();
// Each injection: one session places a tiny resting BUY (unique key). Seats
// rotate so every connection both produces AND consumes bus traffic.
// Balance 1000, price 0.40, size 1 = $0.40 notional; deduper needs unique
// keys (key = seq). Each order is cancelled right after placement so the
// reservation is released — see the arithmetic note below.
//
// The cancel is what keeps this harness honest over a long --full run. A
// $1000 dry seed reserves $0.40 per order, so an uncancelled run stops being
// accepted at exactly 2500 orders (`InsufficientFunds: reserve 0.4 exceeds
// available 0.0`) while still issuing ~5.4k, and the shortfall then shows up
// as a mysterious event-count gap rather than as what it is. Cancelling keeps
// at most CONNS reservations outstanding ($16), and `rejected` below makes
// any refusal a first-class failure instead of a silent one.
const ROUND_MS = 110;
const rounds = Math.floor((RUN_MS - 10_000) / ROUND_MS);
let placeIdx = 0;      // placements issued
let accepted = 0;      // placements the core accepted — each fans out to all sessions
let rejected = 0;      // refusals (dry-fund starvation, risk gates)
const rejectReasons = new Map();
async function runStream() {
  for (let r = 0; r < rounds; r++) {
    const seat = r % CONNS;
    const msg = await rpcOn(seat, 'orders.place', {
      tokenId: 'SCALETOKEN', conditionId: '0x-scale', side: 'buy',
      mode: 'maker', price: 0.4, size: 1, internalKey: `scale-${r}`,
      strategy: 'scale_probe', asset: 'BTC', direction: 'up', roundSlot: 0,
    });
    placeIdx++;
    if (msg?.error) {
      rejected++;
      const key = String(msg.error.message || 'unknown').split(':')[0];
      rejectReasons.set(key, (rejectReasons.get(key) || 0) + 1);
    } else {
      accepted++;
      const orderId = msg?.result?.orderId;
      if (orderId) await rpcOn(seat, 'orders.cancel', { orderId });
    }
    // pace to ~9Hz — arrival is request/response not pushed, so leaks from
    // ordering remain safe: track total bus messages instead.
    const next = startAt + (r + 1) * ROUND_MS;
    const wait = next - Date.now();
    if (wait > 0) await sleep(wait);
  }
}
await runStream();
await sleep(1_500); // drain in-flight notifications

const counts = [...evCounts];
const minC = Math.min(...counts), maxC = Math.max(...counts);
console.log(`\nconnections alive: ${alive}/${CONNS} (broken: ${broken})`);
console.log(`placements issued: ${placeIdx} (accepted ${accepted}, refused ${rejected})`);
console.log(`event pushes per connection: min=${minC} max=${maxC}`);
if (rejectReasons.size) {
  console.log('refusal reasons:', [...rejectReasons.entries()].map(([k, v]) => `${k}×${v}`).join(' | '));
}

check('40 concurrent UDS sessions established', alive === CONNS, `${alive}`);
check('no connection broke mid-stream', broken === 0, `broken=${broken}`);
check('no lagged consumer on the broadcast bus', lagged === 0, `lagged=${lagged}`);
check('every connection received the SAME event push count (zero skew/loss)',
  maxC === minC && minC > 0, `min=${minC} max=${maxC}`);
check('every placement was accepted (no dry-fund/risk starvation mid-run)',
  rejected === 0, `refused=${rejected}${rejected ? ` (${[...rejectReasons.keys()].join(',')})` : ''}`);
check('event count ≥ 1 push per accepted placement (OrderUpdate fanout)',
  minC >= accepted, `min=${minC} vs accepted=${accepted}`);
// Zero loss on the stream itself: the counters must be identical across all
// 40 sessions AND ≥ the number of accepted placements; any bus loss would make
// a lagged error visible (broadcast channel is not lossless-silent).

try { proc.kill(); } catch {}
await sleep(120);
rmSync(workdir, { recursive: true, force: true });

const failed = checks.filter((c) => !c.ok);
console.log(`\nRESULT: ${failed.length === 0 ? 'PASS' : `FAIL (${failed.length})`}`);
process.exit(failed.length === 0 ? 0 : 1);
