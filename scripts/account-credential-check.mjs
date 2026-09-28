#!/usr/bin/env node
/**
 * account-credential-check — DEV_V0_3 §16.4 E28 gate (§9.4 / §1833).
 *
 * The contract: credential NAMES may travel (which environment variables an
 * account's keys come from); credential VALUES may NEVER appear in any IPC
 * answer, event, or audit line. `account.list` answers `credentialsLoaded`
 * as a boolean — a presence fact — and nothing else (§813: 任何方法的响应
 * 都不得包含凭证值).
 *
 * How the gate makes that REAL and non-vacuous:
 *
 *   1. a scratch `accounts.toml` names four env vars for its account, and the
 *      gate spawns the core with those vars SET TO SENTINEL VALUES — so the
 *      kernel's own startup probe (`read_credential_keys`) actually reads
 *      them and `credentialsLoaded` comes back `true`. A grep over an
 *      environment where nothing was planted would pass vacuously; this one
 *      cannot.
 *   2. the gate then drives a FULL session over the wire — every read verb,
 *      a real dry order, the reconcile (venue-gap) path, the account verbs,
 *      DELIBERATE errors (unknown method, bad params) — and concatenates
 *      EVERY response AND EVERY pushed event into one corpus.
 *   3. the corpus is grepped for the sentinel. A hit is
 *      `sentinel leaked` — red.
 *
 * Usage:
 *   node scripts/account-credential-check.mjs          # the real sweep
 *   node scripts/account-credential-check.mjs --teeth  # must go red
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

// The planted values. Distinctive, greppable, never a real secret — and
// NEVER printed by this gate (a gate that echoes the sentinel is its own
// leak, and would also false-positive its own grep).
const SENTINEL = 'BK-E28-SENTINEL-DO-NOT-SERIALIZE';
const KEY_ENV = {
  api_key_env: 'BK_E28_TEST_API_KEY',
  secret_env: 'BK_E28_TEST_SECRET',
  passphrase_env: 'BK_E28_TEST_PASSPHRASE',
  private_key_env: 'BK_E28_TEST_PRIVATE_KEY',
};

const gate = createChecks();
const { check } = gate;

// ── pure judge — the teeth surface ───────────────────────────────────────────

/**
 * The §9.4 verdict for one wire corpus: the sentinel must not appear
 * ANYWHERE, and every `credentialsLoaded` on the wire must be a boolean —
 * never an object/value shape.
 */
export function judgeCorpus(corpus, { expectLoaded = true } = {}) {
  const problems = [];
  if (!corpus || corpus.length < 200) {
    return ['the corpus is empty — the sweep drove nothing, so the grep proves nothing'];
  }
  if (corpus.includes(SENTINEL)) {
    problems.push('sentinel leaked — a credential VALUE reached the wire');
  }
  for (const m of corpus.matchAll(/"credentialsLoaded":([^,}\]]+)/g)) {
    const v = m[1].trim();
    if (v !== 'true' && v !== 'false') {
      problems.push(`credentialsLoaded must be a boolean, got ${v}`);
    }
    if (expectLoaded && v !== 'true') {
      problems.push(`credentialsLoaded came back ${v} — the startup probe never ran, so this sweep is vacuous`);
      break;
    }
  }
  return problems;
}

