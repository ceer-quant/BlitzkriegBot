#!/usr/bin/env node
/**
 * reward-ledger-check — the #910 gate: platform liquidity rewards (REWARD)
 * parse, pull state machine and report merge, all against FIXTURES.
 *
 * The puller it guards (`scripts/reward-pull.mjs` + `scripts/lib/reward-ledger.mjs`)
 * is the only new network surface in the four-strategy optimisation, so the
 * gate pins the three ways it could lie:
 *
 *   1. parse drift — a data-api v2 response that no longer says
 *      `{data:[…], pagination:{has_more,next_cursor}}`, or rows that stopped
 *      carrying `timestamp`/`usdc_size`, must fail closed or be counted, never
 *      silently shrink the income figure;
 *   2. SSRF drift — anything but the single allowlisted https host
 *      (`data-api.polymarket.com`, Mimosa single-pass rule) must be refused
 *      BEFORE it can become a request;
 *   3. merge drift — the report's account income view must state trade net
 *      and reward cash SEPARATELY plus one combined total, summed exactly
 *      (0.3132 + 0.4377 + 0.7396 = 1.4905 — float-summed cents would drop
 *      the fourth decimal), keyed by PAYMENT timestamp.
 *
 * Fixture bodies mirror the #910 evidence rows (wallet 0x3725d52f…8ad88, the
 * 2026-10-30T00:15:21Z payment of 0.3132 USDC). NO NETWORK: the fetch page is
 * a map, the report merge runs the real `dryrun-report.mjs` via child process
 * against tmpdir files (standard temp dir, the #833 gate precedent).
 *
 * Run: node scripts/reward-ledger-check.mjs
 * Exit: 0 pass / 1 violations.
 */

import { spawnSync } from 'child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'fs';
import { tmpdir } from 'os';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';
import assert from 'node:assert/strict';

import {
  DATA_API_HOST,
  activityUrl,
  collectRewards,
  foldRewards,
  loadRewardRows,
  makeTarget,
  parseActivityPage,
  toArchiveCashflow,
} from './lib/reward-ledger.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

let bad = 0;
const check = (name, fn) => {
  try {
    fn();
    console.log(`  ok   ${name}`);
  } catch (e) {
    bad += 1;
    console.error(`  FAIL ${name} — ${e.message}`);
  }
};
const checkAsync = async (name, fn) => {
  try {
    await fn();
    console.log(`  ok   ${name}`);
  } catch (e) {
    bad += 1;
    console.error(`  FAIL ${name} — ${e.message}`);
  }
};

// ── fixtures: the measured v2 REWARD shape (#910 evidence) ──────────────────
const WALLET = '0x3725d52f3c252e8374999cc8617292ea2608ad88';
const TX = '0xf26f32a83c896508443f3579c133c250550e86281d2651878c3d7578865a572b';
const row = (ts, usdc, tx = TX) => ({
  proxy_wallet: WALLET,
  timestamp: ts,
  condition_id: '',
  type: 'REWARD',
  size: usdc,
  usdc_size: usdc,
  transaction_hash: tx,
  price: 0.0,
  token_id: '',
  side: '',
  outcome_index: 999,
  title: '',
  slug: '',
  icon: '',
  event_slug: '',
  outcome: '',
  name: 'almach',
  pseudonym: 'Rural-Wharf-Parable',
});
const page = (rows, hasMore, cursor) =>
  JSON.stringify({ data: rows, pagination: { limit: 100, offset: 0, has_more: hasMore, next_cursor: cursor } });

// 0.3132 USDC paid 2026-10-30T00:15:21Z for the 10-29 activity day.
const OCT30_PAYMENT = Date.parse('2026-10-30T00:15:21Z');
const CURSOR_1 = 'eyJkYXRhIjp7InR5cGUiOiJhY3Rpdml0eSJ9fQ==';
const FIXTURE_PAGE_1 = page([row(1793319321, 0.3132), row(1792619700, 0.4377)], true, CURSOR_1);
const FIXTURE_PAGE_2 = page([row(1792706100, 0.7396)], false, null);

console.log('reward-ledger-check (#910)');

