#!/usr/bin/env node
/**
 * account-cross-isolation-check — DEV_V0_3 §16.4 E28 gate (§9.3 / G5).
 *
 * The contract: accounts are SEPARATE WALLETS (§9.3), not one ledger with
 * aliases. Driven over the real UDS wire against a real dry core booted from
 * a scratch `user_layer/configs/accounts.toml` declaring TWO accounts:
 *
 *   1. `account.list` reports both books (sorted ids, per-account money,
 *      `version: "1.1"`), and the reported default is the process-level one;
 *   2. an order with no accountId spends the ACTIVE account's book, and the
 *      position it opens CARRIES that accountId (§9.2 贯穿);
 *   3. a CLOSE naming a DIFFERENT account than the position's is REFUSED —
 *      `cross-account close accepted` is the red flag (G5 reverse A);
 *   4. an order naming an UNCONFIGURED account is refused and no such
 *      account ever appears in `account.list` — `implicit ledger` is the
 *      red flag (reverse C: no implicit book, ever);
 *   5. a frozen account refuses new entries (ACCOUNT_LIMIT) while its view
 *      carries the posture (Gate 2 at the wire);
 *   6. money facts stay per-account: A's entry moves A's cash and never
 *      touches B's balance.
 *
 * Usage:
 *   node scripts/account-cross-isolation-check.mjs          # the real probes
 *   node scripts/account-cross-isolation-check.mjs --teeth  # must go red
 * Exit: 0 pass / 1 failure / 2 environment missing.
 */

