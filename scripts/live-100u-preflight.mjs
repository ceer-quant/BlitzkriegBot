#!/usr/bin/env node
/**
 * Read-only preflight for the 100 USDC live profile (issue #870).
 *
 * Orders stay disallowed until every check passes. Nothing here signs a
 * transaction or reads a private key. The chain half is eth_call plus the
 * public data API.
 *
 *   node scripts/live-100u-preflight.mjs --self-test
 *   node scripts/live-100u-preflight.mjs --config
 *   node scripts/live-100u-preflight.mjs
 *       # needs POLYMARKET_FUNDER_ADDRESS; optional POLYGON_RPC_URL
 *
 * Exit 0 means the wallet, allowances, and fill/position reconcile agree
 * with the profile. It does not mean a strategy has an edge, and it does
 * not start a process.
 */
import { readFileSync, existsSync } from 'fs';
import { dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const LIVE_ENV = join(ROOT, 'user_layer', 'configs', 'production-live-100u.env');
const LIVE_ALLOW = join(ROOT, 'user_layer', 'configs', 'production-live-100u.allowlist');
const DRY_ENV = join(ROOT, 'user_layer', 'configs', 'production-72h.env');
const EXAMPLE = join(ROOT, '.env.example');

/** Polymarket's published Polygon collateral and the contracts that pull it. */
export const USDC_E = '0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174';
export const SPENDERS = [
  ['ctfExchange', '0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E'],
  ['negRiskExchange', '0xC5d563A36AE78145C45a50134d48A1215220f80a'],
  ['negRiskAdapter', '0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296'],
];
const USDC_DECIMALS = 6n;
const USDC_SCALE = 10n ** USDC_DECIMALS;
const BALANCE_OF = '70a08231';
const ALLOWANCE = 'dd62ed3e';
const DATA_API = 'https://data-api.polymarket.com';
const DEFAULT_RPC = 'https://polygon-rpc.com';

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
 * File checks. The default DRY_RUN stays true. The live env is allowed to
 * say false because sourcing it is the opt-in, and nothing sources it unless
 * the operator does.
 */
export function judgeConfig({ example, dryEnv, liveEnv, allow }) {
  const problems = [];
  if (example.DRY_RUN !== 'true') problems.push(`.env.example DRY_RUN=${example.DRY_RUN}`);
  if (dryEnv.DRY_RUN !== 'true') problems.push(`production-72h.env DRY_RUN=${dryEnv.DRY_RUN}`);
  if (liveEnv.DRY_RUN !== 'false') {
    problems.push(`production-live-100u.env DRY_RUN=${liveEnv.DRY_RUN} (the opt-in file must say false, and only that file)`);
  }
  // Issue #967 retired the TOML layer: neither file may still point at one, and
  // the nine bounds must arrive as env keys (an inert `.env` key arms nothing).
  if (example.BK_CONFIG !== undefined) problems.push(`.env.example BK_CONFIG=${example.BK_CONFIG}`);
  if (liveEnv.BK_CONFIG !== undefined) {
    problems.push(`live BK_CONFIG=${liveEnv.BK_CONFIG} (the TOML layer is retired; drop the key)`);
  }
  if (num(liveEnv.BK_LIVE_BANKROLL_USD) !== 100) {
    problems.push(`bankroll ${liveEnv.BK_LIVE_BANKROLL_USD} (want 100)`);
  }
  const notional = num(liveEnv.BK_LIVE_MAX_ORDER_NOTIONAL);
  if (!(notional >= 1 && notional <= 2)) {
    problems.push(`per-order notional ${liveEnv.BK_LIVE_MAX_ORDER_NOTIONAL} outside $1-2`);
  }
  if (num(liveEnv.BK_MAX_ORDER_NOTIONAL_PCT) !== 2) {
    problems.push(`BK_MAX_ORDER_NOTIONAL_PCT=${liveEnv.BK_MAX_ORDER_NOTIONAL_PCT} (2% of 100 USDC is $2)`);
  }
  if (num(liveEnv.BK_SIZE_PCT) !== 1) {
    problems.push(`BK_SIZE_PCT=${liveEnv.BK_SIZE_PCT} (1% of 100 USDC is $1)`);
  }
  const minShares = num(liveEnv.BK_LIVE_MIN_SHARES);
  const maxShares = num(liveEnv.BK_LIVE_MAX_SHARES);
  if (!(minShares >= 1) || !(maxShares >= minShares) || maxShares > notional) {
    problems.push(`share band ${minShares}..${maxShares} does not fit under $${notional} at a $1 price`);
  }
  if (num(liveEnv.BK_MAX_ORDERBOOK_STALE_MS) !== 3500) {
    problems.push('stale-book guard is not 3500ms');
  }
  if (num(liveEnv.BK_MAX_DAILY_LOSS) !== 5) problems.push('daily loss cap is not $5');
  const required = {
    max_single_loss_usd: '0.50',
    max_daily_drawdown_usd: '5',
    max_position_size: '5',
    max_consecutive_losses: '4',
    cooldown_minutes: '30',
  };
  // Issue #1189 (PR-E): the retired toml bodies are gone — the live env is
  // what arms the boot; the toml-side reconciliation is retired with them.
  for (const [k, v] of Object.entries(required)) {
    if (liveEnv[`BK_RISK_${k.toUpperCase()}`] !== v) {
      problems.push(`${k}=${liveEnv[`BK_RISK_${k.toUpperCase()}`]} want ${v}`);
    }
  }
  if (allow.length !== 0) problems.push(`allowlist is not empty: ${allow.join(',')}`);
  return { problems, pass: problems.length === 0 };
}

/**
 * Collateral and approvals. `balance` and each allowance are USDC base units
 * (6 decimals) as bigint. A max-uint allowance counts as covered.
 */
export function judgeWallet({ balance, allowances, bankrollUsd = 100 }) {
  const problems = [];
  const usd = Number(balance) / 1e6;
  const lo = bankrollUsd * 0.9;
  const hi = bankrollUsd * 1.1;
  if (!(usd >= lo && usd <= hi)) {
    problems.push(`USDC.e balance ${usd} outside [${lo}, ${hi}] for a ${bankrollUsd} USDC bankroll`);
  }
  const need = balance;
  for (const [name, raw] of Object.entries(allowances || {})) {
    let value;
    try {
      value = BigInt(raw);
    } catch {
      problems.push(`allowance ${name} is not an integer`);
      continue;
    }
    if (value < need) problems.push(`allowance ${name} ${value} < balance ${need}`);
  }
  return { problems, pass: problems.length === 0, balanceUsd: usd };
}

function normToken(v) {
  if (v === undefined || v === null) return '';
  return String(v).trim();
}

function normTx(v) {
  if (!v) return '';
  return String(v).trim().toLowerCase();
}

export function rowsFromPositions(rows) {
  const out = [];
  for (const row of rows || []) {
    const token = normToken(row.asset ?? row.tokenId ?? row.token_id);
    const size = Number(row.size ?? row.shares ?? row.amount ?? 0);
    if (!token || !Number.isFinite(size) || Math.abs(size) < 1e-9) continue;
    out.push({ token, size });
  }
  return out;
}

export function rowsFromTrades(rows) {
  const out = [];
  for (const row of rows || []) {
    const tx = normTx(row.transactionHash ?? row.txHash ?? row.tx_hash);
    if (!tx) continue;
    out.push({ tx });
  }
  return out;
}

/**
 * Open inventory must match. When a local fill log exists, the tx-hash sets
 * must match. A missing local log is acceptable only for a flat chain (no
 * positions and no fills). A full trade page is an incomplete read and fails.
 */
export function judgeReconcile({
  chainPositions,
  localPositions,
  chainTrades,
  localTrades,
  tradeLogPresent,
  truncated,
}) {
  const problems = [];
  if (truncated) problems.push('chain trade page is full; reconcile is incomplete');
  const chainPos = new Map();
  for (const row of chainPositions) {
    chainPos.set(row.token, (chainPos.get(row.token) || 0) + row.size);
  }
  const localPos = new Map();
  for (const row of localPositions) {
    localPos.set(row.token, (localPos.get(row.token) || 0) + row.size);
  }
  const tokens = new Set([...chainPos.keys(), ...localPos.keys()]);
  for (const token of tokens) {
    const c = chainPos.get(token) || 0;
    const l = localPos.get(token) || 0;
    if (Math.abs(c - l) > 0.01) problems.push(`position ${token}: chain ${c} local ${l}`);
  }
  const chainTx = new Set(chainTrades.map((t) => t.tx));
  const localTx = new Set(localTrades.map((t) => t.tx));
  if (!tradeLogPresent) {
    if (chainPos.size > 0 || chainTx.size > 0) {
      problems.push('local fill log missing while the chain shows positions or fills');
    }
  } else {
    for (const tx of chainTx) {
      if (!localTx.has(tx)) problems.push(`chain fill ${tx} missing locally`);
    }
    for (const tx of localTx) {
      if (!chainTx.has(tx)) problems.push(`local fill ${tx} missing on chain`);
    }
  }
  return { problems, pass: problems.length === 0 };
}

export function ordersAllowed(config, wallet, reconcile) {
  return Boolean(config?.pass && wallet?.pass && reconcile?.pass);
}

function padAddress(addr) {
  const hex = String(addr).toLowerCase().replace(/^0x/, '');
  if (!/^[0-9a-f]{40}$/.test(hex)) throw new Error(`bad address ${addr}`);
  return hex.padStart(64, '0');
}

export function encodeBalanceOf(holder) {
  return `0x${BALANCE_OF}${padAddress(holder)}`;
}

export function encodeAllowance(owner, spender) {
  return `0x${ALLOWANCE}${padAddress(owner)}${padAddress(spender)}`;
}

async function ethCall(rpc, to, data) {
  const res = await fetch(rpc, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'eth_call', params: [{ to, data }, 'latest'] }),
  });
  if (!res.ok) throw new Error(`rpc http ${res.status}`);
  const body = await res.json();
  if (body.error) throw new Error(body.error.message || 'rpc error');
  if (typeof body.result !== 'string') throw new Error('rpc result missing');
  return BigInt(body.result);
}

