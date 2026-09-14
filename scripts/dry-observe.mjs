#!/usr/bin/env node
/**
 * Real-round DRY observation for the Rust self-driving engine.
 *
 * Purpose: the last pre-cutover gate — watch the Rust core trade a LIVE round
 * end-to-end (real tokens, real orderbooks via Rust-native WS) in DRY mode, and
 * confirm it behaves: discovers the round, ingests books/spot, decides, and only
 * ever places simulated orders.
 *
 * Flow:
 *   1. discover the current round's UP/DOWN tokens from Gamma (slug-based)
 *   2. spawn the real core with --engine --feed-ws (DRY), feed it the round
 *   3. on round rollover, re-discover and re-subscribe
 *   4. print a live status board; log every order/fill/risk/error event
 *   5. on Ctrl+C (or --duration-sec), stop and print a summary
 *
 * Safety: mode is forced DRY. If DRY_RUN=false is set, the script refuses unless
 * --force is passed, so this can never place a real order by accident.
 *
 * Usage:
 *   node scripts/dry-observe.mjs [--assets BTC,ETH] [--duration-sec 900]
 *                                [--round-sec 300] [--force] [--no-feed-ws]
 */

import { join, dirname } from 'path';
import { fileURLToPath } from 'url';
import { tmpdir } from 'os';
import { mkdtempSync } from 'fs';
import { BlitzkriegCoreClient } from '../dist/core/blitzkrieg-core-client.js';
import { scratchSocketPath } from './lib/core-socket.mjs';

const __dirname = dirname(fileURLToPath(import.meta.url));
const BIN = join(__dirname, '..', 'target', 'release', 'blitzkrieg-core');
const GAMMA = 'https://gamma-api.polymarket.com';

// Throwaway working dir: the core writes its trade log at a relative path, so
// this keeps observation orders out of the production data directory.
const WORKDIR = mkdtempSync(join(tmpdir(), 'blitzkrieg-dry-observe-'));

// ── Args ─────────────────────────────────────────────────────────────────────
const argv = process.argv.slice(2);
const opt = (name, def) => {
  const i = argv.indexOf(name);
  return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[i + 1] : def;
};
const has = (name) => argv.includes(name);

const ASSETS = (opt('--assets', 'BTC')).split(',').map((s) => s.trim().toUpperCase()).filter(Boolean);
const DURATION_SEC = parseInt(opt('--duration-sec', '0'), 10); // 0 = until Ctrl+C
const ROUND_SEC = parseInt(opt('--round-sec', '0'), 10); // 0 = auto-probe
const USE_FEED_WS = !has('--no-feed-ws');
const FORCE = has('--force');
const POLL_MS = 3000;

// ── Safety guard ─────────────────────────────────────────────────────────────
if (process.env.DRY_RUN === 'false' && !FORCE) {
  console.error('Refusing to run: DRY_RUN=false (live). This observer is DRY-only. Pass --force to override.');
  process.exit(2);
}

// ── Gamma round discovery ────────────────────────────────────────────────────
const DURATION_LABELS = { 300: '5m', 900: '15m', 3600: '1h', 14400: '4h', 86400: 'daily' };

function decodeWireJson(v) {
  // Gamma returns string fields that are themselves JSON (or already arrays).
  if (Array.isArray(v)) return v;
  if (typeof v === 'string') {
    try { return JSON.parse(v); } catch { return []; }
  }
  return [];
}

async function fetchRound(asset, roundSec) {
  const label = DURATION_LABELS[roundSec];
  if (!label) return null;
  const slotStart = Math.floor(Date.now() / 1000 / roundSec) * roundSec;
  const slug = `${asset.toLowerCase()}-updown-${label}-${slotStart}`;
  const url = `${GAMMA}/markets?slug=${encodeURIComponent(slug)}&active=true&closed=false`;
  let arr;
  try {
    const res = await fetch(url);
    if (!res.ok) return null;
    arr = await res.json();
  } catch {
    return null;
  }
  if (!Array.isArray(arr) || arr.length === 0) return null;
  const m = arr[0];
  if (m.closed || !m.active) return null;

  const outcomes = decodeWireJson(m.outcomes);
  const tokens = decodeWireJson(m.clobTokenIds);
  const prices = decodeWireJson(m.outcomePrices);
  if (outcomes.length < 2 || tokens.length < 2) return null;
  const upIdx = outcomes.findIndex((o) => /^(up|yes)$/i.test(o));
  const downIdx = outcomes.findIndex((o) => /^(down|no)$/i.test(o));
  if (upIdx < 0 || downIdx < 0) return null;

  const endMs = new Date(m.endDate || m.endDateIso).getTime();
  return {
    asset,
    conditionId: m.conditionId,
    questionId: m.questionID || m.question_id || '',
    upTokenId: tokens[upIdx],
    downTokenId: tokens[downIdx],
    upPrice: parseFloat(prices[upIdx]) || 0.5,
    downPrice: parseFloat(prices[downIdx]) || 0.5,
    expiresAtMs: endMs,
    roundSlot: Math.floor(endMs / 1000 / roundSec),
    negRisk: m.negRisk ?? true,
    question: m.question || '',
  };
}

