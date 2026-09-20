#!/usr/bin/env node
/**
 * Shadow-data export with a coverage manifest (E15 / #97).
 *
 * The E15 acceptance asks for "30 days of shadow data, exported". This tool
 * does the export AND the honesty check: it walks the shadow sources (the
 * frozen event archives, the near-miss log, the trade/order/position logs),
 * streams each file once for its first/last event timestamp and event count,
 * and writes a manifest that states the aggregate coverage in days. If that
 * number is far below 30, the manifest says so — a report must never imply
 * the premise was met when the corpus does not reach it.
 *
 * Copying hundreds of MB of archive is optional (`--copy`): the manifest with
 * per-file paths, sizes, counts and content checksums is the auditable
 * record; the bytes are already frozen on disk under data/. `--copy`
 * materialises a self-contained export directory instead.
 *
 * Checksums are over the newline-split, CRLF-stripped content (what the
 * scanner sees), not the raw bytes — they fingerprint the corpus for
 * "did this analysis run on this data" checks, not for transport integrity.
 *
 * Usage:
 *   node scripts/shadow-export.mjs [--out data/shadow/export/<ts>] [--copy]
 */

import { createReadStream, createWriteStream, existsSync, mkdirSync, readFileSync, writeFileSync, statSync, readdirSync, copyFileSync } from 'fs';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';
import { createHash } from 'crypto';
import readline from 'readline';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, '..');
const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const copyMode = args.includes('--copy');
const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\..*/, '').replace('T', '-') + 'Z';
const outDir = resolve(flag('--out') ?? join(ROOT, 'data', 'shadow', 'export', stamp));

const utc = (ms) => (ms == null ? '—' : new Date(ms).toISOString().replace(/\.\d{3}Z$/, 'Z'));

// Sources: every frozen archive, plus the live shadow/ledger logs.
const sources = [];
{
  const archiveDir = join(ROOT, 'data', 'archive');
  if (existsSync(archiveDir)) {
    for (const f of readdirSync(archiveDir).filter((x) => x.endsWith('.jsonl')).sort()) {
      sources.push({ path: join(archiveDir, f), kind: 'event-archive' });
    }
  }
  for (const [rel, kind] of [
    ['data/shadow/near-miss.jsonl', 'shadow-near-miss'],
    ['data/trades/trades.jsonl', 'trade-log'],
    ['data/orders/orders.jsonl', 'order-log'],
    ['data/positions/positions.jsonl', 'position-log'],
  ]) {
    const p = join(ROOT, rel);
    if (existsSync(p)) sources.push({ path: p, kind });
  }
}
if (!sources.length) { console.error('no shadow sources found under data/'); process.exit(2); }

// One streaming pass per file: first/last `at`, event count, content sha256.
async function scan(file) {
  let n = 0, first = null, last = null, timestamped = 0;
  const hash = createHash('sha256');
  const rl = readline.createInterface({ input: createReadStream(file), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line) continue;
    hash.update(line);
    hash.update('\n');
    n++;
    const at = Number(line.match(/"at":(\d+)/)?.[1]);
    if (!Number.isFinite(at)) continue;
    timestamped++;
    if (first == null || at < first) first = at;
    if (last == null || at > last) last = at;
  }
  return { events: n, firstAtMs: first, lastAtMs: last, timestampedEvents: timestamped, contentSha256: hash.digest('hex') };
}

console.log(`scanning ${sources.length} source(s)…`);
const scanned = [];
for (const s of sources) {
  const info = await scan(s.path);
  const size = statSync(s.path).size;
  scanned.push({
    kind: s.kind,
    path: s.path,
    sizeBytes: size,
    ...info,
    coverageDays: info.firstAtMs != null ? Number(((info.lastAtMs - info.firstAtMs) / 86_400_000).toFixed(4)) : null,
  });
  console.log(`  ${s.kind}: ${info.events} events, ${utc(info.firstAtMs)} → ${utc(info.lastAtMs)} (${(size / 1e6).toFixed(1)} MB)`);
}

