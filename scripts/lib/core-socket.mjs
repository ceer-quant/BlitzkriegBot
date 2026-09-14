/**
 * Core UDS path resolution for scripts (mirror of `src/core/core-socket.ts`).
 *
 * The socket name is a four-language contract: the Rust core, `blitzkrieg_ui_kit`,
 * `blitzkrieg_ui_panel`, and the Node shell must all derive the same path. A
 * mismatch means a "helpful" client spawns a second core beside a healthy one.
 *
 * `blitzkrieg-core-<user>.sock` is canonical; `clodds-core-<user>.sock` is the
 * pre-rename name, still discovered for one release so an old core gets adopted
 * instead of duplicated.
 */
import net from 'net';
import { tmpdir } from 'os';
import { join } from 'path';

export const SOCKET_PREFIX = 'blitzkrieg-core';
export const LEGACY_SOCKET_PREFIX = 'clodds-core';

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

/** Pre-rename path, still probed during the migration window. */
export function legacySocketPath(env = process.env) {
  return socketPathFor(LEGACY_SOCKET_PREFIX, env);
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

/** Canonical unless a pre-rename core still owns the legacy name. */
export async function resolveSocketPath(env = process.env) {
  const canonical = defaultSocketPath(env);
  if (await socketServed(canonical)) return canonical;
  const legacy = legacySocketPath(env);
  if (legacy !== canonical && (await socketServed(legacy))) return legacy;
  return canonical;
}

/** A private per-process socket for isolated harnesses. */
export function scratchSocketPath(label, env = process.env) {
  return join(tmpdir(), `blitzkrieg-${label}-${process.pid}.sock`);
}
