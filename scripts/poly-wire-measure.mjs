#!/usr/bin/env node
/**
 * Measure the Polymarket market-channel wire bytes the bot's subscription
 * actually receives.
 *
 * The archive on disk is a steady ~13 GB/day, but a much larger VPN bill
 * implied traffic the archive never sees. This probe subscribes to the same
 * assets the bot does, over the same channel, and counts raw bytes off the
 * socket — so the wire rate is measured rather than inferred from what was
 * persisted.
 *
 * Usage:
 *   node scripts/poly-wire-measure.mjs [seconds] [assets]
 *     seconds  how long to sample (default 120)
 *     assets   comma list (default btc,eth,sol,xrp — the bot's set)
 *
 * Reports bytes/sec and the bytes/day extrapolation, broken down by message
 * type so it is visible which message class dominates.
 */
import WebSocket from 'ws';

const SECONDS = Number(process.argv[2] ?? 120);
const ASSETS = (process.argv[3] ?? 'btc,eth,sol,xrp').split(',');
const ROUND_SECONDS = 900; // 15-minute rounds

/** Both tokens (up/down) of the round currently open for `asset`. */
async function tokensFor(asset) {
  const slot = Math.floor(Date.now() / 1000 / ROUND_SECONDS) * ROUND_SECONDS;
  const slug = `${asset}-updown-15m-${slot}`;
  const r = await fetch(`https://gamma-api.polymarket.com/markets?slug=${slug}`, {
    signal: AbortSignal.timeout(8000),
  });
  if (!r.ok) throw new Error(`gamma ${slug}: HTTP ${r.status}`);
  const [m] = await r.json();
  if (!m) throw new Error(`gamma ${slug}: no market`);
  return JSON.parse(m.clobTokenIds || '[]');
}

const assetTokens = {};
for (const a of ASSETS) {
  try {
    assetTokens[a] = await tokensFor(a);
  } catch (e) {
    console.error(`  ! ${a}: ${e.message}`);
  }
}
const ids = Object.values(assetTokens).flat().filter(Boolean);
if (!ids.length) {
  console.error('no token ids resolved; nothing to measure');
  process.exit(1);
}
console.log(`subscribing ${ids.length} tokens (${ASSETS.join(',')}) for ${SECONDS}s`);

const byType = new Map();
let bytes = 0;
let frames = 0;
let openedAt = 0;
let closedAt = 0;
let closeCode = null;
let closeReason = '';

const ws = new WebSocket('wss://ws-subscriptions-clob.polymarket.com/ws/market');

ws.on('open', () => {
  openedAt = Date.now();
  ws.send(JSON.stringify({ assets_ids: ids, type: 'market', initial_dump: true }));
  console.log('subscribed; sampling...');
});

ws.on('message', (data, isBinary) => {
  // Count what actually crossed the wire, not a re-encode: `data` is the raw
  // payload, so its length is the byte count the socket delivered.
  bytes += data.length;
  frames++;
  let label = 'unknown';
  try {
    const v = JSON.parse(data.toString());
    const first = Array.isArray(v) ? v[0] : v;
    label = first?.event_type ?? first?.type ?? 'unlabeled';
  } catch {
    label = 'unparseable';
  }
  const b = byType.get(label) ?? { n: 0, bytes: 0 };
  b.n++;
  b.bytes += data.length;
  byType.set(label, b);
});

// The rate is bursty enough that a single average hides the shape, so sample a
// 10-second series alongside it: an average that is high because of one burst
// and one that is high because of sustained flow imply different fixes.
const series = [];
const sampler = setInterval(() => {
  if (!openedAt) return;
  series.push({ t: (Date.now() - openedAt) / 1000, bytes });
}, 10_000);

ws.on('error', (e) => console.error('ws error:', e.message));
ws.on('close', (c, reason) => {
  closedAt = Date.now();
  closeCode = c;
  closeReason = reason?.toString() ?? '';
  console.log(`ws closed: ${c} "${closeReason}"`);
});

await new Promise((r) => setTimeout(r, SECONDS * 1000));
clearInterval(sampler);
// A venue-initiated close must not be averaged over time the socket was
// already dead for, or the rate reads far lower than the wire actually saw.
const endAt = closedAt || Date.now();
const elapsed = (endAt - openedAt) / 1000;
try { ws.close(); } catch {}

const perSec = elapsed > 0 ? bytes / elapsed : 0;
console.log('');
console.log(`frames: ${frames} | bytes: ${bytes} | socket open: ${elapsed.toFixed(1)}s of ${SECONDS}s`);
if (closedAt) console.log(`venue closed it early: code ${closeCode} "${closeReason}"`);
console.log(`rate:   ${(perSec / 1024).toFixed(1)} KiB/s  =  ${(perSec * 86400 / 1e9).toFixed(2)} GB/day`);
if (series.length > 1) {
  console.log('');
  console.log('10-second rate series (GB/day equivalent):');
  const rates = [];
  for (let i = 1; i < series.length; i++) {
    const db = series[i].bytes - series[i - 1].bytes;
    const dt = series[i].t - series[i - 1].t;
    if (dt > 0) rates.push(db / dt);
  }
  console.log('  ' + rates.map((r) => (r * 86400 / 1e9).toFixed(1)).join(' '));
  if (rates.length) {
    const sorted = [...rates].sort((a, b) => a - b);
    console.log(
      `  min ${(sorted[0] * 86400 / 1e9).toFixed(1)} | median ${(sorted[Math.floor(sorted.length / 2)] * 86400 / 1e9).toFixed(1)} | max ${(sorted[sorted.length - 1] * 86400 / 1e9).toFixed(1)} GB/day`
    );
  }
}
console.log('');
console.log('by message type:');
for (const [label, b] of [...byType.entries()].sort((x, y) => y[1].bytes - x[1].bytes)) {
  const share = bytes > 0 ? (b.bytes * 100 / bytes).toFixed(1) : '0.0';
  const avg = b.n > 0 ? Math.round(b.bytes / b.n) : 0;
  console.log(
    `  ${label.padEnd(22)} ${String(b.n).padStart(7)} frames  ` +
      `${(b.bytes / 1024).toFixed(1).padStart(9)} KiB  ${share.padStart(5)}%  avg ${avg} B`
  );
}