async function probeRoundSec() {
  for (const rs of [300, 900, 3600]) {
    for (const a of ASSETS) {
      const mk = await fetchRound(a, rs);
      if (mk) return rs;
    }
  }
  return 0;
}

async function discoverRound(roundSec) {
  const out = [];
  for (const a of ASSETS) {
    const mk = await fetchRound(a, roundSec);
    if (mk) out.push(mk);
  }
  return out;
}

// ── Formatting ───────────────────────────────────────────────────────────────
const fmt = (n, d = 2) => (typeof n === 'number' ? n.toFixed(d) : String(n));
const ts = () => new Date().toTimeString().slice(0, 8);

// ── Main ─────────────────────────────────────────────────────────────────────
let client;
let lastSlot = 0;
let status = {
  blockedTiming: 0,
  blockedMomentum: 0,
  roundSlot: 0,
  timeLeftSec: 0,
  markets: 0,
  ordersPlaced: 0,
  fills: 0,
  closed: 0,
  errors: 0,
  riskAlerts: 0,
};
const recentOrders = [];
const recentFills = [];

async function main() {
  if (!USE_FEED_WS) {
    console.log('note: --no-feed-ws set; Rust will NOT pull market data on its own.');
  }

  const roundSec = ROUND_SEC || (await probeRoundSec());
  if (!roundSec) {
    console.error('Could not find live markets for any known duration. Markets may be closed for the hour.');
    process.exit(1);
  }
  console.log(`round duration: ${roundSec}s (${DURATION_LABELS[roundSec]})  assets: ${ASSETS.join(',')}`);

  const sock = scratchSocketPath('dry-observe');
  const extraArgs = [
    '--engine',
    '--no-event-archive',
    '--round-sec', String(roundSec),
    '--min-round-age', '30',
    '--min-time-left', '180',
    '--max-positions', '5',
    '--max-order-notional', '5',
    '--seed-balance', '1000',
  ];
  if (USE_FEED_WS) extraArgs.push('--feed-ws');

  client = new BlitzkriegCoreClient({
    binaryPath: BIN,
    socketPath: sock,
    mode: 'dry', // forced
    seedBalance: 1000,
    maxOrderNotional: 5,
    tickMs: 50,
    autoRestart: false,
    cwd: WORKDIR,
    noTradeLog: true,  // these harnesses assert via events/positions, never the persisted ledger
    extraArgs,
  });

  client.on('event', onEvent);
  await client.start();

  let markets = await discoverRound(roundSec);
  if (markets.length === 0) {
    console.log('no live markets right now for the current round yet; waiting for the next boundary…');
  } else {
    await client.setMarkets(markets);
    console.log(`${ts()} | fed round: ${markets.map((m) => `${m.asset}(${((m.expiresAtMs - Date.now()) / 1000) | 0}s)`).join(' ')}`);
  }

  const startedAt = Date.now();
  const timer = setInterval(async () => {
    try {
      await tick(roundSec, markets);
    } catch (e) {
      console.log(`${ts()} | observe tick error: ${e?.message || e}`);
      status.errors++;
    }
    if (DURATION_SEC > 0 && Date.now() - startedAt >= DURATION_SEC * 1000) {
      await shutdown('duration reached');
    }
  }, POLL_MS);

  const onSignal = () => { shutdown('signal').catch(() => process.exit(0)); };
  process.on('SIGINT', onSignal);
  process.on('SIGTERM', onSignal);

  // Keep a handle so the interval isn't GC'd; shutdown clears it.
  timer.unref?.();
}