function loadJsonl(path) {
  if (!path || !existsSync(path)) return { present: false, rows: [] };
  const rows = [];
  for (const line of readFileSync(path, 'utf8').split('\n')) {
    const t = line.trim();
    if (!t) continue;
    rows.push(JSON.parse(t));
  }
  return { present: true, rows };
}

async function readChain(funder, rpc) {
  const balance = await ethCall(rpc, USDC_E, encodeBalanceOf(funder));
  const allowances = {};
  for (const [name, spender] of SPENDERS) {
    allowances[name] = await ethCall(rpc, USDC_E, encodeAllowance(funder, spender));
  }
  const posRes = await fetch(`${DATA_API}/positions?user=${funder}`);
  if (!posRes.ok) throw new Error(`positions http ${posRes.status}`);
  const positions = await posRes.json();
  const tradeRes = await fetch(`${DATA_API}/trades?user=${funder}&limit=500`);
  if (!tradeRes.ok) throw new Error(`trades http ${tradeRes.status}`);
  const trades = await tradeRes.json();
  if (!Array.isArray(positions) || !Array.isArray(trades)) {
    throw new Error('data API did not return arrays');
  }
  return {
    balance,
    allowances,
    positions: rowsFromPositions(positions),
    trades: rowsFromTrades(trades),
    truncated: trades.length >= 500,
  };
}

