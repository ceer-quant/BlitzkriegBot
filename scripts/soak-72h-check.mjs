#!/usr/bin/env node
/**
 * 72-hour dry-run go/no-go (#868).
 *
 * Preflight does not start a kernel and does not read a live socket. It checks
 * the files a 72-hour run would source:
 *
 *   - DRY_RUN=true in production-72h.env and in .env.example
 *   - risk limits in production-72h.env are the armed defaults, staleness is
 *     3500 (not 0)
 *   - the strategy allowlist is a subset of the cost-adjusted report's `allow`
 *     (today: both empty)
 *   - optional `--trades` scores a dry-run log the same way: PF >= 1.1 after
 *     the log's own net, max drawdown <= the daily cap, at least 30 closes
 *
 * An empty trade log is a refusal, not a pass. A preflight with no trades is
 * a CONFIG check only — it cannot promote a strategy to GO.
 *
 *   node scripts/soak-72h-check.mjs --self-test
 *   node scripts/soak-72h-check.mjs --preflight
 *   node scripts/soak-72h-check.mjs --trades data/trades/trades.jsonl
 */
import { readFileSync } from 'fs';
import { dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const ENV_PATH = join(ROOT, 'user_layer', 'configs', 'production-72h.env');
const ALLOW_PATH = join(ROOT, 'user_layer', 'configs', 'production-72h.allowlist');
const EXAMPLE_PATH = join(ROOT, '.env.example');
const REPORT_PATH = join(ROOT, 'docs', 'reports', 'cost-adjusted-edge-72h.json');

const MIN_PF = 1.1;
const MIN_CLOSES = 30;

function argValue(flag) {
  const i = process.argv.indexOf(flag);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function parseEnv(text) {
  const out = {};
  for (const line of text.split('\n')) {
    const t = line.trim();
    if (!t || t.startsWith('#')) continue;
    const eq = t.indexOf('=');
    if (eq < 0) continue;
    out[t.slice(0, eq).trim()] = t.slice(eq + 1).trim();
  }
  return out;
}

function parseAllowlist(text) {
  return text
    .split('\n')
    .map((l) => l.replace(/#.*/, '').trim())
    .filter((l) => l.length > 0);
}

function num(v) {
  const n = Number(v);
  return Number.isFinite(n) ? n : NaN;
}

/**
 * Score closed trades. `net` is the per-close net the log already charged
 * (fees included when the kernel wrote them). Returns a verdict, never throws.
 */
export function scoreCloses(nets, { maxDrawdownUsd }) {
  const problems = [];
  if (!Array.isArray(nets) || nets.length < MIN_CLOSES) {
    problems.push(`closes ${nets ? nets.length : 0} < ${MIN_CLOSES}`);
  }
  let profit = 0;
  let loss = 0;
  let equity = 0;
  let peak = 0;
  let maxDd = 0;
  for (const raw of nets || []) {
    const n = Number(raw);
    if (!Number.isFinite(n)) {
      problems.push('non-numeric net');
      continue;
    }
    equity += n;
    if (equity > peak) peak = equity;
    const dd = peak - equity;
    if (dd > maxDd) maxDd = dd;
    if (n > 0) profit += n;
    else if (n < 0) loss += -n;
  }
  const pf = loss === 0 ? (profit > 0 ? Infinity : 0) : profit / loss;
  if (!(pf >= MIN_PF)) problems.push(`profit factor ${pf} < ${MIN_PF}`);
  if (maxDd > maxDrawdownUsd) problems.push(`max drawdown ${maxDd} > ${maxDrawdownUsd}`);
  return {
    closes: nets ? nets.length : 0,
    profitFactor: pf,
    maxDrawdownUsd: maxDd,
    pass: problems.length === 0,
    problems,
  };
}

function tradeNet(row) {
  if (row == null || typeof row !== 'object') return undefined;
  for (const k of ['netPnlUsd', 'net_pnl_usd', 'pnl']) {
    if (row[k] !== undefined && row[k] !== null && row[k] !== '') return Number(row[k]);
  }
  return undefined;
}

function loadTrades(path) {
  const text = readFileSync(path, 'utf8');
  const nets = [];
  for (const line of text.split('\n')) {
    const t = line.trim();
    if (!t) continue;
    const row = JSON.parse(t);
    const n = tradeNet(row);
    if (n === undefined) continue;
    nets.push(n);
  }
  return nets;
}

function preflight() {
  const problems = [];
  const env = parseEnv(readFileSync(ENV_PATH, 'utf8'));
  const example = parseEnv(readFileSync(EXAMPLE_PATH, 'utf8'));
  const allow = parseAllowlist(readFileSync(ALLOW_PATH, 'utf8'));
  const report = JSON.parse(readFileSync(REPORT_PATH, 'utf8'));

  if (env.DRY_RUN !== 'true') problems.push(`production-72h.env DRY_RUN=${env.DRY_RUN} (must be true)`);
  if (example.DRY_RUN !== 'true') problems.push(`.env.example DRY_RUN=${example.DRY_RUN} (must stay true)`);
  // Issue #967 retired the TOML layer: a leftover `BK_CONFIG` is a pointer at
  // an inert file, and the nine bounds it used to carry must now be env keys.
  if (env.BK_CONFIG !== undefined) {
    problems.push(`BK_CONFIG=${env.BK_CONFIG} (the TOML layer is retired; drop the key)`);
  }
  const stale = num(env.BK_MAX_ORDERBOOK_STALE_MS);
  if (!(stale > 0) || stale !== 3500) problems.push(`env staleness ${env.BK_MAX_ORDERBOOK_STALE_MS} (want 3500, not 0)`);
  // Issue #1189 (PR-E): the retired toml bodies are gone; the env profile is
  // the only arming surface, and the toml-side reconciliation is retired with
  // it. The inert-record checks below (BK_CONFIG pointer) stay.
  const required = {
    max_single_loss_usd: '0.50',
    max_daily_drawdown_usd: '5',
    max_position_size: '5',
    max_consecutive_losses: '4',
    cooldown_minutes: '30',
  };
  // Issue #1189 (PR-E): the toml bodies are retired — the env profile is what
  // arms the boot: every one of the five must be spelled there with the same
  // value. The toml-side reconciliation is retired with the bodies.
  for (const [k, v] of Object.entries(required)) {
    const key = `BK_RISK_${k.toUpperCase()}`;
    if (env[key] !== v) problems.push(`${key}=${env[key]} want ${v}`);
  }
  const daily = num(env.BK_RISK_MAX_DAILY_DRAWDOWN_USD);
  // Issue #991: the fallback settlement policy is read at boot only, so the
  // 72h profile must SPELL it — the soak gate then records exactly which
  // policy the run exercised. `cash` is the default; either value passes the
  // gate, but the key may not be missing. The aging budget must be a number
  // when present (default 172800000 = 48h; <= 0 disables the aging warn).
  if (env.BK_UNVERIFIED_EXPIRY_POLICY === undefined) {
    problems.push('BK_UNVERIFIED_EXPIRY_POLICY is unset (issue 991: spell cash|receivable — the soak must record which policy ran)');
  } else if (env.BK_UNVERIFIED_EXPIRY_POLICY !== 'cash' && env.BK_UNVERIFIED_EXPIRY_POLICY !== 'receivable') {
    problems.push(`BK_UNVERIFIED_EXPIRY_POLICY=${env.BK_UNVERIFIED_EXPIRY_POLICY} (want cash|receivable)`);
  }
  if (env.BK_RECEIVABLE_MAX_AGE_MS !== undefined && !(num(env.BK_RECEIVABLE_MAX_AGE_MS) >= 0)) {
    problems.push(`BK_RECEIVABLE_MAX_AGE_MS=${env.BK_RECEIVABLE_MAX_AGE_MS} (want a non-negative number of ms, empty = 48h default)`);
  }
  if (!(num(env.BK_MAX_DAILY_LOSS_PCT) > 0 && num(env.BK_MAX_DAILY_LOSS_PCT) <= 10)) {
    problems.push(`BK_MAX_DAILY_LOSS_PCT=${env.BK_MAX_DAILY_LOSS_PCT} (want 1..10)`);
  }
  if (!(num(env.BK_MAX_ORDER_NOTIONAL_PCT) > 0)) {
    problems.push('BK_MAX_ORDER_NOTIONAL_PCT is off');
  }
  const allowedByReport = new Set(report.allow || []);
  for (const name of allow) {
    if (!allowedByReport.has(name)) {
      problems.push(`allowlist names ${name}, but the cost report allow set does not`);
    }
  }
  for (const [name, d] of Object.entries(report.decisions || {})) {
    const listed = allowedByReport.has(name);
    if (d.decision === 'GO' && !listed) {
      problems.push(`${name} is GO but missing from report.allow`);
    }
    if (d.decision !== 'GO' && listed) {
      problems.push(`${name} is ${d.decision} but listed in report.allow`);
    }
  }
  return { problems, dailyCap: daily, allow };
}

function runSelfTest() {
  const good = scoreCloses(
    Array.from({ length: 30 }, (_, i) => (i % 5 === 0 ? -0.4 : 0.5)),
    { maxDrawdownUsd: 5 },
  );
  if (!good.pass) {
    console.error('SELF-TEST FAIL: expected a passing curve', good);
    process.exit(1);
  }
  const lowPf = scoreCloses(
    Array.from({ length: 30 }, () => -1),
    { maxDrawdownUsd: 5 },
  );
  if (lowPf.pass) {
    console.error('SELF-TEST FAIL: a losing curve must not pass');
    process.exit(1);
  }
  const deep = scoreCloses([5, -6, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5], {
    maxDrawdownUsd: 5,
  });
  if (deep.pass || !deep.problems.some((p) => p.includes('drawdown'))) {
    console.error('SELF-TEST FAIL: drawdown over the cap must fail', deep);
    process.exit(1);
  }
  const empty = scoreCloses([], { maxDrawdownUsd: 5 });
  if (empty.pass) {
    console.error('SELF-TEST FAIL: empty log must fail');
    process.exit(1);
  }
  const allow = parseAllowlist('# comment\n\npair_discount_arb\n');
  if (allow.length !== 1 || allow[0] !== 'pair_discount_arb') {
    console.error('SELF-TEST FAIL: allowlist parse', allow);
    process.exit(1);
  }
  const commented = parseAllowlist('# pair_discount_arb\n');
  if (commented.length !== 0) {
    console.error('SELF-TEST FAIL: comments must not enable a strategy');
    process.exit(1);
  }
  console.log('soak-72h-check self-test: PASS');
}

function main() {
  if (process.argv.includes('--self-test')) {
    runSelfTest();
    if (!process.argv.includes('--preflight') && !argValue('--trades')) return;
  }
  const flight = preflight();
  const problems = [...flight.problems];
  const trades = argValue('--trades');
  if (trades) {
    const nets = loadTrades(resolve(trades));
    const scored = scoreCloses(nets, { maxDrawdownUsd: flight.dailyCap });
    console.log(
      `trades: closes=${scored.closes} pf=${scored.profitFactor} maxDD=${scored.maxDrawdownUsd} cap=${flight.dailyCap}`,
    );
    problems.push(...scored.problems.map((p) => `trades: ${p}`));
  }
  if (problems.length) {
    console.error('soak-72h: NO-GO');
    for (const p of problems) console.error(`  - ${p}`);
    process.exit(1);
  }
  console.log('soak-72h: PREFLIGHT GO for a dry run with an empty strategy set');
  console.log('  DRY_RUN=true, limits armed, staleness 3500ms, allowlist empty');
  console.log('  This is not a live-trading approval and not a profitability claim.');
  if (!trades) {
    console.log('  No trade log was scored. After 72h, rerun with --trades <trades.jsonl>.');
    console.log('  That rerun is GO only if PF>=1.1, max drawdown<=$5, and closes>=30.');
  }
}

if (process.argv[1] && process.argv[1].endsWith('soak-72h-check.mjs')) {
  try {
    main();
  } catch (e) {
    console.error(e);
    process.exit(1);
  }
}
