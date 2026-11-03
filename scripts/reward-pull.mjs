#!/usr/bin/env node
/**
 * reward-pull — the read-only platform-liquidity-rewards puller (#910).
 *
 * The maker-side strategies earn money the trade ledger never sees: Polymarket's
 * liquidity rewards pay RESTING orders, not fills, so `data/trades/*.jsonl` is
 * structurally blind to that income. This script pulls the wallet's REWARD
 * activity from the Data API and lands it as a JSONL ledger in the #388 archive
 * cashflow format —
 *
 *     {"at":<epoch_ms>,"k":"cashflow","which":"reward","usd":"<decimal string>"}
 *
 * — the exact row the replay side already credits (`DataEvent::Cashflow`,
 * `CashflowReport.rewards_usd`) and `dryrun-report.mjs --rewards` folds into
 * the account income view. One format, both consumers; the core never learns
 * about the network (contract freeze, #840).
 *
 * Data-api facts this encodes (probed 2026-11-02, #910 evidence):
 *   - v1 is retired (410 Gone); v2 lives at /v2/activity (cursor-only paging —
 *     `offset` answers 400), and has NO time parameter, so the walk always
 *     covers full history and windowing is client-side;
 *   - REWARD rows carry no market attribution (condition_id=""), so the ledger
 *     is wallet×time, nothing more;
 *   - payments settle ~00:15 UTC for the previous UTC day and are NOT daily.
 *
 * Read-only and credential-free by construction: a public wallet address, a
 * GET, no keys, no orders, no venue state. The dial target goes through the
 * ONE constructor (`makeTarget` + host allowlist) — a non-allowlisted,
 * reserved, literal-IP or non-https base is refused before any request.
 *
 * Honest failure: transport errors retry per page (exponential backoff — the
 * network is flaky today); a page that exhausts its retries aborts the whole
 * pull, the existing output is left untouched, and the exit code says so. A
 * silently truncated rewards ledger would understate income — the one thing
 * this surface must never do.
 *
 * Window semantics: `--from/--to` gate which PULLED rows land this run. Rows
 * already in the ledger are always kept, so the file is a monotone union of
 * every pull ever made (a bare re-run = full history) and never shrinks under
 * a narrower window.
 *
 * Usage:
 *   node scripts/reward-pull.mjs --wallet 0x… [--out data/rewards/rewards.jsonl]
 *        [--from <ms|ISO>] [--to <ms|ISO>] [--limit 100] [--max-pages 200]
 *        [--retries 4] [--backoff-ms 2000]
 */

import { existsSync, mkdirSync, renameSync, writeFileSync } from 'fs';
import { dirname, resolve } from 'path';
import { fileURLToPath } from 'url';

import {
  DATA_API_BASE,
  PAGE_LIMIT,
  activityUrl,
  collectRewards,
  loadRewardRows,
  makeTarget,
  rewardKey,
  toArchiveCashflow,
} from './lib/reward-ledger.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};

const wallet = flag('--wallet') ?? process.env.POLYMARKET_DATA_WALLET;
const outPath = resolve(ROOT, flag('--out') ?? 'data/rewards/rewards.jsonl');
const fromArg = flag('--from');
const toArg = flag('--to');
const limit = Number(flag('--limit') ?? PAGE_LIMIT);
const maxPages = Number(flag('--max-pages') ?? 200);
const retries = Number(flag('--retries') ?? 4);
const backoffMs = Number(flag('--backoff-ms') ?? 2000);

const fail = (msg) => {
  console.error(`reward-pull: ${msg}`);
  process.exit(1);
};
if (!wallet) {
  fail('no wallet: pass --wallet 0x… (or set POLYMARKET_DATA_WALLET)');
}
if (!/^0x[0-9a-fA-F]{40}$/.test(wallet)) {
  fail(`not a 20-byte hex wallet address: ${wallet}`);
}
if (!Number.isFinite(limit) || limit < 1 || limit > 500) fail('--limit must be 1..500');
if (!Number.isFinite(maxPages) || maxPages < 1) fail('--max-pages must be >= 1');
if (!Number.isFinite(retries) || retries < 0) fail('--retries must be >= 0');
if (!Number.isFinite(backoffMs) || backoffMs < 0) fail('--backoff-ms must be >= 0');