function loadConfig() {
  return judgeConfig({
    example: parseEnv(readFileSync(EXAMPLE, 'utf8')),
    dryEnv: parseEnv(readFileSync(DRY_ENV, 'utf8')),
    liveEnv: parseEnv(readFileSync(LIVE_ENV, 'utf8')),
    allow: parseAllowlist(readFileSync(LIVE_ALLOW, 'utf8')),
  });
}

function runSelfTest() {
  const fail = (msg) => {
    console.error(`SELF-TEST FAIL ${msg}`);
    process.exit(1);
  };
  const good = judgeConfig({
    example: { DRY_RUN: 'true' },
    dryEnv: { DRY_RUN: 'true' },
    liveEnv: parseEnv(readFileSync(LIVE_ENV, 'utf8')),
    allow: [],
  });
  if (!good.pass) fail(`shipped files: ${good.problems.join('; ')}`);
  const flipped = judgeConfig({
    example: { DRY_RUN: 'false' },
    dryEnv: { DRY_RUN: 'true' },
    liveEnv: parseEnv(readFileSync(LIVE_ENV, 'utf8')),
    allow: [],
  });
  if (flipped.pass) fail('a default DRY_RUN=false must fail');
  const named = judgeConfig({
    example: { DRY_RUN: 'true' },
    dryEnv: { DRY_RUN: 'true' },
    liveEnv: parseEnv(readFileSync(LIVE_ENV, 'utf8')),
    allow: ['pair_discount_arb'],
  });
  if (named.pass) fail('a non-empty allowlist must fail');

  const micro = (usd) => BigInt(Math.round(usd * 1e6));
  const wallet = judgeWallet({
    balance: micro(100),
    allowances: { ctfExchange: micro(100), negRiskExchange: micro(1000), negRiskAdapter: micro(100) },
  });
  if (!wallet.pass) fail(`100 USDC wallet: ${wallet.problems.join('; ')}`);
  const short = judgeWallet({
    balance: micro(50),
    allowances: { ctfExchange: micro(50) },
  });
  if (short.pass) fail('a 50 USDC wallet must fail the 100U band');
  const unapproved = judgeWallet({
    balance: micro(100),
    allowances: { ctfExchange: 0n },
  });
  if (unapproved.pass) fail('a zero allowance must fail');
  const unlimited = judgeWallet({
    balance: micro(100),
    allowances: { ctfExchange: (1n << 256n) - 1n },
  });
  if (!unlimited.pass) fail('max-uint allowance must cover the balance');

  const flat = judgeReconcile({
    chainPositions: [],
    localPositions: [],
    chainTrades: [],
    localTrades: [],
    tradeLogPresent: false,
    truncated: false,
  });
  if (!flat.pass) fail('a flat wallet with no local log must pass');
  const ghost = judgeReconcile({
    chainPositions: [{ token: '1', size: 2 }],
    localPositions: [],
    chainTrades: [],
    localTrades: [],
    tradeLogPresent: true,
    truncated: false,
  });
  if (ghost.pass) fail('a chain position missing locally must fail');
  const fills = judgeReconcile({
    chainPositions: [],
    localPositions: [],
    chainTrades: [{ tx: '0xabc' }],
    localTrades: [{ tx: '0xabc' }],
    tradeLogPresent: true,
    truncated: false,
  });
  if (!fills.pass) fail('matching fill hashes must pass');
  const missing = judgeReconcile({
    chainPositions: [],
    localPositions: [],
    chainTrades: [{ tx: '0xabc' }],
    localTrades: [],
    tradeLogPresent: true,
    truncated: false,
  });
  if (missing.pass) fail('an unlogged chain fill must fail');
  const page = judgeReconcile({
    chainPositions: [],
    localPositions: [],
    chainTrades: [],
    localTrades: [],
    tradeLogPresent: false,
    truncated: true,
  });
  if (page.pass) fail('a full trade page must fail');
  if (ordersAllowed(good, wallet, flat) !== true) fail('all-pass must allow orders');
  if (ordersAllowed(good, short, flat) !== false) fail('a failed wallet must block orders');
  if (encodeBalanceOf('0x' + 'ab'.repeat(20)).length !== 2 + 8 + 64) fail('balanceOf encoding length');
  console.log('live-100u-preflight self-test: PASS');
}

