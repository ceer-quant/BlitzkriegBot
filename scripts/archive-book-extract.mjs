#!/usr/bin/env node
/**
 * Archive book extractor (#192) — turn a real L2 snapshot in the production event
 * archive into the `{"asks": [...], "bids": [...]}` file `capacity-check.mjs`
 * takes, without ever loading the archive into memory.
 *
 * Why this exists: the capacity gate measures self-impact against a stated book.
 * Its default ladder is a FIXTURE, and `--book <file>` takes a real one — but the
 * repository had no real one to hand it, so every capacity number on record
 * described an example ledger's depth rather than any token's. The production
 * archive (`data/archive/*.jsonl`) already records every L2 snapshot the feed
 * delivered; what was missing was a way to get one out. That is all this is.
 *
 * READ-ONLY, and by construction: the only paths opened for reading are the
 * archive segments, and `--out` is REFUSED anywhere under `data/`, so a typo
 * cannot turn a measurement into a mutation of the production archive. Output
 * goes to `--out`, or to stdout when it is omitted.
 *
 * Streaming, not slurping: segments are read with `readline`, one line at a time,
 * and the scan stops at `--max-bytes` (default 64 MB, the LAST 64 MB of the newest
 * segment — the freshest data, which is what a "how deep is the book now" answer
 * wants). `--max-age-min` (default 120) additionally drops snapshots older than
 * the newest event the scan saw, so a stale segment cannot be mistaken for a
 * current book. Both bounds are printed with the answer.
 *
 * The archive's shape (verified against the production files, not assumed):
 *
 *   {"k":"book","at":<ms>,"t":"<token>",
 *    "a":[["0.99","1356.55"],["0.98","281.69"],...,["0.41","365.26"]],
 *    "b":[["0.01","1453.15"],["0.02","258.4"],...,["0.4","72.85"]]}
 *
 * `a` (asks) is DESCENDING and `b` (bids) ASCENDING — both worst-first, best LAST,
 * and both as STRINGS. The normalized output is the opposite order and numbers,
 * because the consumer reads the best level as `asks[0]` / `bids[0]` and every
 * following number is derived from that choice. An ordering mistake here does not
 * fail loudly — it yields a plausible, wrong curve — so the normalization is
 * asserted by `--self-test`, not trusted.
 *
 * Usage:
 *   node scripts/archive-book-extract.mjs --self-test          # no archive needed
 *   node scripts/archive-book-extract.mjs                      # auto-pick a token
 *   node scripts/archive-book-extract.mjs --token <id> --out /tmp/book.json
 *   node scripts/archive-book-extract.mjs --list-candidates    # why THIS token
 *
 * Exit 0 only if the snapshot was found and (on `--self-test`) every assertion held.
 */
import { createReadStream, existsSync, readdirSync, statSync, writeFileSync } from 'fs';
import { createInterface } from 'readline';
import { dirname, basename, join, relative, resolve, sep } from 'path';
import { fileURLToPath } from 'url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

const argv = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = argv.indexOf(name);
  return i >= 0 && argv[i + 1] !== undefined ? argv[i + 1] : fallback;
};
const has = (name) => argv.includes(name);

const ARCHIVE_ARG = opt('--archive', null);
const ARCHIVE_DIR = resolve(ROOT, ARCHIVE_ARG ?? 'data/archive');
/**
 * How the archive is named back in the recorded command.
 *
 * Only THIS checkout's own `data/archive` is echoed in that relative form. An
 * `--archive` pointing anywhere else — the production checkout's archive, a copy
 * on another volume — is echoed VERBATIM, because rewriting any path that merely
 * ends in `data/archive` into the relative `data/archive` records a provenance the
 * fixture does not have: read from a worktree, that relative path is an empty
 * directory, and the committed snapshot's real source becomes unrecoverable.
 * `meta.archiveDir` carries the resolved path either way.
 */
const archiveLabel = (() => {
  const own = join(ROOT, 'data', 'archive');
  return ARCHIVE_DIR === own ? join('data', 'archive') : (ARCHIVE_ARG ?? ARCHIVE_DIR);
})();
const WANT_TOKEN = opt('--token', null);
const OUT = opt('--out', null);
const MAX_BYTES = Number(opt('--max-bytes', String(64 * 1024 * 1024)));
const MAX_AGE_MIN = Number(opt('--max-age-min', '120'));
const ALL_SEGMENTS = has('--all-segments');
const SELF_TEST = has('--self-test');
const LIST_CANDIDATES = has('--list-candidates');

