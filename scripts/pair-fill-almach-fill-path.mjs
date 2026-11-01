#!/usr/bin/env node
/**
 * pair-fill almach fill-path study — WHERE in the book do almach's pair legs
 * actually print?
 *
 * Why: pair_discount_arb v1.2 quotes each leg AT its own ask with
 * `entry_maker` — and the kernel refuses any post-only BUY whose limit
 * reaches the live ask at POST time (#464, `venue.WouldCross`). The 10/15
 * replay shows exactly that: 4 orders, 4 rejections, 0 rests, 0 fills. The
 * fix must copy a fill path that WORKS, and the corpus has one: @almach, the
 * real profitable pair collector (data/almach/trades-full.jsonl, 47,509 BUY
 * fills, SELL=0).
 *
 * This script answers one question per almach pair-leg fill:
 *     fill price  vs  the prevailing book at that instant?
 * A fill AT or ABOVE the then-best-ask is a TAKER print (crossing). A fill
 * BELOW the then-best-ask is a MAKER print (a resting bid the falling ask
 * descended into — the "ask waterfall crosses the limit" path). The book
 * comes from the core's own event archive (events.<stamp>.jsonl, `k:"book"`
 * rows per token), the fills from the on-chain export.
 *
 * Usage:
 *   node scripts/pair-fill-almach-fill-path.mjs \
 *     --pairs '/Volumes/Hard Disk/bk-session/smr-round/pairfill-almach-discount-pairs.json' \
 *     --archive '/Volumes/Hard Disk/BlitzkriegBot/data/archive' \
 *     [--out /Volumes/Hard Disk/bk-session/smr-round/pairfill-almach-fill-path.json]
 *
 * Inputs:
 *   --pairs  JSON array of { cid, slug, ts (up-leg fill, epoch s), up_ts, down_ts,
 *            up_p, down_p, up_asset, down_asset } — produced by the almach
 *            pair extraction (discount pairs, sum < 0.985, legs <= 600s apart).
 *   --archive  the rotated core archive dir (events.<stamp>.jsonl).
 *
 * For every leg fill the script finds the LATEST book row for that token at
 * or before the fill instant (`--book-max-age-ms`, default 30s — the archive's
 * cadence on active BTC wheels is seconds) and prints the verdict per leg
 * plus a summary. Fills without a fresh-enough book row are listed as
 * UNBOOKED, never silently dropped.
 */

import { createReadStream, existsSync, readFileSync, readdirSync, writeFileSync } from 'fs';
import { createInterface } from 'readline';
import { basename, join } from 'path';

const args = process.argv.slice(2);
const flag = (n) => { const i = args.indexOf(n); return i >= 0 ? args[i + 1] : undefined; };

const pairsPath = flag('--pairs');
const archiveDir = flag('--archive');
const outPath = flag('--out') ?? '/Volumes/Hard Disk/bk-session/smr-round/pairfill-almach-fill-path.json';
const maxAgeMs = Number(flag('--book-max-age-ms') ?? 30_000);
const padMs = Number(flag('--pad-ms') ?? 600_000); // scan ±10min around the fills

if (!pairsPath || !existsSync(pairsPath)) {
  console.error('--pairs <file> required (almach discount-pairs JSON)');
  process.exit(2);
}
if (!archiveDir || !existsSync(archiveDir)) {
  console.error(`--archive ${archiveDir} does not exist`);
  process.exit(2);
}

const pairs = JSON.parse(readFileSync(pairsPath, 'utf8'));
const legs = [];
for (const p of pairs) {
  if (p.up_asset) legs.push({ pair: p.slug, side: 'up', tsMs: p.up_ts * 1000, price: p.up_p, token: p.up_asset });
  if (p.down_asset) legs.push({ pair: p.slug, side: 'down', tsMs: p.down_ts * 1000, price: p.down_p, token: p.down_asset });
}
if (!legs.length) {
  console.error('no legs with token ids in --pairs');
  process.exit(2);
}
const tMin = Math.min(...legs.map((l) => l.tsMs)) - padMs;
const tMax = Math.max(...legs.map((l) => l.tsMs)) + padMs;

// ── segments (same overlap rule as smart-money-reverse-replay.mjs) ───────────

const segs = readdirSync(archiveDir)
  .filter((f) => /^events\.\d{8}T\d{6}Z(-\d{4})?\.jsonl$/.test(f))
  .sort()
  .map((f) => {
    const m = /^events\.(\d{8}T\d{6}Z)(?:-(\d{4}))?\.jsonl$/.exec(f);
    const ms = Date.parse(`${m[1].slice(0, 4)}-${m[1].slice(4, 6)}-${m[1].slice(6, 8)}T${m[1].slice(9, 11)}:${m[1].slice(11, 13)}:${m[1].slice(13, 15)}Z`);
    return { file: join(archiveDir, f), endMs: ms, seq: m[2] ? Number(m[2]) : 0 };
  })
  .sort((a, b) => a.endMs - b.endMs || a.seq - b.seq);
segs.forEach((s, i) => { s.startMs = i === 0 ? 0 : segs[i - 1].endMs; });
const overlapping = segs.filter((s) => s.startMs < tMax && s.endMs > tMin);
console.log(`# ${legs.length} legs from ${pairs.length} pairs; scanning ${overlapping.length}/${segs.length} segments ` +
  `(${new Date(tMin).toISOString()} → ${new Date(tMax).toISOString()})`);
if (!overlapping.length) process.exit(2);

// ── one streaming pass: book rows for the leg tokens + round token maps ──────