// ── 1. SSRF: the single-pass allowlist ──────────────────────────────────────
check('target: the allowlisted https base passes', () => {
  for (const raw of ['https://data-api.polymarket.com', 'https://data-api.polymarket.com/']) {
    const t = makeTarget(raw);
    assert.equal(t.ok, true, JSON.stringify(t));
    assert.equal(t.url, `https://${DATA_API_HOST}`, 'trailing slash normalized — request paths append their own');
  }
});
check('target: non-https / wrong host / look-alike / reserved / literal-IP / port / userinfo refused', () => {
  for (const raw of [
    'http://data-api.polymarket.com',
    'https://clob.polymarket.com',
    'https://data-api.polymarket.com.evil.example',
    'https://data-api.polymarket.com@evil.example',
    'https://token@data-api.polymarket.com',
    'https://data-api.polymarket.com:8443',
    'https://localhost/v2/activity',
    'https://data-api.polymarket.com.local',
    'https://data-api.polymarket.com.internal',
    'https://127.0.0.1/v2/activity',
    'https://[::1]/v2/activity',
    'ftp://data-api.polymarket.com',
    'data-api.polymarket.com',
  ]) {
    const t = makeTarget(raw);
    assert.equal(t.ok, false, `${raw} must be refused`);
    assert.ok(t.reason.length > 0, `${raw} must carry a reason`);
  }
});
check('target: is the ONLY path from raw to a request URL', () => {
  assert.throws(() => activityUrl(makeTarget('https://evil.example'), WALLET), /refused target/);
});

// ── 2. request URL construction ─────────────────────────────────────────────
check('activityUrl: user + type=REWARD + limit, cursor only when carried', () => {
  const t = makeTarget('https://data-api.polymarket.com');
  const u = new URL(activityUrl(t, WALLET));
  assert.equal(u.pathname, '/v2/activity');
  assert.equal(u.searchParams.get('user'), WALLET);
  assert.equal(u.searchParams.get('type'), 'REWARD');
  assert.equal(u.searchParams.get('limit'), '100');
  assert.equal(u.searchParams.has('cursor'), false);
  const u2 = new URL(activityUrl(t, WALLET, { cursor: CURSOR_1 }));
  assert.equal(u2.searchParams.get('cursor'), CURSOR_1);
});
check('activityUrl: non-hex or short wallet refused', () => {
  const t = makeTarget('https://data-api.polymarket.com');
  assert.throws(() => activityUrl(t, '0x1234'), /wallet/);
  assert.throws(() => activityUrl(t, 'not-a-wallet'), /wallet/);
});

// ── 3. page parsing: fail-closed envelope, counted row drift ────────────────
check('parse: fixture page yields reward rows with ms timestamps and exact usd strings', () => {
  const p = parseActivityPage(FIXTURE_PAGE_1);
  assert.equal(p.rows.length, 2);
  assert.equal(p.rows[0].atMs, 1793319321 * 1000);
  assert.equal(p.rows[0].usd, '0.3132');
  assert.equal(p.rows[0].txHash, TX);
  assert.equal(p.hasMore, true);
  assert.equal(p.nextCursor, CURSOR_1);
  assert.equal(p.skipped, 0);
});
check('parse: envelope drift fails closed', () => {
  assert.throws(() => parseActivityPage('{"data":[1,2]}'), /has_more/);
  assert.throws(() => parseActivityPage('{"pagination":{"has_more":false}}'), /data/);
  assert.throws(() => parseActivityPage('not json'), /not JSON/);
});
check('parse: non-REWARD and unparsable rows are skipped AND counted, never credited', () => {
  const p = parseActivityPage(
    page(
      [
        row(1793319321, 0.3132),
        { ...row(1793319322, 1.0), type: 'TRADE' },
        { ...row(1793319323, 1.0), timestamp: 'yesterday' },
        { ...row(1793319324, Number('1e-7')) },
      ],
      false,
      null,
    ),
  );
  assert.equal(p.rows.length, 1, 'only the well-formed REWARD row is credited');
  assert.equal(p.skipped, 3, 'drift is counted');
});

// ── 4. archive row: the #388 contract, byte-exact fields ────────────────────
check('archive row: {"at",k:"cashflow",which:"reward",usd} with string decimal', () => {
  const r = toArchiveCashflow({ atMs: 1793319321000, usd: '0.3132' });
  assert.deepEqual(r, { at: 1793319321000, k: 'cashflow', which: 'reward', usd: '0.3132' });
  assert.equal(JSON.stringify(r), '{"at":1793319321000,"k":"cashflow","which":"reward","usd":"0.3132"}');
});