/**
 * Minimum depth the BEST level of EACH side must show before a snapshot is
 * eligible for auto-selection.
 *
 * The number is derived, not tuned: the live deployment pins its ticket size with
 * `min_shares = max_shares = 10` (see `engine.rs`), so a best level holding fewer
 * than 5x that ticket cannot say anything the deployed sizing does not already
 * pin — the answer would be "your 10-share order is fine", which is not a
 * capacity measurement. 50 shares is that floor.
 *
 * It also has to hold on BOTH sides, because the same snapshot is measured twice
 * (buy walks `asks`, sell walks `bids`, see `capacity-check.mjs --side`): a book
 * with a 400-share ask top and a 2-share bid top would give a buy curve worth
 * reading and a sell curve that is noise.
 */
const MIN_TOP_DEPTH = Number(opt('--min-top-depth', '50'));

/**
 * Minimum number of price levels EACH side must show.
 *
 * Also derived rather than tuned: the fixture ladder this extractor exists to
 * replace has five levels (`capacity-check.mjs` DEFAULT_LADDER). A real book with
 * fewer levels on a side than the fixture cannot produce a better-resolved curve
 * than the fixture it replaces — measured on the production archive, the ranking
 * above this filter is dominated by near-certain books quoting 0.99/0.98 with ONE
 * ask level, whose "buy capacity" would be one data point.
 */
const MIN_LEVELS = Number(opt('--min-levels', '5'));

/**
 * Token ids the capacity gate itself seeds. `capacity-check.mjs` publishes its
 * re-mirrored fixture ladder under `cap-<ASSET>` (see its `bookSnapshot` call), the
 * core archives what it publishes, and the archive therefore contains those scratch
 * books: measured on the production live segment, ONE book event under `cap-BTC`
 * (at 2026-09-21T02:59:19.450Z) sits among the market's own. Such a book is a
 * fixture ladder wearing a token id, so measuring it would answer "how deep is the
 * example book" while claiming a token was measured — the exact substitution this
 * extractor exists to prevent. Real Polymarket token ids are decimal strings, so
 * the prefix cannot collide with one.
 *
 * The observed event is also one-sided or crossed (the gate mirrors the ladder onto
 * one side only), so `eligible` would reject it anyway; the prefix is what makes
 * that rejection a stated rule rather than a coincidence of that run's shape.
 */
const SYNTHETIC_TOKEN_RE = /^cap-/;

const log = (m) => process.stderr.write(`${m}\n`);

// ── Normalization: pure, so --self-test exercises the production path ─────────

/**
 * Turn one archive side into `[[price, size], ...]` best-first.
 *
 * The archive writes both sides worst-first (`a` descending, `b` ascending); the
 * output is best-first — ascending for the ask side, descending for the bid side
 * — which is what `capacity-check.mjs` indexes into (`asks[0]` / `bids[0]` is the
 * best, and every number after it follows from that choice).
 *
 * Junk is dropped rather than carried into a capacity number: non-numeric or
 * non-finite entries, prices outside the binary market's (0, 1) range, and levels
 * with size <= 0. Levels that share a price are SUMMED — two resting orders at
 * one price are one level's worth of depth, and keeping only the first would
 * understate the book.
 */
function normalizeSide(rawLevels, { side }) {
  const byPrice = new Map();
  for (const level of rawLevels ?? []) {
    if (!Array.isArray(level) || level.length < 2) continue;
    const price = Number(level[0]);
    const size = Number(level[1]);
    if (!Number.isFinite(price) || !Number.isFinite(size)) continue;
    if (price <= 0 || price >= 1) continue; // binary markets trade strictly inside (0,1)
    if (size <= 0) continue;
    byPrice.set(price, (byPrice.get(price) ?? 0) + size);
  }
  const levels = [...byPrice.entries()];
  // Best-first per side: the best ask is the LOWEST ask, the best bid the HIGHEST.
  const bestFirst = side === 'ask' ? 1 : -1;
  // A tie on price cannot happen after the summing above, but the comparator is
  // total anyway so the result cannot depend on the input order.
  levels.sort((a, b) => (a[0] - b[0]) * bestFirst);
  return levels;
}

/** Best-first levels, plus the numbers the selection rule and the report need. */
function bookStats(raw) {
  const asks = normalizeSide(raw?.a, { side: 'ask' });
  const bids = normalizeSide(raw?.b, { side: 'bid' });
  const bestAsk = asks.length ? asks[0][0] : null;
  const bestBid = bids.length ? bids[0][0] : null;
  const spread = bestAsk !== null && bestBid !== null ? bestAsk - bestBid : null;
  const mid = bestAsk !== null && bestBid !== null ? (bestAsk + bestBid) / 2 : null;
  return {
    asks,
    bids,
    bestAsk,
    bestBid,
    mid,
    spread,
    // The RELATIVE spread, in the same unit as the gate's impact budget. The
    // absolute spread cannot rank books on a (0,1) venue: a long-shot market
    // quoting 0.01 / 0.008 has an absolute spread of 0.002 — the smallest in the
    // archive — and a relative spread of 2222 bps, whose three bid levels hold
    // five dollars. Ranking on the absolute number selects exactly those books.
    spreadBps: mid !== null && mid > 0 ? (spread / mid) * 10_000 : null,
    crossed: spread !== null && spread < 0,
    oneSided: asks.length === 0 || bids.length === 0,
    askTopDepth: asks.length ? asks[0][1] : 0,
    bidTopDepth: bids.length ? bids[0][1] : 0,
    askShares: asks.reduce((s, [, x]) => s + x, 0),
    bidShares: bids.reduce((s, [, x]) => s + x, 0),
    askNotional: asks.reduce((s, [p, x]) => s + p * x, 0),
    bidNotional: bids.reduce((s, [p, x]) => s + p * x, 0),
  };
}

