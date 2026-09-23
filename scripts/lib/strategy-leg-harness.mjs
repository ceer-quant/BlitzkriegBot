/**
 * The harness behind the two strategy-leg acceptance gates —
 * `trend-follow-check.mjs` (E4-a / #30) and `mean-reversion-check.mjs`
 * (E4-b / #31).
 *
 * Both gates make the same claims about a different leg: it starts off, owns its
 * accounting row, prices its entry the way its family does, starves nothing, is
 * gated (or waived) by the shared gates, and is evolvable. Only the fixture and
 * the assertions differ, so the rest — spawn a dry-mode core on a private
 * socket, speak the UDS JSON-RPC wire, feed books, report — lives here once.
 *
 * It lives here because the two copies had already drifted: one had the
 * round-window flags in its base args and the other in a single session, one
 * carried the history of a gate that stayed green on a stale cdylib and the
 * other did not. A fix to the harness must not have to be made twice.
 *
 * Nothing here touches production: the core is spawned with every log and
 * archive switch off, in a scratch cwd, on a socket under the system temp dir.
 */

import { spawn } from './child-guard.mjs';
import net from 'net';
import { join } from 'path';
import { tmpdir } from 'os';
import { existsSync, unlinkSync, mkdtempSync } from 'fs';

export const ROUND_SEC = 3600;
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** The release core these gates drive (see `requireCoreBinary`). */
export const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');

/** A missing binary is a setup error, not a gate failure: say so and stop. */
export function requireCoreBinary() {
  if (!existsSync(BIN)) {
    console.error(`missing binary: ${BIN} (cargo build --release --workspace --locked)`);
    process.exit(2);
  }
}

/**
 * Build this gate's `session(tag, extraArgs, body)`.
 *
 * `tagPrefix` names the socket and the scratch dir (`blitzkrieg-e4a-…`), so a
 * leaked process is attributable to the gate that leaked it. `baseArgs` is the
 * flag set EVERY session of this gate needs and the sibling gate does not (the
 * confirmation window, the position cap); `extraArgs` is the per-session one
 * under test (e.g. --enable-strategy).
 */
