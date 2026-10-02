#!/usr/bin/env node
/**
 * mad_dog spot corpus builder — the frozen-window transformation WITHOUT the
 * two cuts that structurally blind the committed corpus: spot lines are KEPT
 * and book depth is NOT truncated (data_source.rs consumes all three kinds:
 * book/top/spot/round).
 *
 * Two modes:
 *   --validate   book(truncated to TOP_LEVELS)+round ONLY, byte-for-byte the
 *                frozen-corpus.mjs transformation — the output sha256 must
 *                equal the pinned window hash or the builder refuses. This is
 *                what proves this builder reads the same capture the pins
 *                were cut from.
 *   (default)    full fidelity: book (verbatim, full depth) + round + spot,
 *                verbatim lines sorted by `at`.
 *
 * Usage:
 *   node scripts/mad-dog-spot-corpus.mjs --archive <dir> --out <dir> \
 *        --window name=2026-09-19T16:00:00Z [--window ...] [--validate]
 */

import { createHash } from 'crypto';
import { createReadStream, existsSync, mkdirSync, readdirSync, writeFileSync } from 'fs';
import { createInterface } from 'readline';
import { gzipSync } from 'zlib';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';
import { WINDOWS, LEAD_MS, SPAN_MS, TOP_LEVELS } from './lib/frozen-corpus.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const argv = process.argv.slice(2);
const flag = (name) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : undefined;
};
const ARCHIVE = flag('--archive');
const OUT = flag('--out');
const VALIDATE = argv.includes('--validate');
const WINDOW_FLAGS = [];
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === '--window') WINDOW_FLAGS.push(argv[i + 1]);
}

if (!ARCHIVE || !OUT || WINDOW_FLAGS.length === 0) {
  console.error('need --archive <dir> --out <dir> --window name=ISOAT [--validate]');
  process.exit(2);
}

const windows = WINDOW_FLAGS.map((spec) => {
  const [name, at] = spec.split('=');
  const pinned = WINDOWS.find((w) => w.name === name);
  return { name, at, t0: Date.parse(at), sha256: pinned?.sha256 };
});

const files = existsSync(ARCHIVE)
  ? readdirSync(ARCHIVE).filter((f) => /^events.*\.jsonl$/.test(f)).sort()
  : [];
if (files.length === 0) throw new Error(`no events.*.jsonl under ${ARCHIVE}`);

function truncateBook(ev) {
  const b = Array.isArray(ev.b) ? ev.b.slice(-TOP_LEVELS) : undefined;
  const a = Array.isArray(ev.a) ? ev.a.slice(-TOP_LEVELS) : undefined;
  const out = { at: ev.at, k: 'book', t: ev.t };
  if (b) out.b = b;
  if (a) out.a = a;
  return JSON.stringify(out);
}

const rows = new Map(windows.map((w) => [w.name, []]));
const counts = new Map(windows.map((w) => [w.name, { book: 0, round: 0, spot: 0 }]));
let scanned = 0;

for (const f of files) {
  const rl = createInterface({ input: createReadStream(join(ARCHIVE, f)), crlfDelay: Infinity });
  for await (const line of rl) {
    const isBook = line.includes('"k":"book"');
    const isRound = line.includes('"k":"round"');
    const isSpot = !isBook && !isRound && line.includes('"k":"spot"');
    if (!isBook && !isRound && !isSpot) continue;
    const at = Number(/"at":(\d+)/.exec(line)?.[1] ?? NaN);
    if (!Number.isFinite(at)) continue;
    for (const w of windows) {
      if (at < w.t0 - LEAD_MS || at >= w.t0 + SPAN_MS) continue;
      const c = counts.get(w.name);
      if (isBook) c.book++;
      else if (isRound) c.round++;
      else c.spot++;
      if (VALIDATE && isSpot) continue;
      rows.get(w.name).push(isBook ? (VALIDATE ? truncateBook(JSON.parse(line)) : line) : line);
    }
    scanned++;
  }
}

let failed = false;
for (const w of windows) {
  const body = rows.get(w.name);
  body.sort((x, y) => JSON.parse(x).at - JSON.parse(y).at);
  const jsonl = body.join('\n') + '\n';
  const buf = Buffer.from(jsonl, 'utf8');
  const sha = createHash('sha256').update(buf).digest('hex');
  const c = counts.get(w.name);
  console.log(`${w.name}: ${body.length} lines (book ${c.book}, round ${c.round}, spot ${c.spot}), ${(buf.length / 1e6).toFixed(1)} MB, sha256 ${sha.slice(0, 16)}…`);
  if (VALIDATE) {
    const ok = w.sha256 && w.sha256 !== 'PENDING' && w.sha256 === sha;
    console.log(`  pinned ${w.sha256 ?? '(no pin)'} → ${ok ? 'OK' : 'MISMATCH'}`);
    if (!ok) failed = true;
  } else {
    mkdirSync(OUT, { recursive: true });
    writeFileSync(join(OUT, `${w.name}.jsonl`), buf);
  }
}
console.log(`scanned ${scanned} archive lines`);
if (failed) throw new Error('validation failed: builder output does not reproduce the pinned corpus');