/**
 * Whether a snapshot can be measured at all.
 *
 * All four conditions are measurement requirements rather than preferences:
 *   * TWO-SIDED and UNCROSSED — a one-sided snapshot cannot be walked on the side
 *     it lacks, and a crossed one is not a book. The production archive contains
 *     both: a token the feed last saw bids-only (it had gone near-certain) and the
 *     capacity gate's own re-mirrored fixture ladders, which are one-sided or
 *     crossed by construction;
 *   * best-level depth >= `minTopDepth` on BOTH sides — the same snapshot is
 *     measured twice (buy walks `asks`, sell walks `bids`), so one thin top would
 *     make half the answer noise;
 *   * at least `minLevels` levels on BOTH sides — a curve needs points to have a
 *     shape.
 *
 * `--token <id>` applies the same predicate: asking for a specific token asks for
 * a usable book of it, and silently emitting a bids-only snapshot would hand
 * `capacity-check.mjs` a null spread and an empty ladder.
 */
function isSyntheticToken(token) {
  return SYNTHETIC_TOKEN_RE.test(String(token ?? ''));
}

function eligible(c, { minTopDepth, minLevels }) {
  return !isSyntheticToken(c.token) && !c.oneSided && !c.crossed &&
    c.askTopDepth >= minTopDepth && c.bidTopDepth >= minTopDepth &&
    c.asks.length >= minLevels && c.bids.length >= minLevels;
}

/**
 * The auto-selection rule, in one place so it can be self-tested and documented.
 * Among the eligible snapshots (see `eligible`) the winner is
 *
 *   1. the TIGHTEST RELATIVE spread, in bps of the midpoint — deliberately not
 *      the absolute best_ask - best_bid. On a venue that quotes probabilities in
 *      (0,1) the absolute spread cannot rank books: measured on the production
 *      archive, the smallest absolute spread (0.002) belongs to a long-shot book
 *      quoted 0.01 / 0.008, whose relative spread is 2222 bps and whose entire
 *      bid side is three levels and five dollars. A relative spread is also the
 *      same unit as the gate's own `--max-slippage-bps` budget, so "tight" here
 *      means the same thing it means in the number this script feeds. It is a
 *      real economic ranking and it does favour books near certainty — two ticks
 *      of round trip cost less, in percent, at 0.95 than at 0.50 — so the price
 *      level of whatever wins is printed with it and belongs in any write-up;
 *   2. tie-broken by the LARGEST total book notional — depth is what actually
 *      varies between two books at the same relative spread, and the deeper one
 *      is the one whose curve is worth reading;
 *   3. tie-broken by the NEWER snapshot.
 *
 * Returns null when nothing qualifies, which is a red run: "no eligible book" is
 * a fact about the window, not a reason to fall back to the fixture.
 */
function selectToken(candidates, opts) {
  const ok = candidates.filter((c) => eligible(c, opts));
  if (ok.length === 0) return null;
  return ok.reduce((best, c) => {
    if (best === null) return c;
    if (c.spreadBps !== best.spreadBps) return c.spreadBps < best.spreadBps ? c : best;
    const cNotional = c.askNotional + c.bidNotional;
    const bNotional = best.askNotional + best.bidNotional;
    if (cNotional !== bNotional) return cNotional > bNotional ? c : best;
    return c.at > best.at ? c : best;
  }, null);
}

// ── Streaming scan ───────────────────────────────────────────────────────────

/** Newest segment first; `--all-segments` walks the rotated ones after it. */
function segments() {
  if (!existsSync(ARCHIVE_DIR)) {
    log(`archive-book: no archive directory at ${ARCHIVE_DIR}`);
    process.exit(2);
  }
  const all = readdirSync(ARCHIVE_DIR).filter((f) => /^events(\..+)?\.jsonl$/.test(f));
  const withStat = all.map((f) => ({ name: f, path: join(ARCHIVE_DIR, f), mtime: statSync(join(ARCHIVE_DIR, f)).mtimeMs }));
  // `events.jsonl` is the live segment, always newest by construction; the
  // mtime sort keeps this correct if that ever stops being true.
  withStat.sort((a, b) => b.mtime - a.mtime);
  if (!ALL_SEGMENTS) return withStat.slice(0, 1);
  return withStat;
}