function teeth() {
  // §16.6: feed the broken implementations' OUTPUT to the judge; each must go
  // red naming the §1833 anchor, else this gate has no teeth. Fixtures carry
  // enough real-shaped traffic to clear the judge's non-vacuity floor — the
  // floor exists so a REAL sweep that drove nothing fails, not so a fixture
  // can hide behind it.
  const goodList = {
    version: '1.1', active: 'default',
    accounts: [{ id: 'default', status: 'active', credentialsLoaded: true, balance: 1 }],
  };
  const traffic = JSON.stringify({ version: '1.1', trades: [], positions: [], summary: { wins: 0, losses: 0 } })
    + JSON.stringify({ version: '1.1', accounts: goodList.accounts })
    + JSON.stringify({ orderId: 'dry_1', status: 'FILLED' });
  const leaked = JSON.parse(JSON.stringify(goodList));
  leaked.accounts[0].apiKeyValue = SENTINEL;
  const mutations = [
    {
      name: 'teeth: an answer carries the credential VALUE',
      corpus: traffic + JSON.stringify(leaked) + traffic,
      mention: 'sentinel leaked',
    },
    {
      name: 'teeth: an event carries the credential VALUE',
      corpus: traffic + JSON.stringify(goodList) + `{"kind":"ACCOUNT_UPDATE","secretValue":"${SENTINEL}"}`,
      mention: 'sentinel leaked',
    },
    {
      name: 'teeth: credentialsLoaded degraded into a value shape',
      corpus: traffic + JSON.stringify({ ...goodList, accounts: [{ ...goodList.accounts[0], credentialsLoaded: { key: 'BK-READ-BY-KERNEL-ONLY' } }] }),
      mention: 'must be a boolean',
    },
    {
      name: 'teeth: the probe never ran (vacuous green)',
      corpus: traffic + JSON.stringify({ ...goodList, accounts: [{ ...goodList.accounts[0], credentialsLoaded: false }] }),
      mention: 'vacuous',
    },
  ];
  let missed = 0;
  for (const m of mutations) {
    const problems = judgeCorpus(m.corpus);
    const caught = problems.some((p) => p.includes(m.mention));
    console.log(`${caught ? '  ok  ' : '  FAIL'} teeth: ${m.name}`);
    if (caught) console.log(`       caught: ${problems.find((p) => p.includes(m.mention)).slice(0, 140)}`);
    else missed += 1;
  }
  if (missed === 0) {
    console.log('\naccount:credential --teeth: every leak shape was caught — the gate has teeth');
    process.exit(0);
  }
  console.error(`\naccount:credential --teeth: ${missed} mutation(s) slipped through green — the gate has no teeth`);
  process.exit(1);
}

// ── the real sweep ───────────────────────────────────────────────────────────

const BOOK_TOML = `\
# Scratch book for the credential gate (§9.4): the config names ENV VARS —
# the VALUES live only in the kernel process's own environment.
[[account]]
id = "default"
name = "default"
api_key_env = "${KEY_ENV.api_key_env}"
secret_env = "${KEY_ENV.secret_env}"
passphrase_env = "${KEY_ENV.passphrase_env}"
private_key_env = "${KEY_ENV.private_key_env}"
`;

