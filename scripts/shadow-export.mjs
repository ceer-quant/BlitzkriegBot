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

// A file with records but no recognised timestamp is the failure mode this tool
// was blind to (#97): the manifest printed a null span and the 30-day premise
// below silently ignored that source. A new log with a new timestamp field name
// would do it again, so it is an error, not a warning.
const untimestamped = (s) => s.events > 0 && s.timestampedEvents === 0;

// Each source kind names its own timestamp fields. Only the event archives
// carry `at`; the shadow and ledger logs each chose a different name. A scanner
// that knows only `at` reports those files as untimestamped — which is how the
// first manifest shipped three of its seven files with a null span and a `— → —`
// range while every one of their records plainly carried a millisecond stamp
// (#97). That matters beyond cosmetics: the 30-day premise below is computed
// from the timestamps found here, so a corpus the scanner cannot read is a
// corpus the premise check cannot see.
//
// Every candidate on a line is read and the line's span is min→max. That is the
// right answer for a one-stamp archive record (min = max = `at`) and also for a
// trade (`entryTime`→`exitTime`) or an order (`submitted`→`updated`), whose
// meaningful span is the interval rather than either endpoint.
const TIME_KEYS = {
  'event-archive': ['at'],
  'shadow-near-miss': ['blockedAt'],
  'trade-log': ['entryTime', 'exitTime'],
  'order-log': ['submittedAtMs', 'updatedAtMs', 'escalateAtMs'],
  'position-log': ['entered_at_ms', 'expires_at_ms', 'last_book_ts'],
};

/** `[lo, hi]` of the timestamps on one line, or null when it carries none. */
function lineTimes(line, keys) {
  let lo = null, hi = null;
  for (const key of keys) {
    const m = line.match(new RegExp(`"${key}":(\\d+)`));
    if (!m) continue;
    const v = Number(m[1]);
    if (!Number.isFinite(v)) continue;
    if (lo === null || v < lo) lo = v;
    if (hi === null || v > hi) hi = v;
  }
  return lo === null ? null : [lo, hi];
}

// One streaming pass per file: first/last timestamp, event count, content sha256.
async function scan(file, keys) {
  let n = 0, first = null, last = null, timestamped = 0;
  const hash = createHash('sha256');
  const rl = readline.createInterface({ input: createReadStream(file), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line) continue;
    hash.update(line);
    hash.update('\n');
    n++;
    const span = lineTimes(line, keys);
    if (span === null) continue;
    timestamped++;
    if (first == null || span[0] < first) first = span[0];
    if (last == null || span[1] > last) last = span[1];
  }
  return { events: n, firstAtMs: first, lastAtMs: last, timestampedEvents: timestamped, contentSha256: hash.digest('hex') };
}

/** First/last across a set of scanned files, as `{ firstAtMs, lastAtMs, days }`. */
function spanOf(files) {
  const spanned = files.filter((s) => s.firstAtMs != null);
  if (!spanned.length) return { firstAtMs: null, lastAtMs: null, days: 0 };
  const firstAtMs = Math.min(...spanned.map((s) => s.firstAtMs));
  const lastAtMs = Math.max(...spanned.map((s) => s.lastAtMs));
  return { firstAtMs, lastAtMs, days: Number(((lastAtMs - firstAtMs) / 86_400_000).toFixed(4)) };
}

console.log(`scanning ${sources.length} source(s)…`);
const scanned = [];
for (const s of sources) {
  const info = await scan(s.path, TIME_KEYS[s.kind] ?? ['at']);
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

// Non-empty but unreadable is a hard failure, and it comes BEFORE the manifest
// is written: a manifest that omits a source's span would understate — or, with
// a different set of files, overstate — the coverage this export exists to
// report, and a written file outlives the run that was honest about it.
const blind = scanned.filter(untimestamped);
if (blind.length) {
  console.error(`\nFAIL — ${blind.length} non-empty source(s) yielded no timestamp; TIME_KEYS has no entry for their field names:`);
  for (const s of blind) console.error(`  ${s.kind}: ${s.path} (${s.events} events, 0 timestamped)`);
  process.exit(1);
}

// Aggregate coverage over the EVENT-SOURCED files only: ledger logs (trades,
// orders, positions) are sparse by construction and would stretch the span
// without adding observation density.
//
// The premise is about the SHADOW corpus (E15: "30 days of shadow data"), so it
// is judged on the near-miss span, not on the union with the archives. A
// 30-day archive sitting beside a one-day near-miss log is not 30 days of
// shadow data, and the union would have called it one — the same false green
// this export exists to prevent, one level up.
const archiveSpan = spanOf(scanned.filter((s) => s.kind === 'event-archive'));
const shadowSpan = spanOf(scanned.filter((s) => s.kind === 'shadow-near-miss'));
const firstAtMs = archiveSpan.firstAtMs ?? shadowSpan.firstAtMs;
const lastAtMs = Math.max(archiveSpan.lastAtMs ?? 0, shadowSpan.lastAtMs ?? 0) || null;
const spanDays = archiveSpan.days;
const premiseDays = 30;
const premiseMet = archiveSpan.days >= premiseDays && shadowSpan.days >= premiseDays;

// Per-UTC-day event histogram over the archives — the honest shape of the
// corpus (gaps and all) in one line of numbers.
const perDay = {};
for (const s of scanned) {
  // Cheap second pass only if the file is small enough to hold: archives are
  // hundreds of MB, so histogram during the first scan instead? The first
  // scan already ran — do a light rescan for archives only.
  if (s.kind !== 'event-archive') continue;
  const keys = TIME_KEYS[s.kind] ?? ['at'];
  const rl = readline.createInterface({ input: createReadStream(s.path), crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line) continue;
    const span = lineTimes(line, keys);
    if (span === null) continue;
    const day = new Date(span[0]).toISOString().slice(0, 10);
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
    archiveSpanDays: archiveSpan.days,
    shadowSpanDays: shadowSpan.days,
    premiseMet,
    totalEvents: scanned.reduce((a, s) => a + s.events, 0),
    totalBytes: scanned.reduce((a, s) => a + s.sizeBytes, 0),
    perUtcDay: perDay,
    verdict: premiseMet
      ? `coverage ${spanDays} days ≥ ${premiseDays}: the 30-day premise is met`
      : `coverage ${shadowSpan.days} days of shadow data (archive ${archiveSpan.days}) < ${premiseDays}: ` +
        'the 30-day premise is NOT met — walk-forward results on this corpus are method validation, not the E15 verification',
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
md.push(
  `**Shadow corpus coverage: ${shadowSpan.days} days** (${utc(shadowSpan.firstAtMs)} → ${utc(shadowSpan.lastAtMs)}); ` +
    `**event archive: ${archiveSpan.days} days** (${utc(archiveSpan.firstAtMs)} → ${utc(archiveSpan.lastAtMs)}). ${manifest.coverage.verdict}.`,
);
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
console.log(
  `coverage: shadow ${shadowSpan.days} days, archive ${archiveSpan.days} days — 30-day premise ${premiseMet ? 'MET' : 'NOT MET'}`,
);