/**
 * Stream one segment's last `budget` bytes. Returns the book events seen, the
 * bytes/lines actually read, and the newest `at` across ALL events (not just book
 * events — the age bound is relative to "now" as the archive last recorded it).
 */
async function scanSegment(path, budget, sink) {
  const size = statSync(path).size;
  const start = Math.max(0, size - budget);
  let lines = 0;
  let bytes = 0;
  let newestAt = sink.newestAt;
  const rl = createInterface({
    input: createReadStream(path, { start, encoding: 'utf8' }),
    crlfDelay: Infinity,
  });
  let first = start > 0; // the first line of a mid-file window is a partial one
  for await (const line of rl) {
    if (first) { first = false; continue; }
    lines++;
    bytes += Buffer.byteLength(line) + 1;
    // Cheap pre-filter before JSON.parse: the archive is mostly non-book events.
    if (!line.includes('"k":"book"')) continue;
    let o;
    try { o = JSON.parse(line); } catch { continue; }
    if (!o.a || !o.b || !o.t || typeof o.at !== 'number') continue;
    if (o.at > newestAt) newestAt = o.at;
    sink.onBook(o, basename(path));
  }
  sink.newestAt = newestAt;
  return { file: basename(path), start, size, lines, bytes };
}

// ── Self-test: the normalization and the selection rule, on hand-made edges ───
function selfTest() {
  let failures = 0;
  const check = (name, cond, detail = '') => {
    if (cond) console.log(`  ok   ${name}`);
    else { failures++; console.log(`  FAIL ${name} ${detail}`); }
  };
  const eq = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  // Prices are parsed from decimal STRINGS, so a difference like 0.51 - 0.50
  // carries binary-float noise; the assertions below compare with a tolerance
  // rather than pretending the doubles are exact.
  const near = (x, y) => Math.abs(x - y) < 1e-9;

  // An archive side as the feed writes it: strings, worst-first, with every edge
  // the normalizer claims to handle — a duplicate price, a zero-size level, a
  // negative size, a malformed level, an out-of-range price, and NaN.
  const rawAsks = [
    ['0.30', '10'], ['0.29', '5'], ['0.29', '7'], ['0.28', '0'],
    ['0.27', '-3'], ['0.26', '4'], ['0.25', 'not-a-number'], ['1.00', '99'],
    ['0.00', '99'], ['0.24', '2'], 'not-a-level', ['0.23'],
  ];
  const rawBids = [
    ['0.20', '1'], ['0.21', '2'], ['0.21', '3'], ['0.22', '8'],
  ];

  const asks = normalizeSide(rawAsks, { side: 'ask' });
  const bids = normalizeSide(rawBids, { side: 'bid' });

  check('asks come out ASCENDING (best ask first, so `asks[0]` is the best)',
    eq(asks.map(([p]) => p), [0.24, 0.26, 0.29, 0.30]), JSON.stringify(asks));
  check('bids come out DESCENDING (best bid first, so `bids[0]` is the best)',
    eq(bids.map(([p]) => p), [0.22, 0.21, 0.20]), JSON.stringify(bids));
  check('string prices/sizes become numbers',
    asks.every(([p, s]) => typeof p === 'number' && typeof s === 'number'), JSON.stringify(asks));
  check('same-price levels are summed, not duplicated',
    eq(asks.find(([p]) => p === 0.29), [0.29, 12]) && eq(bids.find(([p]) => p === 0.21), [0.21, 5]),
    JSON.stringify({ ask029: asks.find(([p]) => p === 0.29), bid021: bids.find(([p]) => p === 0.21) }));
  check('zero and negative sizes are dropped',
    !asks.some(([p]) => p === 0.28 || p === 0.27), JSON.stringify(asks));
  check('prices outside (0,1) are dropped',
    !asks.some(([p]) => p === 0 || p === 1), JSON.stringify(asks));
  check('non-numeric sizes and malformed levels are dropped',
    !asks.some(([p]) => p === 0.25) && asks.length === 4, JSON.stringify(asks));

  // bookStats + the selection rule, on books with the same 5-level floor the
  // production default applies. A is TIGHT (1 tick on a 0.505 mid) and thin, C is
  // WIDER (2 ticks) and deeper, B is one-sided, D is crossed.
  const mk = (o) => ({ ...bookStats(o), at: o.at, token: o.t });
  const a = mk({
    at: 100,
    a: [['0.51', '60'], ['0.52', '10'], ['0.53', '10'], ['0.54', '10'], ['0.55', '10']],
    b: [['0.50', '60'], ['0.49', '10'], ['0.48', '10'], ['0.47', '10'], ['0.46', '10']],
  });
  const b = mk({ at: 100, a: [['0.51', '900'], ['0.52', '900'], ['0.53', '900'], ['0.54', '900'], ['0.55', '900']], b: [] });
  const c = mk({
    at: 200,
    a: [['0.52', '80'], ['0.53', '50'], ['0.54', '50'], ['0.55', '50'], ['0.60', '5']],
    b: [['0.50', '80'], ['0.49', '50'], ['0.48', '50'], ['0.47', '50'], ['0.45', '5']],
  });
  const d = mk({
    at: 300,
    a: [['0.49', '900'], ['0.48', '1'], ['0.47', '1'], ['0.46', '1'], ['0.45', '1']],
    b: [['0.50', '900'], ['0.51', '1'], ['0.52', '1'], ['0.53', '1'], ['0.54', '1']],
  });
  // The production archive's actual trap, reproduced: a long-shot book whose
  // ABSOLUTE spread (0.002) is the smallest in the sample and whose RELATIVE
  // spread is 2222 bps — eleven times the mid-range book's. Ranking on
  // best_ask - best_bid alone selects E; the rule must select F.
  const e = mk({
    at: 400,
    a: [['0.01', '161'], ['0.011', '10'], ['0.012', '10'], ['0.013', '10'], ['0.02', '200']],
    b: [['0.008', '568'], ['0.007', '10'], ['0.006', '10'], ['0.005', '10'], ['0.001', '10']],
  });
  const f = mk({
    at: 500,
    a: [['0.50', '161'], ['0.51', '10'], ['0.52', '10'], ['0.53', '10'], ['0.54', '10']],
    b: [['0.49', '161'], ['0.48', '10'], ['0.47', '10'], ['0.46', '10'], ['0.45', '10']],
  });
  e.token = 'longshot';
  f.token = 'mid';
  const DEFAULTS = { minTopDepth: 50, minLevels: 5 };

  check('spread is best_ask - best_bid, computed from the normalized bests',
    near(a.spread, 0.01) && near(c.spread, 0.02), `${a.spread} / ${c.spread}`);
  check('spreadBps is the spread relative to the midpoint (the rank key, not the absolute spread)',
    near(a.spreadBps, (0.01 / 0.505) * 10_000) && near(e.spreadBps, (0.002 / 0.009) * 10_000),
    `${a.spreadBps} / ${e.spreadBps}`);
  check('a one-sided book is flagged and disqualified',
    b.oneSided === true && selectToken([b], DEFAULTS) === null, JSON.stringify(b.oneSided));
  check('a crossed book (best_ask < best_bid) is flagged and disqualified',
    d.crossed === true && selectToken([d], DEFAULTS) === null, `crossed=${d.crossed}`);
  check('the tightest RELATIVE spread wins when both sides clear the depth floor',
    selectToken([c, a], DEFAULTS).at === 100, 'expected A (198 bps) over C (392 bps)');
  check('a wide-absolute / tight-relative book beats a tight-absolute / wide-relative one',
    selectToken([e, f], DEFAULTS).token === 'mid', 'expected the 0.50/0.49 book over the 0.01/0.008 one');
  check('a snapshot whose weaker side is under the depth floor is disqualified',
    selectToken([a], { ...DEFAULTS, minTopDepth: 61 }) === null, 'ask top is 60, floor is 61');
  check('the depth floor applies to BOTH sides (a deep ask top cannot save a thin bid top)',
    selectToken([{ ...c, bidTopDepth: 0 }], DEFAULTS) === null, 'bid top 0');
  check('a side with fewer levels than the fixture it replaces is disqualified',
    selectToken([{ ...a, asks: a.asks.slice(0, 2) }], DEFAULTS) === null, 'ask side cut to 2 levels');
  check('the level floor applies to BOTH sides',
    selectToken([{ ...a, bids: a.bids.slice(0, 1) }], DEFAULTS) === null, 'bid side cut to 1 level');
  check('equal spread breaks toward the deeper book',
    selectToken([
      { ...a, at: 1, askNotional: 1, bidNotional: 0 },
      { ...a, at: 2, askNotional: 100, bidNotional: 100 },
    ], DEFAULTS).at === 2, 'expected the deeper snapshot');
  check('equal spread and depth break toward the newer snapshot',
    selectToken([
      { ...a, at: 1, askNotional: 5, bidNotional: 5 },
      { ...a, at: 9, askNotional: 5, bidNotional: 5 },
    ], DEFAULTS).at === 9, 'expected the newer snapshot');

  // The gate's own scratch books are IN the archive: capacity-check.mjs publishes
  // its re-mirrored ladder under `cap-<ASSET>` and the core archives it. A book
  // that clears every other condition must still be refused — and it must lose to a
  // real book it would otherwise beat on spread, or the rule is decoration.
  const scratch = mk({
    at: 600, t: 'cap-BTC',
    a: [['0.51', '60'], ['0.52', '10'], ['0.53', '10'], ['0.54', '10'], ['0.55', '10']],
    b: [['0.50', '60'], ['0.49', '10'], ['0.48', '10'], ['0.47', '10'], ['0.46', '10']],
  });
  check('a gate scratch book (cap-*) is disqualified even though it passes every depth/level test',
    eligible(a, DEFAULTS) === true && eligible(scratch, DEFAULTS) === false &&
    selectToken([scratch], DEFAULTS) === null &&
    selectToken([scratch, f], DEFAULTS).token === 'mid',
    `eligible(real)=${eligible(a, DEFAULTS)} eligible(cap-BTC)=${eligible(scratch, DEFAULTS)}`);

  // The exact shape capacity-check.mjs consumes: an ordered pair of pairs with no
  // strings anywhere, since it reads `asks[0]` / `bids[0]` as the best level.
  const shape = mk({ at: 1, a: [['0.51', '60'], ['0.52', '10']], b: [['0.50', '60'], ['0.49', '10']] });
  const produced = JSON.stringify({ asks: shape.asks, bids: shape.bids });
  check('the emitted book is JSON numbers with the best level first on each side',
    produced === JSON.stringify({ asks: [[0.51, 60], [0.52, 10]], bids: [[0.50, 60], [0.49, 10]] }), produced);

  console.log(failures === 0
    ? '\nARCHIVE-BOOK-EXTRACT SELF-TEST OK — normalization and token selection are what they claim.'
    : `\nARCHIVE-BOOK-EXTRACT SELF-TEST FAILED (${failures})`);
  process.exit(failures === 0 ? 0 : 1);
}
if (SELF_TEST) selfTest();