async function tick(roundSec, markets) {
  const now = Date.now();
  const slot = Math.floor(now / 1000 / roundSec);

  // Round rollover: rediscover and re-feed.
  if (slot !== lastSlot) {
    lastSlot = slot;
    const fresh = await discoverRound(roundSec);
    if (fresh.length > 0) {
      await client.setMarkets(fresh);
      markets.length = 0;
      markets.push(...fresh);
      console.log(`${ts()} | new round slot=${slot}: ${fresh.map((m) => m.asset).join(',')} fed`);
    } else {
      console.log(`${ts()} | new round slot=${slot}: no markets found yet`);
    }
  }

  const [round, orderList, pos, stats] = await Promise.all([
    client.round().catch(() => null),
    client.listOrders().catch(() => ({ orders: [] })),
    client.positions().catch(() => ({ positions: [] })),
    client.request('engine.stats').catch(() => null),
  ]);

  status.roundSlot = round?.slot ?? slot;
  status.timeLeftSec = round?.timeLeftSec ?? 0;
  status.markets = round?.markets ?? markets.length;
  status.ordersPlaced = orderList.orders.filter((o) => o.strategy === 'spread_arb').length;
  if (stats?.blocked) {
    status.blockedTiming = stats.blocked.timing;
    status.blockedMomentum = stats.blocked.momentum;
  }

  const live = orderList.orders.filter((o) => o.status === 'LIVE' || o.status === 'PENDING' || o.status === 'PARTIALLY_FILLED');
  const opens = pos.positions;

  const feedInfo = stats
    ? `books=${stats.books} spots=${stats.spots} conf=${stats.confirmed?.length ?? 0} sigs=${stats.signals}` +
      (stats.blocked ? ` blocked(timing=${stats.blocked.timing},mom=${stats.blocked.momentum})` : '')
    : '';
  console.log(
    `${ts()} | round=${status.roundSlot} tLeft=${status.timeLeftSec}s mkt=${status.markets} ` +
    `entries=${status.ordersPlaced} live=${live.length} pos=${opens.length} fills=${status.fills} ${feedInfo} err=${status.errors}`
  );
  if (stats?.confirmedDetail?.length) {
    for (const d of stats.confirmedDetail) {
      console.log(`         trend ${String(d.token).slice(0, 10)}… mid=${fmt(d.mid, 3)} entry=${fmt(d.entry, 3)} cap=${fmt(d.cap, 2)} inBand=${d.inBand}`);
    }
  }
  for (const o of live) {
    console.log(`         live  ${o.asset || '?'} ${o.direction} ${o.side} ${fmt(o.price, 3)} x${fmt(o.size, 0)} [${o.status}]`);
  }
  for (const p of opens) {
    console.log(`         pos   ${p.asset} ${p.direction} entry=${fmt(p.entryPrice, 3)} cur=${fmt(p.currentPrice, 3)} pnl=${fmt(p.unrealizedPct, 1)}%`);
  }
}

function onEvent(e) {
  switch (e.kind) {
    case 'ORDER_UPDATE': {
      const o = e.order;
      if (o.strategy !== 'spread_arb') return;
      recentOrders.push(o);
      console.log(`         ORDER ${o.asset} ${o.direction} ${o.side} ${fmt(o.price, 3)} x${fmt(o.size, 0)} -> ${o.status}`);
      break;
    }
    case 'FILL': {
      status.fills++;
      recentFills.push(e.delta);
      console.log(`         FILL  ${e.delta.asset} ${e.delta.direction} ${e.delta.side} ${fmt(e.delta.delta, 0)}@${fmt(e.delta.price, 3)}`);
      break;
    }
    case 'POSITION_CLOSED': {
      status.closed++;
      console.log(`         CLOSE ${e.asset} ${e.direction} ${e.reason} pnl=$${fmt(e.netPnlUsd)} daily=$${fmt(e.dailyPnlUsd)}`);
      break;
    }
    case 'RISK_ALERT': {
      status.riskAlerts++;
      console.log(`         RISK  ${e.code} ${e.message}`);
      break;
    }
    case 'ERROR': {
      status.errors++;
      console.log(`         ERROR ${e.error?.code} ${e.error?.message}`);
      break;
    }
    case 'RECONCILE_REPORT': {
      if (e.filled || e.markedFilled || e.markedCancelled || e.ghostIds?.length) {
        console.log(`         RECON filled=${e.filled} filled_status=${e.markedFilled} cancelled=${e.markedCancelled} ghosts=${e.ghostIds?.length || 0}`);
      }
      break;
    }
    default:
      break;
  }
}

async function shutdown(reason) {
  console.log(`\n${ts()} | stopping (${reason})`);
  try { await client?.stop(); } catch { /* noop */ }
  console.log('\n--- observation summary ---');
  console.log(`round slot        : ${status.roundSlot}`);
  console.log(`spread_arb entries: ${status.ordersPlaced}`);
  console.log(`fills             : ${status.fills}`);
  console.log(`positions closed  : ${status.closed}`);
  console.log(`risk alerts       : ${status.riskAlerts}`);
  console.log(`near-miss blocked : timing=${status.blockedTiming} momentum=${status.blockedMomentum}`);
  console.log(`errors            : ${status.errors}`);
  const placed = recentOrders.filter((o) => o.status === 'LIVE' || o.status === 'PARTIALLY_FILLED' || o.status === 'FILLED');
  console.log(`recent orders     : ${recentOrders.length} (${placed.length} live/filled)`);
  for (const f of recentFills.slice(-5)) {
    console.log(`  fill ${f.asset} ${f.direction} ${f.side} ${fmt(f.delta, 0)}@${fmt(f.price, 3)}`);
  }
  process.exit(0);
}

main().catch((e) => {
  console.error('observer failed:', e?.stack || e);
  try { client?.stop(); } catch { /* noop */ }
  process.exit(1);
});
