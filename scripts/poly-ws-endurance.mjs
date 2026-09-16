#!/usr/bin/env node
/**
 * Does a single clean Polymarket market subscription survive, and for how
 * long does the server allow it?
 *
 * The bot's log shows the venue closing the channel roughly every two minutes,
 * and the SDK reconnect path re-subscribes every tracked token with a full book
 * dump each time. Distinguishing "the venue sheds load on its own schedule"
 * from "the bot's accumulated connections provoke it" matters: the first needs
 * a reconnect strategy, the second is a bug the bot creates.
 *
 * So: hold one subscription, count the bytes, and report exactly when (and with
 * which close code) the venue ends it. Run it twice concurrently to see whether
 * a second connection changes the outcome.
 *
 * Usage: node scripts/poly-ws-endurance.mjs [seconds]
 */
import WebSocket from 'ws';

const SECONDS = Number(process.argv[2] ?? 400);
const ROUND_SECONDS = 900;

async function tokensFor(asset) {
  const slot = Math.floor(Date.now() / 1000 / ROUND_SECONDS) * ROUND_SECONDS;
  const r = await fetch(`https://gamma-api.polymarket.com/markets?slug=${asset}-updown-15m-${slot}`, {
    signal: AbortSignal.timeout(8000),
  });
  if (!r.ok) throw new Error(`gamma HTTP ${r.status}`);
  const [m] = await r.json();
  return m ? JSON.parse(m.clobTokenIds || '[]') : [];
}

const ids = (await Promise.all(['btc', 'eth', 'sol', 'xrp'].map(tokensFor))).flat().filter(Boolean);
if (!ids.length) {
  console.error('no tokens resolved');
  process.exit(1);
}

const started = Date.now();
let bytes = 0;
let frames = 0;
let endedAt = null;
let closeCode = null;

const ws = new WebSocket('wss://ws-subscriptions-clob.polymarket.com/ws/market');

ws.on('open', () => {
  console.log(`[${((Date.now() - started) / 1000).toFixed(1)}s] open; subscribing ${ids.length} tokens`);
  ws.send(JSON.stringify({ assets_ids: ids, type: 'market', initial_dump: true }));
});
ws.on('message', (d) => { bytes += d.length; frames++; });
ws.on('error', (e) => console.log(`[${((Date.now() - started) / 1000).toFixed(1)}s] error: ${e.message}`));
ws.on('close', (code, reason) => {
  endedAt = (Date.now() - started) / 1000;
  closeCode = code;
  console.log(`[${endedAt.toFixed(1)}s] CLOSED code=${code} reason="${reason}"`);
});

await new Promise((r) => setTimeout(r, SECONDS * 1000));
const elapsed = endedAt ?? (Date.now() - started) / 1000;
try { ws.close(); } catch {}

console.log('');
console.log(`survived: ${elapsed.toFixed(1)}s ${endedAt ? `(venue closed it, code ${closeCode})` : '(still open when sampling ended)'}`);
console.log(`frames: ${frames} | bytes: ${bytes}`);
if (elapsed > 0) {
  const perSec = bytes / elapsed;
  console.log(`rate while open: ${(perSec / 1024).toFixed(1)} KiB/s = ${(perSec * 86400 / 1e9).toFixed(2)} GB/day`);
}