// Window filter, applied to pulled rows only (see "Window semantics" above).
const parseMs = (v) => (/^-?\d+$/.test(String(v)) ? Number(v) : Date.parse(v));
const fromMs = fromArg != null ? parseMs(fromArg) : null;
const toMs = toArg != null ? parseMs(toArg) : null;
if (fromArg != null && !Number.isFinite(fromMs)) fail('--from must be epoch ms or an ISO date string');
if (toArg != null && !Number.isFinite(toMs)) fail('--to must be epoch ms or an ISO date string');
if (fromMs != null && toMs != null && fromMs > toMs) fail('--from is after --to');

// SSRF: the one constructor, the one allowlisted host. A refused base never
// becomes a request — it becomes this message.
const target = makeTarget(DATA_API_BASE);
if (!target.ok) fail(`base target refused: ${target.reason}`);

// Transport: one GET per page, the repo's honest research UA (the #901 corpus
// puller's spelling, re-stamped for this issue), 30s budget; any non-2xx is a
// failure whose message carries the status and the body head (v1's 410
// retirement was exactly how the migration announced itself — the body must
// survive into the log).
const fetchPage = async (url) => {
  const resp = await fetch(url, {
    signal: AbortSignal.timeout(30_000),
    headers: {
      'User-Agent': 'Mozilla/5.0 (BlitzkriegBot-research; issue-910)',
      Accept: 'application/json',
    },
  });
  const text = await resp.text();
  if (!resp.ok) {
    throw new Error(`http ${resp.status} from ${url.split('?')[0]}: ${text.slice(0, 200)}`);
  }
  return text;
};

const started = Date.now();
let collected;
try {
  collected = await collectRewards({
    target,
    wallet,
    fetchPage,
    maxPages,
    retries,
    backoffMs,
    onPage: (e) => {
      if (e.note) console.error(`reward-pull: page ${e.page}: ${e.note}`);
      else
        console.error(
          `reward-pull: page ${e.page}: ${e.rows} reward rows (+${e.skipped} skipped), has_more=${e.hasMore}`,
        );
    },
  });
} catch (e) {
  console.error(`reward-pull: pull FAILED — nothing written, existing ledger untouched: ${e.message}`);
  process.exit(1);
}

// Merge with the existing ledger: existing rows always kept, pulled rows gated
// by the window, union deduped by second+amount, ordered oldest-first — the
// archive's own replay order (ReplaySource reads in timestamp order), so the
// file can ride straight into a replay corpus.
const existing = existsSync(outPath)
  ? loadRewardRows(outPath)
  : { present: false, rows: [], malformed: 0, wrongKind: 0 };
if (existing.present && (existing.malformed > 0 || existing.wrongKind > 0)) {
  console.error(
    `reward-pull: existing ledger carries ${existing.malformed} malformed + ${existing.wrongKind} non-reward lines — dropped and counted (clean-rewrite semantics), only well-formed reward rows merge`,
  );
}
const landed = collected.rows.filter(
  (r) => (fromMs == null || r.atMs >= fromMs) && (toMs == null || r.atMs <= toMs),
);
const byKey = new Map();
let duplicateRows = 0;
for (const r of [...existing.rows, ...landed]) {
  const key = rewardKey(r);
  if (byKey.has(key)) duplicateRows += 1;
  else byKey.set(key, r);
}
const all = [...byKey.values()].sort((a, b) => a.atMs - b.atMs);

// One rewrite for the whole file via rename: a reader never sees a torn JSONL
// — either the complete new bytes land or the old file stays.
const tmpPath = `${outPath}.tmp`;
try {
  mkdirSync(dirname(tmpPath), { recursive: true });
  writeFileSync(tmpPath, all.map((r) => JSON.stringify(toArchiveCashflow(r))).join('\n') + '\n');
  renameSync(tmpPath, outPath);
} catch (e) {
  fail(`could not write ${outPath}: ${e.message}`);
}

const summary = {
  wallet,
  endpoint: `${activityUrl(target, wallet).split('?')[0]} (type=REWARD, cursor-paged)`,
  pages: collected.pages,
  fetchedRows: collected.rows.length,
  landedRows: landed.length,
  skippedRows: collected.skipped,
  retriesUsed: collected.retriesUsed,
  mergedExistingRows: existing.rows.length,
  duplicateRowsDropped: duplicateRows,
  windowFilter: fromMs != null || toMs != null ? { fromMs, toMs } : null,
  outPath,
  ledgerRows: all.length,
  ledgerUsd: all.reduce((a, r) => a + Number(r.usd), 0).toFixed(4),
  elapsedMs: Date.now() - started,
};
console.log(JSON.stringify(summary, null, 2));
console.error(`reward-pull: ledger now ${all.length} rows (${summary.ledgerUsd} USDC) in ${outPath}`);