if (has('--help') || has('-h')) {
  console.log(`archive-book-extract — one real L2 snapshot out of the production event archive.

usage:
  node scripts/archive-book-extract.mjs [--token <id>] [--out <path>]
  node scripts/archive-book-extract.mjs --list-candidates
  node scripts/archive-book-extract.mjs --self-test

options:
  --archive <dir>       archive directory (default: data/archive)
  --token <id>          emit THIS token's newest eligible snapshot instead of auto-picking
  --max-bytes <n>       how much of the newest segment to scan, from its TAIL (default ${MAX_BYTES})
  --max-age-min <n>     drop snapshots older than this, vs the newest event seen (default ${MAX_AGE_MIN})
  --min-top-depth <n>   shares required at the BEST level of EACH side (default ${MIN_TOP_DEPTH})
  --min-levels <n>      price levels required on EACH side (default ${MIN_LEVELS})
  --all-segments        continue into rotated segments after the newest one
  --list-candidates     print every token in the window with its stats and eligibility
  --out <path>          write here (refused anywhere under data/); stdout when omitted
  --self-test           assert the normalization and the selection on fixtures; needs no archive

auto-selection, when --token is not given:
  among snapshots inside the window that are TWO-SIDED, UNCROSSED, at least ${MIN_TOP_DEPTH} shares deep at
  the best level of EACH side, and at least ${MIN_LEVELS} levels on EACH side, pick the one with the
  tightest RELATIVE spread (spreadBps = (best_ask - best_bid) / mid * 10000). Ties break toward
  the deeper book, then the newer snapshot.

  * relative, not absolute: on a 0.01-tick binary venue an absolute spread cannot rank books
    (0.01 is 102.6 bps at mid 0.975 but 183.5 bps at mid 0.545), and bps-of-mid is the unit
    capacity-check.mjs already budgets in (--max-slippage-bps).
  * ${MIN_TOP_DEPTH} shares = 5x the live ticket (min_shares = max_shares = 10, engine.rs), so the
    answer can say more than "your 10-share order fits".
  * ${MIN_LEVELS} levels = the fixture ladder's own level count; a coarser book cannot produce a
    finer curve than the fixture it replaces.
  * KNOWN BIAS: relative spread shrinks as mid -> 1 on a 0.01 tick, so this criterion favours
    near-resolution tokens. Pick a mid-priced token explicitly with --token.
  * gate scratch books (token ids matching ${SYNTHETIC_TOKEN_RE}, which is what capacity-check.mjs
    publishes its re-mirrored fixture ladder under) are excluded from both the auto and the
    --token path: measuring one would answer "how deep is the example book".

output: {"asks":[[price,size],...],"bids":[[price,size],...],"meta":{...}}
  asks ASCENDING, bids DESCENDING (best level first on both), same-price levels summed,
  size <= 0 dropped, prices/sizes as numbers. capacity-check.mjs --book reads level 0 as the
  best, so an ordering mistake would produce a plausible, wrong curve.

exit: 0 emitted, 1 nothing eligible in the window, 2 bad input (missing archive, --out under data/).`);
  process.exit(0);
}

