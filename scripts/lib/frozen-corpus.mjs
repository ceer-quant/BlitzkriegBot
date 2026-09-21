/**
 * The frozen replay corpus, shared by the gates that measure on it.
 *
 * Why this exists: #176 pinned four one-hour slices of the 2026-09-19/20 capture
 * by CONTENT (`sha256` over the decompressed jsonl) so a replay conclusion could
 * be re-derived instead of trusted. #203 then needed the SAME corpus under a
 * different taker-fee schedule — "what does the published rate cost this
 * strategy" — and a second copy of four hashes is a copy that cannot notice the
 * original moving. So the pins live here, once, and every consumer verifies them
 * before it reports.
 *
 * A corpus file whose bytes do not hash to its pin is a REFUSAL, never a
 * warning: a conclusion measured on unchecked bytes is a conclusion about
 * unknown data.
 *
 * `--build-corpus` regenerates the files from a local `data/archive` and checks
 * the same hashes, so the transformation (window, filters, ladder truncation) is
 * auditable rather than a claim.
 */
import { createHash } from 'crypto';
import { createReadStream, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from 'fs';
import { createInterface } from 'readline';
import { gunzipSync, gzipSync } from 'zlib';
import { join } from 'path';
import { tmpdir } from 'os';

/** Lead-in before the window: warms the replayed rounds and the strategy buffers. */
export const LEAD_MS = 15 * 60 * 1000;
/** Window length replayed. */
export const SPAN_MS = 60 * 60 * 1000;
/** Book levels kept per side (truncation is validated by `--verify-truncation`). */
export const TOP_LEVELS = 3;
/** Where the committed corpus lives, relative to the repo root. */
export const CORPUS_REL_DIR = join('docs', 'reports', 'data', 'mean-reversion-gate');

/**
 * The frozen windows. `sha256` is over the DECOMPRESSED jsonl.
 *
 * Two are one-sided slices and two two-sided; the rule that picked them is stated
 * per window (`basis`) and is a baseline-replay outcome, not a hand-label: the
 * hours with the worst shipped-config PnL and zero wins (one-sided) and the hours
 * with its best win rate (two-sided).
 */
export const WINDOWS = [
  {
    name: 'trend-20260919T1000Z',
    at: '2026-09-19T10:00:00Z',
    regime: 'one-sided',
    basis: 'worst shipped-config PnL of 2026-09-19 (-$12.11, 0 wins / 20 trades)',
    sha256: 'eb43ecef092dd4d1cc37064d8d0b7ce7ec754fcd25b7ea9e357ae4aa34104d18',
  },
  {
    name: 'range-20260919T1600Z',
    at: '2026-09-19T16:00:00Z',
    regime: 'two-sided',
    basis: 'best shipped-config win rate of 2026-09-19 (7 wins / 17 trades, 41%)',
    sha256: '497b921df3d24911604132c42a7b7102573d0da2a29485eb53d44f82b3ee87d0',
  },
  {
    name: 'trend-20260920T2100Z',
    at: '2026-09-20T21:00:00Z',
    regime: 'one-sided',
    basis: 'worst shipped-config PnL of 2026-09-20 (-$10.16, 0 wins / 17 trades)',
    sha256: '5179ac7f7f7eb7108b8baf8a5a0153868f0ea3d10f63f085fc0300b6fb5a0836',
  },
  {
    name: 'range-20260920T2300Z',
    at: '2026-09-20T23:00:00Z',
    regime: 'two-sided',
    basis: 'best shipped-config win rate of 2026-09-20 (7 wins / 18 trades, 39%; the only profitable hour of the capture)',
    sha256: '3c365b0a9574117cc2f0b3e60d96f8e0159035dee9b3cc8c50e4dcd559f6ea79',
  },
];

/** Path of one committed corpus file, from the repo root. */
export function corpusPath(root, w) {
  return join(root, CORPUS_REL_DIR, `${w.name}.jsonl.gz`);
}

/**
 * Verify the committed file against its pin and write the decompressed jsonl to
 * a scratch dir. Returns `{ path, dir, sha }`; exits 2 on a missing file or a
 * hash mismatch (a refusal, since every caller's conclusion depends on it).
 */
export function materialize(root, w) {
  const gz = corpusPath(root, w);
  if (!existsSync(gz)) {
    console.error(`missing corpus ${gz}: run --build-corpus from a checkout with data/archive`);
    process.exit(2);
  }
  const buf = gunzipSync(readFileSync(gz));
  const sha = createHash('sha256').update(buf).digest('hex');
  if (w.sha256 !== 'PENDING' && sha !== w.sha256) {
    console.error(`${w.name}: corpus sha256 ${sha} != pinned ${w.sha256} — refusing to report on a changed corpus`);
    process.exit(2);
  }
  const dir = mkdtempSync(join(tmpdir(), 'bk-corpus-'));
  const path = join(dir, `${w.name}.jsonl`);
  writeFileSync(path, buf);
  return { path, dir, sha };
}

function truncateBook(ev) {
  const b = Array.isArray(ev.b) ? ev.b.slice(-TOP_LEVELS) : undefined;
  const a = Array.isArray(ev.a) ? ev.a.slice(-TOP_LEVELS) : undefined;
  const out = { at: ev.at, k: 'book', t: ev.t };
  if (b) out.b = b;
  if (a) out.a = a;
  return JSON.stringify(out);
}

/**
 * Regenerate the committed corpus files from a local archive directory and check
 * the pins. Returns the number of lines scanned.
 */
export async function buildCorpus(root, archiveDir) {
  if (!existsSync(archiveDir)) {
    throw new Error(`missing ${archiveDir}: --build-corpus needs the recorded capture (override with --archive <dir>)`);
  }
  const files = readdirSync(archiveDir).filter((f) => /^events.*\.jsonl$/.test(f)).sort();
  const outDir = join(root, CORPUS_REL_DIR);
  mkdirSync(outDir, { recursive: true });
  const rows = new Map(WINDOWS.map((w) => [w.name, []]));
  let scanned = 0;
  for (const f of files) {
    const rl = createInterface({ input: createReadStream(join(archiveDir, f)), crlfDelay: Infinity });
    for await (const line of rl) {
      // Cheap prefilter before JSON: books and rounds only.
      if (!line.includes('"k":"book"') && !line.includes('"k":"round"')) continue;
      scanned += 1;
      const at = Number(/^\{?"?at"?:\s*(\d+)/.exec(line)?.[1] ?? /"at":(\d+)/.exec(line)?.[1] ?? NaN);
      if (!Number.isFinite(at)) continue;
      for (const w of WINDOWS) {
        const t0 = Date.parse(w.at);
        if (at < t0 - LEAD_MS || at >= t0 + SPAN_MS) continue;
        rows.get(w.name).push(line.includes('"k":"book"') ? truncateBook(JSON.parse(line)) : line);
      }
    }
  }
  let failed = false;
  for (const w of WINDOWS) {
    const body = rows.get(w.name);
    body.sort((x, y) => JSON.parse(x).at - JSON.parse(y).at);
    const jsonl = body.join('\n') + '\n';
    const buf = Buffer.from(jsonl, 'utf8');
    const sha = createHash('sha256').update(buf).digest('hex');
    const gz = corpusPath(root, w);
    writeFileSync(gz, gzipSync(buf, { level: 9 }));
    const ok = w.sha256 === 'PENDING' || w.sha256 === sha;
    console.log(`${w.name}: ${body.length} events, ${(buf.length / 1e6).toFixed(2)} MB raw, ${(readFileSync(gz).length / 1e6).toFixed(2)} MB gz`);
    console.log(`  sha256(uncompressed) ${sha} ${w.sha256 === 'PENDING' ? '(paste this)' : ok ? 'OK' : `MISMATCH want ${w.sha256}`}`);
    if (!ok) failed = true;
  }
  console.log(`scanned ${scanned} archive lines`);
  if (failed) throw new Error('corpus regeneration did not reproduce the pinned hashes');
  return scanned;
}

/** A scratch dir for one gate run's reports. */
export function scratchDir(prefix) {
  return mkdtempSync(join(tmpdir(), `${prefix}-`));
}