// ── 5. the cursor walk (fake transport, no network) ─────────────────────────
await checkAsync('collect: two pages walked, rows deduped, ends on has_more=false', async () => {
  const pages = [FIXTURE_PAGE_1, FIXTURE_PAGE_2];
  const seen = [];
  const out = await collectRewards({
    target: makeTarget('https://data-api.polymarket.com'),
    wallet: WALLET,
    fetchPage: (url) => {
      seen.push(url);
      return pages[seen.length - 1];
    },
    sleep: async () => {},
  });
  assert.equal(out.pages, 2);
  assert.equal(out.rows.length, 3);
  assert.equal(seen.length, 2, 'second request carries the cursor');
  assert.ok(seen[1].includes(`cursor=${encodeURIComponent(CURSOR_1)}`), seen[1]);
  assert.equal(out.rows[2].usd, '0.7396');
});
await checkAsync('collect: transport failures retry with backoff, then succeed', async () => {
  let calls = 0;
  const sleeps = [];
  const out = await collectRewards({
    target: makeTarget('https://data-api.polymarket.com'),
    wallet: WALLET,
    fetchPage: () => {
      calls += 1;
      if (calls < 3) throw new Error('http 503 (network is flaky today)');
      return FIXTURE_PAGE_2;
    },
    retries: 4,
    backoffMs: 1000,
    sleep: async (ms) => sleeps.push(ms),
  });
  assert.equal(calls, 3, 'two failed attempts + one success');
  assert.equal(out.retriesUsed, 2);
  assert.deepEqual(sleeps, [1000, 2000], 'exponential backoff');
});
await checkAsync('collect: exhausted retries abort loudly — nothing is silently dropped', async () => {
  let calls = 0;
  await assert.rejects(
    collectRewards({
      target: makeTarget('https://data-api.polymarket.com'),
      wallet: WALLET,
      fetchPage: () => {
        calls += 1;
        throw new Error(`http 410 from ${DATA_API_HOST}: Data API v1 has been retired`);
      },
      retries: 2,
      backoffMs: 1,
      sleep: async () => {},
    }),
    (e) => /page 1 failed after 3 attempts/.test(e.message),
  );
  assert.equal(calls, 3);
});
await checkAsync('collect: runaway pagination refuses to spin', async () => {
  await assert.rejects(
    collectRewards({
      target: makeTarget('https://data-api.polymarket.com'),
      wallet: WALLET,
      fetchPage: () => page([row(1793319321, 0.3132)], true, 'next'),
      maxPages: 5,
      backoffMs: 1,
      sleep: async () => {},
    }),
    /5 pages/,
  );
});
await checkAsync('collect: has_more=true without a cursor is contract drift, not a loop', async () => {
  await assert.rejects(
    collectRewards({
      target: makeTarget('https://data-api.polymarket.com'),
      wallet: WALLET,
      fetchPage: () => page([row(1793319321, 0.3132)], true, null),
      backoffMs: 1,
      sleep: async () => {},
    }),
    /no next_cursor/,
  );
});

// ── 6. ledger loading and the exact fold ────────────────────────────────────
check('load: absent ledger is unknown, not zero', () => {
  const l = loadRewardRows(join(tmpdir(), 'reward-ledger-check-does-not-exist.jsonl'));
  assert.equal(l.present, false);
  assert.equal(l.rows.length, 0);
});
check('fold: exact decimal sum over the measured payouts (4-dp survives)', () => {
  const rows = [
    { atMs: OCT30_PAYMENT, usd: '0.3132' },
    { atMs: Date.parse('2026-10-20T00:15:04Z'), usd: '0.4377' },
    { atMs: Date.parse('2026-10-19T00:15:11Z'), usd: '0.7396' },
  ];
  const f = foldRewards(rows, OCT30_PAYMENT - 86_400_000, OCT30_PAYMENT);
  assert.equal(f.windowRows, 1);
  assert.equal(f.rewardsUsd, 0.3132);
  const all = foldRewards(rows, 0, Date.now());
  assert.equal(all.windowRows, 3);
  assert.equal(all.rewardsUsd, 1.4905, '0.3132+0.4377+0.7396 — a float sum of cents fails this');
});
check('fold: empty window is a valid zero, not missing data', () => {
  const f = foldRewards([{ atMs: OCT30_PAYMENT, usd: '0.3132' }], 0, 1);
  assert.deepEqual(f, { windowRows: 0, rewardsUsd: 0 });
});