// Aggregate coverage over the EVENT-SOURCED files only: ledger logs (trades,
// orders, positions) are sparse by construction and would stretch the span
// without adding observation density.
const eventFiles = scanned.filter((s) => s.kind === 'event-archive' || s.kind === 'shadow-near-miss');
const spanned = eventFiles.filter((s) => s.firstAtMs != null);
const firstAtMs = spanned.length ? Math.min(...spanned.map((s) => s.firstAtMs)) : null;
const lastAtMs = spanned.length ? Math.max(...spanned.map((s) => s.lastAtMs)) : null;
const spanDays = firstAtMs != null ? Number(((lastAtMs - firstAtMs) / 86_400_000).toFixed(4)) : 0;
const premiseDays = 30;
const premiseMet = spanDays >= premiseDays;

// Per-UTC-day event histogram over the archives — the honest shape of the
// corpus (gaps and all) in one line of numbers.
const perDay = {};
for (const s of spanned) {
  // Cheap second pass only if the file is small enough to hold: archives are
  // hundreds of MB, so histogram during the first scan instead? The first
  // scan already ran — do a light rescan for archives only.
  if (s.kind !== 'event-archive') continue;
  const rl = readline.createInterface({ input: createReadStream(s.path), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line) continue;
    const at = Number(line.match(/"at":(\d+)/)?.[1]);
    if (!Number.isFinite(at)) continue;
    const day = new Date(at).toISOString().slice(0, 10);
    perDay[day] = (perDay[day] ?? 0) + 1;
  }
}

const manifest = {
  generatedAtMs: Date.now(),
  premiseDays,
  coverage: {
    firstAtMs,
    lastAtMs,
    spanDays,
    premiseMet,
    totalEvents: scanned.reduce((a, s) => a + s.events, 0),
    totalBytes: scanned.reduce((a, s) => a + s.sizeBytes, 0),
    perUtcDay: perDay,
    verdict: premiseMet
      ? `coverage ${spanDays} days ≥ ${premiseDays}: the 30-day premise is met`
      : `coverage ${spanDays} days < ${premiseDays}: the 30-day premise is NOT met — walk-forward results on this corpus are method validation, not the E15 verification`,
  },
  files: scanned,
};
mkdirSync(outDir, { recursive: true });
writeFileSync(join(outDir, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');

if (copyMode) {
  const copied = [];
  for (const s of scanned) {
    const dest = join(outDir, s.kind, s.path.split('/').pop());
    mkdirSync(dirname_of(dest), { recursive: true });
    copyFileSync(s.path, dest);
    copied.push(dest);
  }
  console.log(`copied ${copied.length} file(s) into ${outDir}`);
}
function dirname_of(p) {
  return p.split('/').slice(0, -1).join('/');
}

// Markdown summary.
const md = [];
md.push(`# Shadow data export — ${utc(manifest.generatedAtMs)}`);
md.push('');
md.push(`**Coverage: ${spanDays} days** (${utc(firstAtMs)} → ${utc(lastAtMs)}). ${manifest.coverage.verdict}.`);
md.push('');
md.push('| kind | file | events | span (UTC) | size MB |');
md.push('|---|---|---|---|---|');
for (const s of scanned) {
  md.push(`| ${s.kind} | ${s.path.replace(ROOT + '/', '')} | ${s.events} | ${utc(s.firstAtMs)} → ${utc(s.lastAtMs)} | ${(s.sizeBytes / 1e6).toFixed(1)} |`);
}
md.push('');
if (Object.keys(perDay).length) {
  md.push('Events per UTC day (event-sourced files):');
  md.push('');
  md.push('| day | events |');
  md.push('|---|---|');
  for (const day of Object.keys(perDay).sort()) md.push(`| ${day} | ${perDay[day]} |`);
  md.push('');
}
writeFileSync(join(outDir, 'manifest.md'), md.join('\n') + '\n');

console.log(`\nmanifest: ${join(outDir, 'manifest.json')}`);
console.log(`summary : ${join(outDir, 'manifest.md')}`);
console.log(`coverage: ${spanDays} days — 30-day premise ${premiseMet ? 'MET' : 'NOT MET'}`);