async function main() {
  const selfTest = process.argv.includes('--self-test');
  const configOnly = process.argv.includes('--config');
  if (selfTest) runSelfTest();
  if (selfTest && !configOnly && !process.argv.includes('--chain')) return;

  const config = loadConfig();
  if (!config.pass) {
    console.error('live-100u: NO-GO (config)');
    for (const p of config.problems) console.error(`  - ${p}`);
    process.exit(1);
  }
  if (configOnly || (selfTest && !process.argv.includes('--chain'))) {
    console.log('live-100u: CONFIG GO. Orders are not allowed yet; the chain reconcile has not run.');
    console.log('  DRY_RUN default is still true. No strategy is enabled.');
    return;
  }

  const funder = process.env.POLYMARKET_FUNDER_ADDRESS || process.env.FUNDER_ADDRESS || '';
  if (!funder) {
    console.error('live-100u: NO-GO. POLYMARKET_FUNDER_ADDRESS is unset, so balance and allowances were not read.');
    console.error('  Orders stay disallowed. The private key is not required for this preflight and was not read.');
    process.exit(2);
  }
  const rpc = process.env.POLYGON_RPC_URL || DEFAULT_RPC;
  const chain = await readChain(funder, rpc);
  const wallet = judgeWallet({ balance: chain.balance, allowances: chain.allowances });
  const tradePath = argValue('--trades') || join(ROOT, 'data', 'trades', 'trades.jsonl');
  const posPath = argValue('--positions') || join(ROOT, 'data', 'positions', 'positions.jsonl');
  const localTrades = loadJsonl(tradePath);
  const localPositions = loadJsonl(posPath);
  const reconcile = judgeReconcile({
    chainPositions: chain.positions,
    localPositions: rowsFromPositions(localPositions.rows),
    chainTrades: chain.trades,
    localTrades: rowsFromTrades(localTrades.rows),
    tradeLogPresent: localTrades.present || localPositions.present,
    truncated: chain.truncated,
  });
  console.log(`live-100u: USDC.e balance ${wallet.balanceUsd} (band 90..110)`);
  console.log(`  chain positions ${chain.positions.length}, chain fills ${chain.trades.length}`);
  const problems = [...wallet.problems, ...reconcile.problems];
  if (!ordersAllowed(config, wallet, reconcile)) {
    console.error('live-100u: NO-GO. Orders stay disallowed.');
    for (const p of problems) console.error(`  - ${p}`);
    process.exit(1);
  }
  console.log('live-100u: READONLY PREFLIGHT GO. Point the already-running process at production-live-100u.toml.');
  console.log('  This is not a strategy GO and not a profitability claim. Allowlist stays empty.');
}

if (process.argv[1] && process.argv[1].endsWith('live-100u-preflight.mjs')) {
  main().catch((e) => {
    console.error(`live-100u: NO-GO. ${e.message || e}`);
    console.error('  Orders stay disallowed.');
    process.exit(1);
  });
}
