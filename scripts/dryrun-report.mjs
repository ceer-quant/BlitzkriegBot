#!/usr/bin/env node
/**
 * DryRun report (E16 / #98): the per-strategy ledger view the 7-day DryRun
 * acceptance needs, re-runnable on any day.
 *
 * Reads the append-only trade ledger (`data/trades/trades.jsonl`, written by
 * the kernel's trade_db) and — optionally — the shadow-evolution per-strategy
 * audit files under `data/evolution/`, then emits:
 *
 *   - per-strategy ledger: closed trades, wins/losses, win rate, payoff,
 *     profit factor, net PnL, fees — the "按策略独立账本" view;
 *   - per-UTC-day breakdown for the requested window;
 *   - portfolio totals;
 *   - fill rate / partial-fill rate over the ORDER ledger
 *     (`data/orders/orders.jsonl`, issue #183): how many orders actually
 *     traded, how many were left partially filled, and what share of the
 *     requested size filled. A dry run used to fill every crossing maker
 *     whole, so this read a flat 100% and hid the live-bug-② shape.
 *
 * The tool is read-only over the ledger and never trades. `--days N` selects
 * the trailing window (default 7 = the acceptance's DryRun week); `--from
 * <ms|ISO>` / `--to <ms>` pin an explicit window instead. Exit code 0 always
 * (a report on empty data is a valid, honest report) unless a read fails.
 *
 * Usage:
 *   node scripts/dryrun-report.mjs [--days 7] [--trades data/trades/trades.jsonl]
 *        [--orders data/orders/orders.jsonl]
 *        [--evolution-dir data/evolution] [--out <dir>] [--json]
 *
 * `--json` prints the same manifest the `--out` directory would receive, so a
 * script can consume the metrics without parsing the markdown.
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync, statSync, readdirSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};

const tradesPath = resolve(ROOT, flag('--trades') ?? 'data/trades/trades.jsonl');
const ordersPath = resolve(ROOT, flag('--orders') ?? 'data/orders/orders.jsonl');
const evolutionDir = resolve(ROOT, flag('--evolution-dir') ?? 'data/evolution');
const days = Number(flag('--days') ?? 7);
const fromArg = flag('--from');
const toArg = flag('--to');
const outDir = flag('--out') ? resolve(flag('--out')) : null;
const asJson = args.includes('--json');

const utc = (ms) => new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z');
const day = (ms) => new Date(ms).toISOString().slice(0, 10);

// Window: trailing N days (wall clock now) unless pinned. `--from/--to`
// accept epoch ms or an ISO date string.
const parseMs = (v) => {
  const s = String(v);
  if (/^-?\d+$/.test(s)) return Number(s);
  return Date.parse(s);
};
const nowMs = Date.now();
const fromMs = fromArg ? parseMs(fromArg) : nowMs - days * 86_400_000;
const toMs = toArg ? parseMs(toArg) : nowMs;
if (!Number.isFinite(fromMs) || !Number.isFinite(toMs)) {
  console.error('--from/--to must be epoch ms or an ISO date string');
  process.exit(1);
}

if (!existsSync(tradesPath)) {
  console.error(`trade ledger not found: ${tradesPath}`);
  console.error('the DryRun week needs a core running with the trade log enabled');
  process.exit(1);
}

const num = (v) => {
  if (v == null) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
};
const r3 = (n) => (n == null ? '—' : (Math.round(n * 1000) / 1000).toString());

// ── load the ledger ─────────────────────────────────────────────────────────
let malformed = 0, skippedNoTs = 0;
const trades = [];
{
  const raw = readFileSync(tradesPath, 'utf8');
  for (const line of raw.split('\n')) {
    if (!line.trim()) continue;
    try {
      const t = JSON.parse(line);
      // Close timestamp: the Node-format ledger stamps the close in
      // `exitTime`; tolerate the other spellings a hand-merged file may have.
      const ts = num(t.exitTime) ?? num(t.exit_time) ?? num(t.closedAtMs) ?? num(t.ts) ?? num(t.at);
      if (ts == null) { skippedNoTs++; continue; }
      trades.push({ ts, ...t });
    } catch { malformed++; }
  }
}
const inWindow = trades.filter((t) => t.ts >= fromMs && t.ts <= toMs);

// ── aggregate ───────────────────────────────────────────────────────────────
function aggregate(rows) {
  const wins = rows.filter((t) => Number(t.netPnlUsd ?? t.net_pnl_usd) > 0);
  const losses = rows.filter((t) => Number(t.netPnlUsd ?? t.net_pnl_usd) < 0);
  const gp = wins.reduce((a, t) => a + Number(t.netPnlUsd ?? t.net_pnl_usd), 0);
  const gl = Math.abs(losses.reduce((a, t) => a + Number(t.netPnlUsd ?? t.net_pnl_usd), 0));
  const fees = rows.reduce((a, t) => a + Number(t.feesUsd ?? t.fees_usd ?? 0), 0);
  const net = rows.reduce((a, t) => a + Number(t.netPnlUsd ?? t.net_pnl_usd), 0);
  return {
    closed: rows.length,
    wins: wins.length,
    losses: losses.length,
    winRatePct: rows.length ? Number(((wins.length / rows.length) * 100).toFixed(3)) : null,
    payoff: wins.length && losses.length ? Number(((gp / wins.length) / (gl / losses.length)).toFixed(4)) : null,
    profitFactor: gl > 0 ? Number((gp / gl).toFixed(4)) : (gp > 0 ? null : 0),
    feesUsd: Number(fees.toFixed(4)),
    netPnlUsd: Number(net.toFixed(4)),
  };
}

// ── order flow: fill rate / partial-fill rate (#183) ────────────────────────
//
// The trade ledger only knows CLOSED positions, so it cannot say whether the
// entries that opened them filled whole. The order log can: it appends the
// order's current snapshot on every state change, so folding it to the latest
// line per order id yields each order's final `size` / `filledSize` / `status`.
// Absent file is not an error — the report is still valid without it, so the
// section is simply reported as unavailable rather than failing the run.
function orderFlow(path) {
  if (!existsSync(path)) return null;
  const latest = new Map();
  let malformed = 0;
  let total = 0;
  for (const line of readFileSync(path, 'utf8').split('\n')) {
    if (!line.trim()) continue;
    total++;
    let o;
    try { o = JSON.parse(line); } catch { malformed++; continue; }
    const id = o.orderId ?? o.order_id;
    if (id == null) { malformed++; continue; }
    latest.set(id, o);
  }
  const numOr = (v) => (Number.isFinite(Number(v)) ? Number(v) : 0);
  let filledOrders = 0, partialOrders = 0, unfilledOrders = 0;
  let requested = 0, filled = 0;
  for (const o of latest.values()) {
    const size = numOr(o.size);
    const got = numOr(o.filledSize ?? o.filled_size);
    requested += size;
    filled += got;
    if (got <= 0) unfilledOrders++;
    else if (got >= size) filledOrders++;
    else partialOrders++;
  }
  const orders = latest.size;
  const pct = (n, d) => (d > 0 ? Number(((n / d) * 100).toFixed(4)) : 0);
  return {
    path,
    lines: total,
    malformedLines: malformed,
    orders,
    filledOrders,
    partialOrders,
    unfilledOrders,
    // Share of orders that traded at all (whole or partial).
    fillRatePct: pct(filledOrders + partialOrders, orders),
    // Share of orders left PARTIALLY filled: the dry run's exposure to the
    // live-bug-② shape (issue #183).
    partialRatePct: pct(partialOrders, orders),
    // Filled size over requested size across every order: the volume view.
    sizeFillRatioPct: pct(filled, requested),
  };
}

const orderFlowStats = orderFlow(ordersPath);

const strategies = [...new Set(inWindow.map((t) => t.strategy ?? 'unknown'))].sort();
const perStrategy = {};
for (const s of strategies) {
  perStrategy[s] = aggregate(inWindow.filter((t) => t.strategy === s));
}

// Per-UTC-day trend across the window (the DryRun week's day-by-day shape).
const perDay = {};
for (const t of inWindow) {
  const d = day(t.ts);
  perDay[d] = perDay[d] ?? [];
  perDay[d].push(t);
}
const daysOut = Object.keys(perDay).sort().map((d) => ({ date: d, ...aggregate(perDay[d]) }));

// Breaker trips from the evolution audit files (audit records carry
// `applied:false` with a reason; consecutive-loss trips carry the breaker
// message in the kernel log — the audit is the durable per-strategy record).
const evolutionFiles = existsSync(evolutionDir)
  ? readdirSync(evolutionDir).filter((f) => f.endsWith('.jsonl'))
  : [];
const breakerTrips = [];
for (const f of evolutionFiles) {
  const strategy = f.replace(/\.jsonl$/, '');
  try {
    for (const line of readFileSync(join(evolutionDir, f), 'utf8').split('\n')) {
      if (!line.trim()) continue;
      let rec;
      try { rec = JSON.parse(line); } catch { continue; }
      const ts = num(rec.timestamp);
      if (ts != null && ts >= fromMs && ts <= toMs
        && (rec.applied === false || rec.rollback === true)) {
        breakerTrips.push({ ts, strategy, reason: rec.reason ?? '', rollback: rec.rollback === true });
      }
    }
  } catch {}
}

const portfolio = aggregate(inWindow);

const manifest = {
  generatedAtMs: nowMs,
  window: { fromMs, toMs, fromUtc: utc(fromMs), toUtc: utc(toMs), daysRequested: days },
  tradesLedger: { path: tradesPath, bytes: statSync(tradesPath).size, totalTrades: trades.length, malformedLines: malformed, withoutTimestamp: skippedNoTs },
  perStrategy,
  perDay: daysOut,
  portfolio,
  // #183 fill realism. `null` when the order log is not present: the metric is
  // then unknown, which is not the same as "everything filled".
  orderFlow: orderFlowStats,
  orderFlowNote: orderFlowStats
    ? undefined
    : `order log not found at ${ordersPath}: fill rate unavailable (pass --orders)`,
  breakerTrips: breakerTrips.sort((a, b) => a.ts - b.ts),
};

if (outDir) {
  mkdirSync(outDir, { recursive: true });
  writeFileSync(join(outDir, 'dryrun-report.json'), JSON.stringify(manifest, null, 2) + '\n');
}

if (asJson) {
  // Machine-readable surface: the same manifest `--out` writes, on stdout.
  console.log(JSON.stringify(manifest, null, 2));
  process.exit(0);
}

// ── markdown ────────────────────────────────────────────────────────────────
const md = [];
md.push(`# DryRun report — ${utc(nowMs)}`);
md.push('');
md.push(`Window: ${utc(fromMs)} → ${utc(toMs)} (${inWindow.length} closed trades of ${trades.length} in the ledger).`);
md.push('');
md.push('## Per-strategy ledger (独立账本)');
md.push('');
md.push('| strategy | closed | wins | losses | WR% | payoff | PF | fees USD | net USD |');
md.push('|---|---|---|---|---|---|---|---|---|');
for (const s of strategies) {
  const m = perStrategy[s];
  md.push(`| ${s} | ${m.closed} | ${m.wins} | ${m.losses} | ${r3(m.winRatePct)} | ${r3(m.payoff)} | ${r3(m.profitFactor)} | ${r3(m.feesUsd)} | ${r3(m.netPnlUsd)} |`);
}
md.push('');
md.push('## Per-UTC-day trend');
md.push('');
md.push('| UTC day | closed | wins | losses | WR% | PF | net USD |');
md.push('|---|---|---|---|---|---|---|');
for (const d of daysOut) {
  md.push(`| ${d.date} | ${d.closed} | ${d.wins} | ${d.losses} | ${r3(d.winRatePct)} | ${r3(d.profitFactor)} | ${r3(d.netPnlUsd)} |`);
}
md.push('');
md.push('## Portfolio (组合)');
md.push('');
md.push(`closed ${portfolio.closed}, WR ${r3(portfolio.winRatePct)}%, payoff ${r3(portfolio.payoff)}, PF ${r3(portfolio.profitFactor)}, fees ${r3(portfolio.feesUsd)}, **net ${r3(portfolio.netPnlUsd)} USD**.`);
md.push('');
md.push('## Fill rate / partial fills (#183)');
md.push('');
if (orderFlowStats) {
  const f = orderFlowStats;
  md.push('| orders | filled | partial | never filled | fill rate % | partial rate % | size filled % |');
  md.push('|---|---|---|---|---|---|---|');
  md.push(`| ${f.orders} | ${f.filledOrders} | ${f.partialOrders} | ${f.unfilledOrders} | ${r3(f.fillRatePct)} | ${r3(f.partialRatePct)} | ${r3(f.sizeFillRatioPct)} |`);
  md.push('');
  md.push(`${f.path}: ${f.lines} lines folded to ${f.orders} orders. "fill rate" = orders that traded at all; "partial rate" = orders left partially filled (the live-bug-② shape).`);
} else {
  md.push(`order log not found at ${ordersPath} — fill rate unavailable (pass \`--orders\`).`);
}
md.push('');
if (breakerTrips.length) {
  md.push('## Breaker / evolution events in window');
  md.push('');
  for (const b of breakerTrips.slice(-20)) md.push(`- ${utc(b.ts)} ${b.rollback ? 'rollback' : 'reject'} ${b.strategy}: ${b.reason}`);
} else {
  md.push('## Breaker / evolution events in window');
  md.push('');
  md.push('None recorded in the evolution audit directory for this window.');
}
md.push('');
md.push('Note (E15 / #97): dry-mode economics are read under the KI-1 / #15 premise (dry fills are not venue fills) until that fix lands — money figures are directional, not a live P&L forecast.');
md.push('');

console.log(md.join('\n'));
if (outDir) {
  writeFileSync(join(outDir, 'dryrun-report.md'), md.join('\n') + '\n');
  console.log(`\nreport: ${join(outDir, 'dryrun-report.json')}`);
  console.log(`report: ${join(outDir, 'dryrun-report.md')}`);
}