export function makeSession({ tagPrefix, baseArgs = [] }) {
  /**
   * Spawn a core, hand the connected RPC client to `body`, always tear down.
   */
  return async function session(tag, extraArgs, body) {
    const sock = join(tmpdir(), `${tagPrefix}-${tag}-${process.pid}.sock`);
    const workdir = mkdtempSync(join(tmpdir(), `${tagPrefix}-${tag}-`));
    try { unlinkSync(sock); } catch {}

    const args = [
      '--socket', sock, '--mode', 'dry', '--tick-ms', '50',
      '--seed-balance', '1000', '--max-order-notional', '50',
      '--engine', '--no-discovery', '--no-event-archive', '--no-trade-log',
      // Scratch dir only: never restore, or leave behind, a real position/order.
      '--no-order-log', '--no-position-log',
      '--round-sec', String(ROUND_SEC),
      // Round-window gates off. The core derives time_left from the WALL clock
      // (the declared expiresAtMs is not plumbed to the scanner), so with a
      // 3600s round this check fails for the three minutes before every hour:
      // entries are refused with "Too close to expiry" and the assertions
      // collapse into "placed NO entry". Pinning the round boundary is not what
      // these gates are about — the entry, its pricing and the starve
      // attribution are — so open the window the way every other engine gate does.
      '--min-round-age', '0', '--min-time-left', '0',
      ...baseArgs,
      ...extraArgs,
    ];
    const proc = spawn(BIN, args, { stdio: ['ignore', 'ignore', 'pipe'], cwd: workdir });
    let stderr = '';
    proc.stderr.on('data', (d) => { stderr += d.toString(); });

    let sockc = null, buf = '', seq = 0;
    const pending = new Map();
    const rpc = (method, params = {}) => new Promise((res, rej) => {
      const id = ++seq; pending.set(id, { resolve: res, reject: rej });
      sockc.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
    const connect = () => new Promise((res, rej) => {
      sockc = net.connect(sock, () => res());
      sockc.on('error', rej);
      sockc.on('data', (d) => {
        buf += d.toString(); let i;
        while ((i = buf.indexOf('\n')) >= 0) {
          const line = buf.slice(0, i); buf = buf.slice(i + 1);
          if (!line.trim()) continue;
          let msg; try { msg = JSON.parse(line); } catch { continue; }
          if (msg.id != null && pending.has(msg.id)) {
            const p = pending.get(msg.id); pending.delete(msg.id);
            msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result);
          }
        }
      });
    });

    try {
      for (let i = 0; i < 120; i++) { if (existsSync(sock)) break; await sleep(50); }
      await connect();
      await rpc('core.ready');
      return await body({ rpc, stderr: () => stderr });
    } finally {
      proc.kill('SIGKILL');
      try { unlinkSync(sock); } catch {}
    }
  };
}

export const rowFor = (stats, name) => (stats.strategies || []).find((s) => s.name === name);
export const isOn = (list, name) => (list.strategies || []).find((s) => s.name === name)?.enabled;

/**
 * Declare a round's markets in one shot. `specs` is one entry per asset:
 * `{ asset, upToken, downToken, upBid, upAsk, downBid, downAsk }`. A side is
 * left untraded when its bid is null. Feed every asset in the SAME call: the
 * handler replaces the whole market list, so two calls would drop the first.
 */
export async function setMarkets(rpc, now, specs) {
  const slot = Math.floor(now / 1000 / ROUND_SEC);
  await rpc('engine.markets', {
    markets: specs.map((s) => ({
      asset: s.asset, conditionId: `0xc-${s.asset}`, questionId: `0xq-${s.asset}`,
      upTokenId: s.upToken, downTokenId: s.downToken, upPrice: 0.5, downPrice: 0.5,
      expiresAtMs: (slot + 1) * ROUND_SEC * 1000, roundSlot: slot,
      negRisk: true, question: `${s.asset} up/down`,
    })),
  });
  for (const s of specs) {
    if (s.upBid != null) {
      await rpc('books.snapshot', { tokenId: s.upToken, bids: [{ price: s.upBid, size: 100 }], asks: [{ price: s.upAsk, size: 100 }] });
    }
    if (s.downBid != null) {
      await rpc('books.snapshot', { tokenId: s.downToken, bids: [{ price: s.downBid, size: 100 }], asks: [{ price: s.downAsk, size: 100 }] });
    }
  }
}

/** One market, BTC, tokens UP/DOWN — the shape most sections only need. */
export async function setMarket(rpc, { now, upBid, upAsk, downBid, downAsk }) {
  await setMarkets(rpc, now, [
    { asset: 'BTC', upToken: 'UP', downToken: 'DOWN', upBid, upAsk, downBid, downAsk },
  ]);
}

/**
 * Hold a book steady for `ms`. The dip buyer's candidate list only contains
 * trend-CONFIRMED tokens, and confirmation is a rolling window: it needs samples
 * spanning >=90% of `confirm_sec` with the mid held above `min_price`. A single
 * dip snapshot with no history behind it confirms nothing, so the side that is
 * meant to dip has to be held above the threshold for the whole window first.
 */
export async function feedHold(rpc, tokenId, bid, ask, ms, stepMs = 40) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    await rpc('books.snapshot', { tokenId, bids: [{ price: bid, size: 100 }], asks: [{ price: ask, size: 100 }] });
    await sleep(stepMs);
  }
}

/** Wait until `pick(rpc)` returns something truthy, or give up. */
export async function settle(rpc, pick, tries = 120) {
  for (let i = 0; i < tries; i++) {
    await sleep(50);
    const v = await pick(rpc);
    if (v) return v;
  }
  return null;
}

/**
 * Print the verdict and exit. `problems` is the gate's own accumulator (its
 * scenario bodies push into it); `okLines` are the claims that hold when it is
 * empty. The caller prints the title before running, so the ordering of the
 * gate's output is unchanged.
 */
export function report({ name, problems, okLines }) {
  if (problems.length) {
    for (const p of problems) console.log(`  FAIL ${p}`);
    console.log(`\n${name}: ${problems.length} problem(s)`);
    process.exit(1);
  }
  for (const line of okLines) console.log(`  ok   ${line}`);
  console.log(`\n${name}: pass`);
}
