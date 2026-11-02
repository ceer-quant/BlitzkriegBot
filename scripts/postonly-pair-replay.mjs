#!/usr/bin/env node
/**
 * Conservative maker-fill replay for postonly_pair (issue #872).
 *
 * A leg fills only when a LATER book in the same round prints an ask at or
 * below the resting quote, and only while more than cancel_before_sec remains.
 * Both legs filling locks 1 - (quote_up + quote_down). Maker fee is 0. The
 * 20% rebate is not credited. One leg filling is a total loss of that quote
 * (the naked share can settle to 0 under Chainlink TWAP; this model does not
 * assume a partial recovery). Rounds that never trade through are not closes.
 *
 *   node scripts/postonly-pair-replay.mjs --self-test
 *   node scripts/postonly-pair-replay.mjs --write
 *   node scripts/postonly-pair-replay.mjs --check
 */
import { readFileSync, writeFileSync, rmSync, createReadStream } from 'fs';
import { createInterface } from 'readline';
import { dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';
import { WINDOWS, materialize } from './lib/frozen-corpus.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const JSON_PATH = join(ROOT, 'docs', 'reports', 'postonly-pair-replay.json');
const MD_PATH = join(ROOT, 'docs', 'reports', 'postonly-pair-replay.md');
const MICRO = 1_000_000n;
const TICK = 10_000n;
const MIN_PX = 10_000n;
const MAX_COST = 985_000n;
const CANCEL_BEFORE_SEC = 20;
const REQUOTE_MS = 15_000;

function bestAsk(levels) {
  if (!Array.isArray(levels)) return null;
  let best = null;
  for (const level of levels) {
    if (!Array.isArray(level) || level.length < 2) continue;
    const px = parseMicro(level[0]);
    const sz = parseMicro(level[1]);
    if (px === null || sz === null || sz <= 0n || px <= 0n || px >= MICRO) continue;
    if (best === null || px < best) best = px;
  }
  return best;
}

function parseMicro(raw) {
  const s = String(raw).trim();
  if (!/^-?\d+(\.\d+)?$/.test(s)) return null;
  const neg = s.startsWith('-');
  const t = neg ? s.slice(1) : s;
  const [w, f = ''] = t.split('.');
  const v = BigInt(w) * MICRO + BigInt((f + '000000').slice(0, 6));
  return neg ? -v : v;
}

function formatMicro(n) {
  const neg = n < 0n;
  const v = neg ? -n : n;
  const whole = v / MICRO;
  const frac = (v % MICRO).toString().padStart(6, '0').replace(/0+$/, '');
  const body = frac.length ? `${whole}.${frac}` : `${whole}`;
  return neg ? `-${body}` : body;
}

function quoteOf(ask) {
  const q = ask - TICK;
  if (q < MIN_PX || q >= ask) return null;
  return q;
}

/**
 * One round of books. `points` are {at, up, down, timeLeftSec} in time order.
 * Returns null when the strategy never quoted, else the close.
 */
export function replayRound(points) {
  let quote = null;
  let upFilled = false;
  let downFilled = false;
  let quotedLegs = 0;
  let filledLegs = 0;
  for (const p of points) {
    if (p.timeLeftSec <= CANCEL_BEFORE_SEC) {
      quote = null;
      continue;
    }
    const qu = quoteOf(p.up);
    const qd = quoteOf(p.down);
    const tradable = qu !== null && qd !== null && qu + qd < MAX_COST;
    if (!quote && tradable) {
      quote = { up: qu, down: qd, at: p.at };
      quotedLegs += 2;
    } else if (quote && p.at - quote.at >= REQUOTE_MS && tradable) {
      quote = { up: qu, down: qd, at: p.at };
      upFilled = false;
      downFilled = false;
      quotedLegs += 2;
    }
    if (!quote) continue;
    if (!upFilled && p.up <= quote.up) {
      upFilled = true;
      filledLegs += 1;
    }
    if (!downFilled && p.down <= quote.down) {
      downFilled = true;
      filledLegs += 1;
    }
  }
  if (!quote && quotedLegs === 0) return null;
  let pnl = null;
  let kind = 'unfilled';
  if (upFilled && downFilled && quote) {
    pnl = MICRO - quote.up - quote.down;
    kind = 'paired';
  } else if ((upFilled || downFilled) && quote) {
    pnl = upFilled ? -quote.up : -quote.down;
    kind = 'unpaired';
  }
  return { pnl, kind, quotedLegs, filledLegs };
}

export function summarize(closes) {
  let profit = 0n;
  let loss = 0n;
  let net = 0n;
  let adverse = 0n;
  let quoted = 0;
  let filled = 0;
  let paired = 0;
  let unpaired = 0;
  for (const c of closes) {
    quoted += c.quotedLegs;
    filled += c.filledLegs;
    if (c.kind === 'paired') paired += 1;
    if (c.kind === 'unpaired') unpaired += 1;
    if (c.pnl === null) continue;
    net += c.pnl;
    if (c.pnl > 0n) profit += c.pnl;
    else if (c.pnl < 0n) {
      loss += -c.pnl;
      adverse += -c.pnl;
    }
  }
  const scored = closes.filter((c) => c.pnl !== null).length;
  let pf = 'na';
  if (loss === 0n && profit > 0n) pf = 'inf';
  else if (loss > 0n) pf = formatMicro((profit * MICRO + loss / 2n) / loss);
  const fillRate = quoted === 0 ? 'na' : formatMicro((BigInt(filled) * MICRO) / BigInt(quoted));
  return {
    attempts: closes.length,
    scored,
    paired,
    unpaired,
    netUsd: formatMicro(net),
    profitFactor: pf,
    adverseSelectionUsd: formatMicro(adverse),
    fillRate,
    decision: scored >= 30 && (pf === 'inf' || Number(pf) >= 1.1) ? 'GO' : 'NO-GO',
  };
}

async function scanWindow(name) {
  const spec = WINDOWS.find((w) => w.name === name);
  const { path, dir } = materialize(ROOT, spec);
  const meta = new Map();
  const asks = new Map();
  const rounds = new Map();
  try {
    const rl = createInterface({ input: createReadStream(path), crlfDelay: Infinity });
    for await (const line of rl) {
      if (!line) continue;
      const ev = JSON.parse(line);
      if (ev.k === 'round' && Array.isArray(ev.m)) {
        for (const m of ev.m) {
          if (!m.conditionId || m.roundSlot === undefined) continue;
          const key = `${m.conditionId}|${m.roundSlot}`;
          meta.set(m.upTokenId, { key, side: 'up', expires: m.expiresAtMs });
          meta.set(m.downTokenId, { key, side: 'down', other: m.upTokenId, expires: m.expiresAtMs });
          meta.set(m.upTokenId, { key, side: 'up', other: m.downTokenId, expires: m.expiresAtMs });
        }
        continue;
      }
      if (ev.k !== 'book') continue;
      const info = meta.get(ev.t);
      if (!info) continue;
      const px = bestAsk(ev.a);
      if (px === null) continue;
      asks.set(ev.t, { px, at: ev.at });
      const other = asks.get(info.other);
      if (!other || Math.abs(ev.at - other.at) > 3500) continue;
      const up = info.side === 'up' ? px : other.px;
      const down = info.side === 'down' ? px : other.px;
      const expires = info.expires;
      const timeLeftSec = Math.floor((expires - ev.at) / 1000);
      let bucket = rounds.get(info.key);
      if (!bucket) {
        bucket = [];
        rounds.set(info.key, bucket);
      }
      bucket.push({ at: ev.at, up, down, timeLeftSec });
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  const closes = [];
  for (const points of rounds.values()) {
    points.sort((a, b) => a.at - b.at);
    const result = replayRound(points);
    if (result) closes.push(result);
  }
  return closes;
}

function render(report) {
  const s = report.summary;
  return [
    '# postonly_pair 保守回放',
    '',
    `- 日期：${report.generatedOn}`,
    '- 问题：#872',
    '- 语料：`docs/reports/data/mean-reversion-gate/` 四个一小时窗口',
    '- 脚本：`node scripts/postonly-pair-replay.mjs --check`',
    '',
    '## 模型',
    '',
    '报价是 ask 下一档。两边报价之和必须低于 0.985 才挂单。一条腿只有在之后的盘口把 ask 打到报价或更低时才算成交，而且必须还剩 20 秒以上。两条腿都打到，盈亏是 `1 - 报价之和`，maker 费按 0，不算 20% rebate。只打到一条腿，按这条腿的报价全额亏损（裸腿可以结算成 0；Chainlink TWAP 不在这里被猜成更有利的价格）。没打到的轮次不是平仓。',
    '',
    '## 结果',
    '',
    `| 指标 | 值 |`,
    `| --- | ---: |`,
    `| 挂单的轮 | ${s.attempts} |`,
    `| 有成交的轮 | ${s.scored} |`,
    `| 配成对 | ${s.paired} |`,
    `| 裸腿 | ${s.unpaired} |`,
    `| 净盈亏 USD | ${s.netUsd} |`,
    `| PF | ${s.profitFactor} |`,
    `| 逆向选择 USD | ${s.adverseSelectionUsd} |`,
    `| 腿成交率 | ${s.fillRate} |`,
    `| 判定 | ${s.decision} |`,
    '',
    '策略默认不启用。`DRY_RUN` 没有改。成交日志上的平仓仍写 `netPnlUsd`；`postonly_pair` 的 maker 平仓另外写 `adverseSelectionUsd` 和 `makerFillRate`，只读 `netPnlUsd` 的打分脚本会忽略这两个字段。',
    '',
  ].join('\n');
}

function runSelfTest() {
  const fail = (m) => {
    console.error(`SELF-TEST FAIL ${m}`);
    process.exit(1);
  };
  const paired = replayRound([
    { at: 0, up: 400_000n, down: 400_000n, timeLeftSec: 100 },
    { at: 1000, up: 390_000n, down: 390_000n, timeLeftSec: 99 },
  ]);
  if (!paired || paired.kind !== 'paired' || paired.pnl !== 220_000n) fail(`paired ${paired && paired.pnl}`);
  const naked = replayRound([
    { at: 0, up: 400_000n, down: 400_000n, timeLeftSec: 100 },
    { at: 1000, up: 390_000n, down: 500_000n, timeLeftSec: 99 },
  ]);
  if (!naked || naked.kind !== 'unpaired' || naked.pnl !== -390_000n) fail(`naked ${naked && naked.pnl}`);
  const late = replayRound([
    { at: 0, up: 400_000n, down: 400_000n, timeLeftSec: 10 },
    { at: 1000, up: 100_000n, down: 100_000n, timeLeftSec: 9 },
  ]);
  if (late !== null) fail('inside the cancel window must not quote');
  const sum = summarize([
    { pnl: 100_000n, kind: 'paired', quotedLegs: 2, filledLegs: 2 },
    { pnl: -50_000n, kind: 'unpaired', quotedLegs: 2, filledLegs: 1 },
  ]);
  if (sum.profitFactor !== '2' || sum.adverseSelectionUsd !== '0.05') fail(JSON.stringify(sum));
  if (summarize([]).decision !== 'NO-GO') fail('empty sample must be NO-GO');
  console.log('postonly-pair-replay self-test: PASS');
}

async function main() {
  const selfTest = process.argv.includes('--self-test');
  const write = process.argv.includes('--write');
  const check = process.argv.includes('--check') || (!selfTest && !write);
  if (selfTest) runSelfTest();
  if (selfTest && !write && !process.argv.includes('--check')) return;
  const closes = [];
  for (const w of WINDOWS) closes.push(...await scanWindow(w.name));
  const report = {
    generatedOn: '2026-11-02',
    issue: 872,
    summary: summarize(closes),
  };
  const body = JSON.stringify(report, null, 2) + '\n';
  const md = render(report);
  if (write) {
    writeFileSync(JSON_PATH, body);
    writeFileSync(MD_PATH, md);
    console.log(JSON.stringify(report.summary));
    return;
  }
  if (check) {
    if (readFileSync(JSON_PATH, 'utf8') !== body || readFileSync(MD_PATH, 'utf8') !== md) {
      console.error('postonly-pair-replay: committed report != fresh scan. Run --write.');
      process.exit(1);
    }
    if (report.summary.decision !== 'NO-GO') {
      console.error('postonly-pair-replay: refusing a GO until the sample and the costs say so');
      process.exit(1);
    }
    console.log(`postonly-pair-replay check: PASS (${report.summary.decision}, net ${report.summary.netUsd})`);
  }
}

if (process.argv[1] && process.argv[1].endsWith('postonly-pair-replay.mjs')) {
  main().catch((e) => {
    console.error(e);
    process.exit(1);
  });
}