// ── Main: stream the archive, pick a snapshot, emit it ───────────────────────
const segs = segments();
const maxAgeMs = MAX_AGE_MIN * 60_000;
/** Best candidate per token, kept as a one-entry-per-token rolling window. */
const perToken = new Map();
let newestAt = 0;
let bookEvents = 0;
/** Book events dropped because their token id is a gate scratch book (`cap-*`). */
let syntheticEvents = 0;
const sink = {
  newestAt: 0,
  onBook(o, sourceFile) {
    bookEvents++;
    if (isSyntheticToken(o.t)) { syntheticEvents++; return; }
    if (WANT_TOKEN && o.t !== WANT_TOKEN) return;
    const stats = bookStats(o);
    const prev = perToken.get(o.t);
    // Age is resolved after the scan (it is relative to the newest event, which
    // is only known once the window has been read), so the window is bounded by
    // bytes here and by `at` below.
    const cand = { token: o.t, at: o.at, source: sourceFile, ...stats };
    if (WANT_TOKEN) {
      // An explicit token means "the newest ELIGIBLE snapshot of it in the
      // window" — a specific token whose last snapshot is one-sided must not
      // silently become an empty ask ladder downstream.
      if (!eligible(cand, { minTopDepth: MIN_TOP_DEPTH, minLevels: MIN_LEVELS })) return;
      if (!prev || cand.at >= prev.at) perToken.set(o.t, cand);
      return;
    }
    // Auto mode keeps the per-token winner, so memory stays O(tokens).
    if (!prev || selectToken([prev, cand], { minTopDepth: MIN_TOP_DEPTH, minLevels: MIN_LEVELS }) === cand) perToken.set(o.t, cand);
  },
};

