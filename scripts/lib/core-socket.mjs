/**
 * Core UDS path resolution for scripts (mirror of `src/core/core-socket.ts`).
 *
 * The socket name is a four-language contract: the Rust core, `blitzkrieg_ui_kit`,
 * `blitzkrieg_ui_panel`, and the Node shell must all derive the same path. A
 * mismatch means a "helpful" client spawns a second core beside a healthy one.
 */
import net from 'net';
import { tmpdir } from 'os';
import { join } from 'path';

export const SOCKET_PREFIX = 'blitzkrieg-core';

/** Path for an explicit prefix. `USER` fallback must match the Rust side. */
export function socketPathFor(prefix, env = process.env) {
  const user = env.USER || 'user';
  const dir = env.TMPDIR && env.TMPDIR.length > 0 ? env.TMPDIR : tmpdir();
  return join(dir.replace(/\/$/, ''), `${prefix}-${user}.sock`);
}

/** Canonical path; what a fresh deployment binds. */
export function defaultSocketPath(env = process.env) {
  return socketPathFor(SOCKET_PREFIX, env);
}

/**
 * The socket a client should use. Mirrors `resolveSocketPath` in
 * `src/core/core-socket.ts` — the two must stay in step, because the scripts
 * that import it (soak-monitor, account-drift-check, feed-live-probe,
 * price-compare) all attach to a core that the Node shell may have spawned.
 *
 * The probe is deliberately not a fallback: it only decides whether the
 * canonical path is LIVE, so a monitor can report "no core answering" instead of
 * silently inventing a second path and watching an empty socket. The path
 * returned is always canonical, which is the whole point of the four-language
 * naming contract documented above.
 */
export async function resolveSocketPath(env = process.env) {
  const canonical = defaultSocketPath(env);
  if (await socketServed(canonical, 300)) return canonical;
  return canonical;
}

/** Is something accepting connections on this path right now? */
export function socketServed(path, timeoutMs = 500) {
  return new Promise((resolve) => {
    const sock = net.connect(path);
    let settled = false;
    const fin = (v) => {
      if (settled) return;
      settled = true;
      try { sock.destroy(); } catch { /* noop */ }
      resolve(v);
    };
    sock.on('connect', () => fin(true));
    sock.on('error', () => fin(false));
    setTimeout(() => fin(false), timeoutMs);
  });
}

/**
 * A private per-process socket for isolated harnesses.
 *
 * Deliberately NOT shaped like the canonical `<prefix>-<user>.sock`: a harness
 * must neither be adopted as the real core by `resolveSocketPath`, nor adopt a
 * running core it did not spawn. Keep `label` short — a UDS path is capped
 * around 104 bytes, and `TMPDIR` already spends most of that on macOS.
 */
export function scratchSocketPath(label, env = process.env) {
  const dir = env.TMPDIR && env.TMPDIR.length > 0 ? env.TMPDIR : tmpdir();
  return join(dir.replace(/\/$/, ''), `blitzkrieg-${label}-${process.pid}.sock`);
}