// ── 7. the report merge, end-to-end (real dryrun-report, tmpdir ledgers) ────
const tmp = mkdtempSync(join(tmpdir(), 'reward-ledger-check-'));
try {
  const tradesPath = join(tmp, 'trades.jsonl');
  const rewardsPath = join(tmp, 'rewards.jsonl');
  const emptyDir = join(tmp, 'evolution');
  // One in-window trade day (2026-10-29) + one reward settlement (10-30 00:15).
  writeFileSync(
    tradesPath,
    `${JSON.stringify({
      id: 't1',
      strategy: 'market_maker',
      netPnlUsd: 1.25,
      feesUsd: 0.1,
      exitTime: Date.parse('2026-10-29T12:00:00Z'),
      wasMakerEntry: true,
    })}\n`,
  );
  writeFileSync(
    rewardsPath,
    `${JSON.stringify({ at: OCT30_PAYMENT, k: 'cashflow', which: 'reward', usd: '0.3132' })}\n${JSON.stringify({
      at: Date.parse('2026-09-06T00:15:00Z'),
      k: 'cashflow',
      which: 'reward',
      usd: '59.9000',
    })}\n`,
  );

  const run = (extra) =>
    spawnSync(
      process.execPath,
      [
        join(ROOT, 'scripts', 'dryrun-report.mjs'),
        '--trades',
        tradesPath,
        '--rewards',
        rewardsPath,
        '--orders',
        join(tmp, 'absent-orders.jsonl'),
        '--evolution-dir',
        emptyDir,
        ...extra,
      ],
      { encoding: 'utf8' },
    );

  const json = run(['--days', '30', '--json']);
  check('report merge: exits 0 and emits a valid manifest', () => {
    assert.equal(json.status, 0, json.stderr);
    JSON.parse(json.stdout);
  });
  const manifest = JSON.parse(json.stdout);
  check('report merge: rewards fold into the income view (separate columns, one total)', () => {
    assert.equal(manifest.rewards.windowRows, 1, 'the 10-30 settlement is in the 30-day window');
    assert.equal(manifest.rewards.rewardsUsd, 0.3132);
    assert.equal(manifest.portfolio.netPnlUsd, 1.25);
    assert.equal(manifest.rewards.netIncludingRewardsUsd, 1.5632);
  });
  check('report merge: trading metrics stay rewards-free', () => {
    assert.equal(manifest.portfolio.closed, 1);
    assert.ok(!('rewardsUsd' in manifest.portfolio), 'aggregate() must not know about rewards');
  });
  check('report merge: per-day fold keyed by PAYMENT day (10-30 shows the cash)', () => {
    const d29 = manifest.perDay.find((d) => d.date === '2026-10-29');
    const d30 = manifest.perDay.find((d) => d.date === '2026-10-30');
    assert.ok(d29, 'trade day present');
    assert.equal(d29.rewardsUsd, null, 'no reward settled on the trade day itself');
    assert.ok(d30, 'a reward-only day still appears in the trend');
    assert.equal(d30.closed, 0);
    assert.equal(d30.rewardsUsd, 0.3132);
  });
  const md = run(['--days', '30']);
  check('report merge: markdown carries the rewards section and day column', () => {
    assert.equal(md.status, 0, md.stderr);
    assert.ok(md.stdout.includes('## Platform rewards'), md.stdout);
    assert.ok(md.stdout.includes('1.5632'), 'net incl. rewards total printed');
    assert.ok(md.stdout.includes('rewards USD |'), 'per-day column header');
    assert.ok(md.stdout.includes('no condition_id'), 'attribution caveat stated');
  });

  // Absent rewards ledger → unknown, not zero (the orderFlow precedent).
  const noRewards = spawnSync(
    process.execPath,
    [
      join(ROOT, 'scripts', 'dryrun-report.mjs'),
      '--trades',
      tradesPath,
      '--rewards',
      join(tmp, 'absent.jsonl'),
      '--orders',
      join(tmp, 'absent-orders.jsonl'),
      '--evolution-dir',
      emptyDir,
      '--days',
      '30',
      '--json',
    ],
    { encoding: 'utf8' },
  );
  check('report merge: absent rewards ledger reports UNKNOWN, not 0', () => {
    assert.equal(noRewards.status, 0, noRewards.stderr);
    const m = JSON.parse(noRewards.stdout);
    assert.equal(m.rewards, null);
    assert.ok(m.rewardsNote.includes('unknown'), m.rewardsNote);
  });
} finally {
  rmSync(tmp, { recursive: true, force: true });
}

if (bad > 0) {
  console.error(`reward-ledger-check: ${bad} FAIL`);
  process.exit(1);
}
console.log('reward-ledger-check: all green');