log(`archive-book: scanning ${segs[0].name}${ALL_SEGMENTS ? ` and ${segs.length - 1} rotated segment(s)` : ''} ` +
  `from the tail, budget ${(MAX_BYTES / 1e6).toFixed(0)} MB, age bound ${MAX_AGE_MIN} min, ` +
  `min top depth ${MIN_TOP_DEPTH} shares/side`);
const scanned = [];
let remaining = MAX_BYTES;
for (const seg of segs) {
  if (remaining <= 0) break;
  const r = await scanSegment(seg.path, remaining, sink);
  scanned.push(r);
  remaining -= r.size - r.start;
}
newestAt = sink.newestAt;
const totalLines = scanned.reduce((s, r) => s + r.lines, 0);
const totalBytes = scanned.reduce((s, r) => s + r.bytes, 0);
log(`archive-book: read ${totalBytes} bytes / ${totalLines} lines, ${bookEvents} book events, ` +
  `${perToken.size} token(s) matching${WANT_TOKEN ? ` ${WANT_TOKEN}` : ''}` +
  `${syntheticEvents > 0 ? `; ${syntheticEvents} gate scratch book event(s) (${SYNTHETIC_TOKEN_RE}) skipped` : ''}`);

const all = [...perToken.values()].map((c) => ({ ...c, ageMs: newestAt - c.at }));
const inWindow = all.filter((c) => c.ageMs <= maxAgeMs);
for (const c of all) c.inWindow = c.ageMs <= maxAgeMs;

if (LIST_CANDIDATES) {
  const ranked = [...inWindow].sort((x, y) => (x.spreadBps - y.spreadBps) || (y.askNotional + y.bidNotional) - (x.askNotional + x.bidNotional));
  console.log(`# candidates in the window (${inWindow.length} of ${all.length} tokens; "eligible" = two-sided, uncrossed, ` +
    `>= ${MIN_TOP_DEPTH} shares at EACH best level, >= ${MIN_LEVELS} levels per side)`);
  console.log('# eligible  spreadBps   spread  bestAsk  bestBid  askTop  bidTop  askDepth  bidDepth  lvls   at                        token');
  for (const c of ranked) {
    const ok = eligible(c, { minTopDepth: MIN_TOP_DEPTH, minLevels: MIN_LEVELS });
    console.log(`  ${ok ? 'yes   ' : 'no    '}  ${String(c.spreadBps === null ? 'n/a' : c.spreadBps.toFixed(1)).padStart(9)}  ` +
      `${String(c.spread === null ? 'n/a' : c.spread.toFixed(4)).padStart(7)}  ` +
      `${String(c.bestAsk ?? 'n/a').padStart(7)}  ${String(c.bestBid ?? 'n/a').padStart(7)}  ` +
      `${String(c.askTopDepth.toFixed(1)).padStart(7)}  ${String(c.bidTopDepth.toFixed(1)).padStart(6)}  ` +
      `${String(c.askShares.toFixed(0)).padStart(8)}  ${String(c.bidShares.toFixed(0)).padStart(8)}  ` +
      `${String(c.asks.length).padStart(3)}/${String(c.bids.length).padEnd(3)}  ` +
      `${new Date(c.at).toISOString()}  ${c.token}`);
  }
  process.exit(0);
}