function makeCore() {
  const cwd = mkdtempSync(join(tmpdir(), 'bk-e28-cred-'));
  mkdirSync(join(cwd, 'user_layer', 'configs'), { recursive: true });
  writeFileSync(join(cwd, 'user_layer', 'configs', 'accounts.toml'), BOOK_TOML);
  return new CoreClient({
    binaryPath: BIN,
    socketPath: scratchSocketPath('cred'),
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

async function main() {
  if (process.argv.includes('--teeth')) teeth();
  if (!existsSync(BIN)) {
    console.error(`missing binary: ${BIN} (cargo build --release --workspace)`);
    process.exit(2);
  }

  const core = makeCore();
  const events = [];
  core.onEvent = (params) => events.push(JSON.stringify(params));
  let cwd;
  try {
    checkCoreProvenance(BIN, check);
    // Plant the sentinels in THIS process's env: CoreClient spawns the child
    // with `env: process.env`, so the kernel's startup probe reads them.
    for (const name of Object.values(KEY_ENV)) process.env[name] = SENTINEL;
    await core.start();
    cwd = core.cwd;
    await rpc.ready(core);

    const replies = [];
    const call = async (label, p) => {
      try {
        const r = await core.request(p.method, p.params ?? {});
        replies.push(JSON.stringify(r));
        return r;
      } catch (e) {
        // Error replies are wire traffic too — sweep them.
        replies.push(JSON.stringify({ error: String(e.message ?? e) }));
        return null;
      }
    };

    // A FULL session: reads, the account verbs, a real dry order, the
    // venue-gap reconcile path, exits, and deliberate errors.
    await call('ready', { method: 'core.ready' });
    await call('version', { method: 'system.version' });
    const list = await call('account.list', { method: 'account.list', params: {} });
    await call('account.switch', { method: 'account.switch', params: { accountId: 'default' } });
    await call('account.status tighten', { method: 'account.status', params: { accountId: 'default', status: 'read_only' } });
    await call('account.status loosen (refused)', { method: 'account.status', params: { accountId: 'default', status: 'active' } });
    // Put the posture back so the order below is admittable.
    await call('cleanup posture', { method: 'account.list', params: {} });
    await call('balance', { method: 'ledger.balance' });
    await call('positions', { method: 'positions.list' });
    await call('trades', { method: 'trades.history', params: { limit: 10 } });
    await call('summary', { method: 'trades.summary' });
    await call('orders', { method: 'orders.list' });
    await call('strategies', { method: 'strategy.list' });
    await call('extensions', { method: 'extension.list' });
    await call('stats', { method: 'engine.stats' });
    await call('audit tail', { method: 'intent.audit.tail', params: { limit: 10 } });
    await call('unknown method (error)', { method: 'no.such.method' });
    await call('bad params (error)', { method: 'account.switch' }); // missing accountId

    // A real dry round trip through the account's own book.
    await call('book', { method: 'books.snapshot', params: { tokenId: 'tok', bids: [{ price: 0.40, size: 500 }], asks: [{ price: 0.41, size: 500 }] } });
    const slot = Math.floor(Date.now() / 1000 / 900);
    await call('place', {
      method: 'orders.place',
      params: { tokenId: 'tok', conditionId: 'cond', side: 'buy', mode: 'taker', price: 0.41, size: 5, internalKey: 'e-cred', strategy: 'cred', asset: 'BTC', direction: 'up', roundSlot: slot },
    });
    const orders = (await core.request('orders.list')).orders ?? [];
    const live = orders.find((o) => o.status?.isLive || o.status === 'live' || o.status === 'LIVE');
    if (live) {
      await call('reconcile', {
        method: 'orders.reconcile',
        params: { openOrderIds: [live.orderId ?? live.order_id], trades: [] },
      });
    }
    await call('exit', { method: 'positions.exit', params: { positionId: 'all' } });
    // Give the event stream a beat to drain.
    await new Promise((r) => setTimeout(r, 300));

    // §9.4 non-vacuity: the probe MUST have run (sentinels were loadable).
    const view = (list?.accounts ?? [])[0] ?? {};
    check('the startup probe loaded the planted credentials', view.credentialsLoaded === true,
      `credentialsLoaded=${JSON.stringify(view.credentialsLoaded)}`);

    const corpus = replies.join('\n') + '\n' + events.join('\n');
    check(`the sweep drove a full session (${replies.length} replies, ${events.length} events)`,
      replies.length >= 15, `${replies.length} replies captured`);
    const problems = judgeCorpus(corpus);
    check('no credential VALUE appears in any reply or event (§9.4)', problems.length === 0,
      problems.join(' | ').slice(0, 200));

    // The audit file on disk is not wire traffic, but it is an "answer" the
    // operator reads — sweep it too when the session wrote one.
    const auditPath = join(cwd, 'data', 'audit', 'intents.jsonl');
    if (existsSync(auditPath)) {
      const audit = (await import('fs')).readFileSync(auditPath, 'utf8');
      check('no credential VALUE appears in the audit log',
        !audit.includes(SENTINEL), `${audit.split('\n').filter((l) => l.trim()).length} audit line(s) swept`);
    }
  } catch (e) {
    check('harness error', false, e?.stack || String(e));
  } finally {
    // Scrub the sentinels from this process before anything else runs.
    for (const name of Object.values(KEY_ENV)) delete process.env[name];
    await core.stop();
    if (cwd) { try { rmSync(cwd, { recursive: true, force: true }); } catch { /* best effort */ } }
  }

  const failed = gate.failures;
  console.log(failed === 0
    ? '\naccount:credential — the values stayed in the kernel process; the wire carries presence, never secrets.'
    : `\naccount:credential — ${failed} assertion(s) failed.`);
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`account-credential-check interrupted: ${String(e.message || e).slice(0, 500)}`);
  process.exit(1);
});