import { existsSync, mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';
import { CoreClient, rpc } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { coreBinaryPath, checkCoreProvenance } from './lib/core-provenance.mjs';
import { createChecks } from './lib/gate-harness.mjs';

const BIN = coreBinaryPath();
const SEED = 10000;

const gate = createChecks();
const { check } = gate;

// ── pure judges — the teeth surface ──────────────────────────────────────────

/** The cross-account close verdict for ONE wire reply (G5 reverse A). */
export function judgeCrossClose(reply) {
  if (reply?.error) {
    const msg = String(reply.error.message ?? '');
    if (!msg.includes('cross-account close refused')) {
      return [`close was refused but the refusal lost its vocabulary: ${msg.slice(0, 140)}`];
    }
    return [];
  }
  return [
    'cross-account close accepted — an order would settle money into a book that never paid for the shares',
  ];
}

/** The unknown-account verdict: the order must be refused AND no book created. */
export function judgeUnknownAccount(reply, listReply) {
  const problems = [];
  if (!reply?.error) {
    problems.push('implicit ledger — an order for an unconfigured account was accepted');
  }
  const ids = (listReply?.result?.accounts ?? listReply?.accounts ?? []).map((a) => a?.id);
  if (ids.includes('ghost')) {
    problems.push('implicit ledger — `ghost` appeared in account.list with a book');
  }
  return problems;
}

function teeth() {
  // §16.6: feed the broken implementations' OUTPUT to the judges; each must go
  // red naming the §1831/§1832 anchor, else this gate has no teeth.
  const mutations = [
    {
      name: 'teeth A: the kernel accepted a cross-account close',
      probe: () => judgeCrossClose({ result: { orderId: 'o-x', status: 'live' } }),
      mention: 'cross-account close accepted',
    },
    {
      name: 'teeth C: an unconfigured account got a book and an accepted order',
      probe: () =>
        judgeUnknownAccount(
          { result: { orderId: 'o-g', status: 'pending' } },
          { result: { accounts: [{ id: 'default' }, { id: 'paper' }, { id: 'ghost' }] } },
        ),
      mention: 'implicit ledger',
    },
    {
      name: 'teeth: the refusal survived but lost the naming vocabulary',
      probe: () => judgeCrossClose({ error: { message: 'RiskRejected: rejected' } }),
      mention: 'lost its vocabulary',
    },
  ];
  let missed = 0;
  for (const m of mutations) {
    const problems = m.probe();
    const caught = problems.some((p) => p.includes(m.mention));
    console.log(`${caught ? '  ok  ' : '  FAIL'} teeth: ${m.name}`);
    if (caught) console.log(`       caught: ${problems.find((p) => p.includes(m.mention)).slice(0, 140)}`);
    else missed += 1;
  }
  if (missed === 0) {
    console.log('\naccount:isolation --teeth: every broken output was caught — the gate has teeth');
    process.exit(0);
  }
  console.error(`\naccount:isolation --teeth: ${missed} mutation(s) slipped through green — the gate has no teeth`);
  process.exit(1);
}

// ── the real probes ──────────────────────────────────────────────────────────

const BOOK_TOML = `\
# Scratch book for the isolation gate: two wallets, not aliases (§9.3).
[[account]]
id = "default"
name = "default"

[[account]]
id = "paper"
name = "paper trading"
`;

/** A dry core in a scratch dir whose cwd CARRIES the two-account book. */
function makeCore(label) {
  const cwd = mkdtempSync(join(tmpdir(), `bk-e28-iso-${label}-`));
  mkdirSync(join(cwd, 'user_layer', 'configs'), { recursive: true });
  writeFileSync(join(cwd, 'user_layer', 'configs', 'accounts.toml'), BOOK_TOML);
  return new CoreClient({
    binaryPath: BIN,
    socketPath: scratchSocketPath(label),
    mode: 'dry',
    seedBalance: SEED,
    maxOrderNotional: 100,
    tickMs: 20,
    autoRestart: false,
    cwd,
    noTradeLog: true,
    noOrderLog: true,
    noPositionLog: true,
    extraArgs: ['--no-auto-exits'],
  });
}

const order = (side, tokenId, price, size, key, account, asset = 'BTC') => ({
  tokenId,
  conditionId: `cond-${tokenId}`,
  side,
  mode: 'taker',
  price,
  size,
  internalKey: key,
  strategy: 'iso',
  asset,
  direction: 'up',
  roundSlot: 1,
  ...(account ? { accountId: account } : {}),
});

const book = (tokenId, bids, asks) =>
  JSON.stringify({
    jsonrpc: '2.0', id: 1, method: 'books.snapshot',
    params: { tokenId, bids: bids.map(([p, s]) => ({ price: p, size: s })), asks: asks.map(([p, s]) => ({ price: p, size: s })) },
  });

async function main() {
  // §16.6: the teeth must be runnable WITHOUT a built binary — a gate that
  // cannot self-certify on a bare checkout is a gate nobody runs.
  if (process.argv.includes('--teeth')) teeth();
  if (!existsSync(BIN)) {
    console.error(`missing binary: ${BIN} (cargo build --release --workspace)`);
    process.exit(2);
  }
  const dirOf = (c) => c.cwd;
  const core = makeCore('iso');
  try {
    checkCoreProvenance(BIN, check);
    await core.start();
    const ready = await rpc.ready(core);
    console.log(`  core dry=${ready.build ?? '?'} commit=${ready.commit ?? '?'}`);

    // 1. The book at the wire: two accounts, process default, both seeded.
    const list = await rpc.accountList(core);
    check('account.list carries the 1.1 envelope', list.version === '1.1', JSON.stringify(list.version));
    const ids = (list.accounts ?? []).map((a) => a.id);
    check('both configured accounts are listed, sorted', JSON.stringify(ids) === JSON.stringify(['default', 'paper']), ids.join(','));
    check('the reported default is the process-level one', list.active === 'default', list.active);
    const byId = Object.fromEntries((list.accounts ?? []).map((a) => [a.id, a]));
    check('each account starts on its OWN seed (§9.3 separate wallets)',
      byId.default?.balance === SEED && byId.paper?.balance === SEED,
      `default ${byId.default?.balance}, paper ${byId.paper?.balance}`);
    check('postures read active at boot', byId.default?.status === 'active' && byId.paper?.status === 'active');

    // 2. An order with no accountId spends the ACTIVE book, and the position
    //    it opens carries that accountId.
    await core.request('books.snapshot', { tokenId: 'tok', bids: [], asks: [{ price: 0.41, size: 500 }] });
    const placed = await rpc.placeOrder(core, order('buy', 'tok', 0.41, 2, 'e-iso'));
    check('the entry order lands', !!placed.orderId, JSON.stringify(placed).slice(0, 120));
    const positions = (await rpc.positions(core)).positions ?? [];
    check('the position is attributed to the account that placed it',
      positions.length === 1 && positions[0].accountId === 'default',
      JSON.stringify(positions.map((p) => p.accountId)));

    // 6. Money facts stay per-account: default paid, paper did not move.
    const listAfter = await rpc.accountList(core);
    const after = Object.fromEntries((listAfter.accounts ?? []).map((a) => [a.id, a]));
    check("default's book paid for the entry", after.default?.balance < SEED, `balance ${after.default?.balance}`);
    check("paper's book is untouched by default's entry", after.paper?.balance === SEED, `balance ${after.paper?.balance}`);

    // 3. G5 reverse A: a close naming ANOTHER account is refused.
    let crossErr = null;
    try {
      await rpc.placeOrder(core, order('sell', 'tok', 0.90, 2, 'exit:tok:WrongBook', 'paper'));
    } catch (e) {
      crossErr = e;
    }
    const crossReply = crossErr
      ? { error: { message: crossErr.message } }
      : { result: { orderId: 'accepted' } };
    const crossProblems = judgeCrossClose(crossReply);
    check('a cross-account close is REFUSED, never re-routed', crossProblems.length === 0,
      crossProblems.join(' | ').slice(0, 200));
    check('the position survived the refused close', ((await rpc.positions(core)).positions ?? []).length === 1);

    // 4. Reverse C: an unconfigured account is refused, never given a book.
    let ghostErr = null;
    try {
      await rpc.placeOrder(core, order('buy', 'tok2', 0.41, 2, 'e-ghost', 'ghost'));
    } catch (e) {
      ghostErr = e;
    }
    check('an order for an unconfigured account is refused',
      ghostErr !== null && String(ghostErr.message).includes('unknown account'),
      ghostErr ? String(ghostErr.message).slice(0, 120) : 'the order was ACCEPTED');
    const listGhost = await rpc.accountList(core);
    const ghostProblems = judgeUnknownAccount(
      ghostErr ? { error: { message: ghostErr.message } } : { result: {} },
      listGhost,
    );
    check('no implicit book was created for it', ghostProblems.length === 0,
      ghostProblems.join(' | ').slice(0, 200));

    // 5. Gate 2 at the wire: a frozen account refuses new entries.
    await core.request('account.status', { accountId: 'paper', status: 'frozen' });
    const listFrozen = await rpc.accountList(core);
    const frozen = (listFrozen.accounts ?? []).find((a) => a.id === 'paper');
    check('the freeze is visible in the view as a plain string', frozen?.status === 'frozen', frozen?.status);
    let limitErr = null;
    try {
      await core.request('books.snapshot', { tokenId: 'tok2', bids: [], asks: [{ price: 0.41, size: 500 }] });
      await rpc.placeOrder(core, order('buy', 'tok2', 0.41, 2, 'e-frozen', 'paper'));
    } catch (e) {
      limitErr = e;
    }
    check('a frozen account refuses new entries with ACCOUNT_LIMIT',
      limitErr !== null && limitErr.coreCode === 'ACCOUNT_LIMIT',
      limitErr ? `${limitErr.coreCode}: ${String(limitErr.message).slice(0, 100)}` : 'the order was ACCEPTED');

    // The RIGHT account closes: proceeds land in the book that paid.
    await core.request('books.snapshot', { tokenId: 'tok', bids: [{ price: 0.90, size: 500 }], asks: [] });
    const close = await rpc.placeOrder(core, order('sell', 'tok', 0.90, 2, 'exit:tok:RightBook'));
    check("the position's own account may close it", !!close.orderId, JSON.stringify(close).slice(0, 120));
    const positionsEnd = (await rpc.positions(core)).positions ?? [];
    check('the position is gone after the right-account close', positionsEnd.length === 0,
      `${positionsEnd.length} left`);
    const listEnd = await rpc.accountList(core);
    const end = Object.fromEntries((listEnd.accounts ?? []).map((a) => [a.id, a]));
    check("the profit landed in default's book", end.default?.balance > SEED, `balance ${end.default?.balance}`);
    check("paper's book STILL never moved (every probe ran against it)", end.paper?.balance === SEED,
      `balance ${end.paper?.balance}`);
  } catch (e) {
    check('harness error', false, e?.stack || String(e));
  } finally {
    await core.stop();
    try { rmSync(dirOf(core), { recursive: true, force: true }); } catch { /* best effort */ }
  }

  const failed = gate.failures;
  console.log(failed === 0
    ? '\naccount:isolation — cross-account money facts hold on both books; nothing implicit, nothing re-routed.'
    : `\naccount:isolation — ${failed} assertion(s) failed.`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`account-cross-isolation-check interrupted: ${String(e.message || e).slice(0, 500)}`);
  process.exit(1);
});