const tokens = new Set(legs.map((l) => l.token));
const books = new Map(); // token -> [{ atMs, bestBid, bestAsk }] (appended in scan order)
const roundTokens = new Map(); // conditionId -> { upTokenId, downTokenId }
let scanned = 0, bookRows = 0;

for (const seg of overlapping) {
  const rl = createInterface({ input: createReadStream(seg.file), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line || line.length < 20) continue;
    const at = Number(line.match(/"at":(\d+)/)?.[1]);
    if (!Number.isFinite(at) || at < tMin || at > tMax) continue;
    scanned++;
    if (line.includes('"k":"round"')) {
      try {
        const row = JSON.parse(line);
        for (const mk of Array.isArray(row?.m) ? row.m : []) {
          if (mk.conditionId && mk.upTokenId && mk.downTokenId) {
            roundTokens.set(mk.conditionId, { upTokenId: mk.upTokenId, downTokenId: mk.downTokenId });
          }
        }
      } catch { /* a malformed round line must not kill the study */ }
      continue;
    }
    if (!line.includes('"k":"book"')) continue;
    // cheap token test before JSON.parse — the token id is a quoted string
    let tokenHit = null;
    for (const t of tokens) if (line.includes(`"${t}"`)) { tokenHit = t; break; }
    if (!tokenHit) continue;
    try {
      const row = JSON.parse(line);
      if (row.t !== tokenHit) continue;
      const bestAsk = (row.a ?? []).reduce((m, x) => Math.min(m, Number(x[0])), Infinity);
      const bestBid = (row.b ?? []).reduce((m, x) => Math.max(m, Number(x[0])), -Infinity);
      if (!Number.isFinite(bestAsk) || !Number.isFinite(bestBid)) continue;
      if (!books.has(tokenHit)) books.set(tokenHit, []);
      books.get(tokenHit).push({ atMs: at, bestBid, bestAsk });
      bookRows++;
    } catch { /* skip */ }
  }
  console.log(`  ${basename(seg.file)} done (running: ${bookRows} book rows)`);
}

// ── verdicts ─────────────────────────────────────────────────────────────────

/** Latest book row for the token at or before tsMs (within maxAgeMs). */
function bookAt(token, tsMs) {
  const rows = books.get(token);
  if (!rows) return null;
  let lo = 0, hi = rows.length - 1, best = null;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (rows[mid].atMs <= tsMs) { best = rows[mid]; lo = mid + 1; } else hi = mid - 1;
  }
  return best && tsMs - best.atMs <= maxAgeMs ? best : null;
}

const TICK = 0.01;
const results = [];
let maker = 0, taker = 0, unbooked = 0;
const ticksBelowAsk = [];

for (const leg of legs) {
  const b = bookAt(leg.token, leg.tsMs);
  if (!b) {
    unbooked++;
    results.push({ ...leg, verdict: 'UNBOOKED' });
    console.log(`UNBOOKED  ${leg.pair} ${leg.side} fill=${leg.price.toFixed(4)} (no book row within ${maxAgeMs / 1000}s before fill)`);
    continue;
  }
  const ageMs = leg.tsMs - b.atMs;
  const dAsk = (b.bestAsk - leg.price) / TICK; // >0: fill strictly below the then-ask
  const dBid = (leg.price - b.bestBid) / TICK; // >=0: fill at or above the then-bid
  let verdict;
  if (leg.price >= b.bestAsk - 1e-9) { verdict = 'TAKER(at-ask-or-above)'; taker++; }
  else if (leg.price > b.bestBid + 1e-9) { verdict = 'MAKER(inside-spread)'; maker++; ticksBelowAsk.push(dAsk); }
  else { verdict = 'BELOW-BID(deep-maker)'; maker++; ticksBelowAsk.push(dAsk); }
  results.push({ ...leg, verdict, bestBid: b.bestBid, bestAsk: b.bestAsk, bookAgeMs: ageMs, ticksBelowAsk: dAsk, ticksAboveBid: dBid });
  console.log(`${verdict.padEnd(24)} ${leg.pair} ${leg.side} fill=${leg.price.toFixed(4)} book(bid=${b.bestBid.toFixed(2)} ask=${b.bestAsk.toFixed(2)}, ${Math.round(ageMs / 1000)}s old) → ${dAsk >= 0 ? dAsk.toFixed(0) : '?'} ticks below ask, ${dBid.toFixed(0)} above bid`);
}

// pair-level: both legs' verdicts together
const pairRows = [];
for (const p of pairs) {
  const up = results.find((r) => r.pair === p.slug && r.side === 'up');
  const dn = results.find((r) => r.pair === p.slug && r.side === 'down');
  pairRows.push({
    slug: p.slug, sum: p.up_p + p.down_p,
    up: up?.verdict ?? 'MISSING', down: dn?.verdict ?? 'MISSING',
    upTicksBelowAsk: up?.ticksBelowAsk ?? null, downTicksBelowAsk: dn?.ticksBelowAsk ?? null,
  });
}

const booked = maker + taker;
const summary = {
  legsTotal: legs.length, booked, unbooked,
  makerLegs: maker, takerLegs: taker,
  makerSharePct: booked ? Math.round((100 * maker) / booked) : null,
  medianTicksBelowAsk: ticksBelowAsk.length ? ticksBelowAsk.sort((a, b) => a - b)[Math.floor(ticksBelowAsk.length / 2)] : null,
  bookRows, linesScanned: scanned,
};
console.log('\n# summary:', JSON.stringify(summary, null, 2));
writeFileSync(outPath, JSON.stringify({ summary, legs: results, pairs: pairRows }, null, 2) + '\n');
console.log(`# written: ${outPath}`);
