/**
 * The one "poll until it is true" primitive for the gates in `scripts/`.
 *
 * Thirty-nine call sites across twenty-three gates used to carry their own version
 * of this loop: six `waitFor`/`until` definitions in three different signatures
 * (`(fn, ms, label)`, `(label, fn, timeoutMs)`, `(fn, ms = 2000)`), six polling
 * cadences (20/25/50/100/200/250 ms), and fifteen socket-ready spins written as
 * `for (let i = 0; i < N && !existsSync(sock); i++) await sleep(50)` — where the
 * iteration count N was the only record of how long a core is allowed to take, and
 * it varied from 40 to 120 with nothing anywhere saying which value was meant.
 *
 * What mattered was not the repetition but the contract nobody chose on purpose:
 * `account-parity`'s `until` returned `false` on timeout while the other five
 * threw. A caller that ignores a `false` return carries on as if the wait had
 * succeeded — the shape of a false green. A caller cannot ignore a throw. So the
 * contract is in the name now:
 *
 *   waitFor(probe, opts)       the condition MUST happen. Throws on timeout, naming
 *                              the condition and the last value or error it saw.
 *   pollUntil(probe, opts)     poll, then decide for yourself. Returns `undefined`
 *                              on timeout and lets the caller's check() judge.
 *   waitForSocket(path, opts)  a core must be listening. waitFor() for a socket.
 *
 * A probe exception is a fact about the system, not a slow condition, so neither
 * wait swallows one unless asked (`retryOnError: true`) — the three gates that
 * already retried a refusing connection say so at the call site instead of hiding
 * it in a private helper.
 */
import { existsSync } from 'node:fs';

/** One cadence for every wait. A gate's budget is its timeout, not its poll rate. */
const POLL_MS = 50;

/**
 * The one sleep. Twenty-seven scripts carried their own copy of this one-liner
 * (in three spellings — `r`, `resolve`, and two `r =>` without parens); this is
 * that definition, exported so the copies can go.
 */
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Enough of whatever the probe last produced to name the failure. */
function describe(value) {
  if (value === undefined) return 'undefined';
  let text;
  try {
    text = typeof value === 'string' ? value : JSON.stringify(value) ?? String(value);
  } catch {
    text = String(value);
  }
  return text.length > 200 ? `${text.slice(0, 200)}…` : text;
}

async function poll(probe, { timeoutMs, retryOnError }) {
  const deadline = Date.now() + timeoutMs;
  let last;
  for (;;) {
    try {
      const value = await probe();
      if (value) return { ok: true, value };
      last = value;
    } catch (e) {
      if (!retryOnError) throw e;
      last = e?.message ?? e;
    }
    if (Date.now() >= deadline) return { ok: false, last };
    await sleep(POLL_MS);
  }
}

/**
 * Poll `probe` until it returns something truthy, and return that value.
 * Throws if the timeout passes first — use this when the gate cannot proceed
 * without the condition, so a timeout reads as the failure it is.
 */
export async function waitFor(probe, { timeoutMs = 5000, label = 'the condition', retryOnError = false } = {}) {
  const r = await poll(probe, { timeoutMs, retryOnError });
  if (r.ok) return r.value;
  throw new Error(`timed out waiting for ${label} (${timeoutMs} ms); last=${describe(r.last)}`);
}

/**
 * Poll `probe` until it returns something truthy and return that value, or return
 * `undefined` once the timeout passes. Use this when the caller already asserts the
 * same fact afterwards (or reports its own diagnostic) — the wait is then an
 * optimization, not the gate.
 */
export async function pollUntil(probe, { timeoutMs = 5000, retryOnError = false } = {}) {
  const r = await poll(probe, { timeoutMs, retryOnError });
  return r.ok ? r.value : undefined;
}

/**
 * Wait for a core to be listening on `socketPath`. The socket file appearing is the
 * earliest observable proof of a bind; the core may still be finishing its own boot,
 * which is what the fixed sleeps after these waits are for.
 */
export async function waitForSocket(socketPath, { timeoutMs = 5000 } = {}) {
  const seen = await pollUntil(() => existsSync(socketPath), { timeoutMs });
  if (!seen) throw new Error(`timed out waiting for a core socket at ${socketPath} (${timeoutMs} ms)`);
}