const chosen = WANT_TOKEN
  ? (inWindow[0] ?? null)
  : selectToken(inWindow, { minTopDepth: MIN_TOP_DEPTH, minLevels: MIN_LEVELS });

if (chosen === null) {
  const why = all.length === 0
    ? `no book event for ${WANT_TOKEN ?? 'any token'} in the scanned window`
    : `${inWindow.length} of ${perToken.size} token(s) were inside the ${MAX_AGE_MIN} min window` +
      (WANT_TOKEN ? '' : `, none with a two-sided, uncrossed book at least ${MIN_TOP_DEPTH} shares deep at the best level on both sides`);
  log(`archive-book: nothing to emit — ${why}. Widen --max-bytes/--max-age-min, or lower --min-top-depth.`);
  process.exit(1);
}

const payload = {
  asks: chosen.asks,
  bids: chosen.bids,
  meta: {
    token: chosen.token,
    at: chosen.at,
    atIso: new Date(chosen.at).toISOString(),
    spread: chosen.spread,
    spreadBps: chosen.spreadBps,
    mid: chosen.mid,
    bestAsk: chosen.bestAsk,
    bestBid: chosen.bestBid,
    askLevels: chosen.asks.length,
    bidLevels: chosen.bids.length,
    askTopDepth: chosen.askTopDepth,
    bidTopDepth: chosen.bidTopDepth,
    askShares: chosen.askShares,
    bidShares: chosen.bidShares,
    askNotional: Number(chosen.askNotional.toFixed(6)),
    bidNotional: Number(chosen.bidNotional.toFixed(6)),
    source: chosen.source,
    archiveDir: ARCHIVE_DIR,
    ageMs: chosen.ageMs,
    newestEventAt: newestAt,
    newestEventAtIso: new Date(newestAt).toISOString(),
    selection: WANT_TOKEN ? `--token ${WANT_TOKEN} (newest in window)` : 'auto: tightest RELATIVE spread (bps of mid), then deepest book; both best levels >= --min-top-depth and both sides >= --min-levels',
    minTopDepth: MIN_TOP_DEPTH,
    minLevels: MIN_LEVELS,
    scannedBytes: totalBytes,
    scannedLines: totalLines,
    bookEvents,
    syntheticEvents,
    maxBytes: MAX_BYTES,
    maxAgeMin: MAX_AGE_MIN,
    // The invocation that produced this file, so a committed copy carries its own
    // provenance: whoever finds it in the repository can re-run it verbatim and
    // compare. The archive is named the way it was resolved — this checkout's own
    // `data/archive` in that relative form, anything else (the production
    // checkout's archive, a copy on another volume) verbatim — and the resolved
    // path is in `archiveDir`, so a fixture can never claim a source it did not
    // read.
    command: `node scripts/archive-book-extract.mjs ${argv.map((a) =>
      (a === ARCHIVE_ARG ? archiveLabel : a)).join(' ')}`,
    note: 'asks ascending (best first) / bids descending (best first); prices and sizes are numbers',
  },
};

const json = JSON.stringify(payload, null, 2) + '\n';
if (OUT) {
  const abs = resolve(OUT);
  const rel = relative(ROOT, abs);
  if (rel === 'data' || rel.startsWith(`data${sep}`)) {
    log(`archive-book: refusing to write ${abs} — this script never writes under data/ (pass a path outside the repository)`);
    process.exit(2);
  }
  writeFileSync(abs, json);
  log(`archive-book: wrote ${abs}`);
} else {
  process.stdout.write(json);
}

log(`archive-book: token ${chosen.token}`);
log(`  snapshot ${new Date(chosen.at).toISOString()} (age ${(chosen.ageMs / 1000).toFixed(1)}s vs the newest event seen)`);
log(`  spread ${chosen.spread.toFixed(4)} (best ask ${chosen.bestAsk} / best bid ${chosen.bestBid})`);
log(`  asks ${chosen.asks.length} levels, ${chosen.askShares.toFixed(2)} shares (top ${chosen.askTopDepth.toFixed(2)})`);
log(`  bids ${chosen.bids.length} levels, ${chosen.bidShares.toFixed(2)} shares (top ${chosen.bidTopDepth.toFixed(2)})`);
log(`  from ${chosen.source}, after reading ${totalBytes} bytes / ${totalLines} lines`);
