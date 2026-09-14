/**
 * Core UDS path resolution.
 *
 * The socket name is a four-language contract — the Rust core (`default_socket()`),
 * `blitzkrieg_ui_kit` (`default_socket_path()`), `blitzkrieg_ui_panel`, and this
 * Node shell must all derive the same path, or a client "helpfully" spawns a
 * second core beside a healthy one. Two cores on one cwd interleave their
 * order/position logs, and the archive's single-writer lock makes one of them
 * stop recording silently.
 *
 * `blitzkrieg-core-<user>.sock` is canonical. `clodds-core-<user>.sock` is the
 * pre-rename name, kept discoverable for one release: a client that starts while
 * an old-branded core is still running adopts that core instead of spawning a
 * rival. See `docs/blitzkrieg/MIGRATION_LOG.md` §38.
 */
import { tmpdir } from 'os';
import { join } from 'path';
import net from 'net';

export const SOCKET_PREFIX = 'blitzkrieg-core';
export const LEGACY_SOCKET_PREFIX = 'clodds-core';

/** Path for an explicit prefix. `USER` fallback must match the Rust side. */
export function socketPathFor(prefix: string, env: NodeJS.ProcessEnv = process.env): string {
  const user = env.USER || 'user';
  const dir = env.TMPDIR && env.TMPDIR.length > 0 ? env.TMPDIR : tmpdir();
  return join(dir.replace(/\/$/, ''), `${prefix}-${user}.sock`);
}

/** Canonical path. No probing — this is what a fresh deployment binds. */
export function defaultSocketPath(env: NodeJS.ProcessEnv = process.env): string {
  return socketPathFor(SOCKET_PREFIX, env);
}

/** Pre-rename path, still probed during the migration window. */
export function legacySocketPath(env: NodeJS.ProcessEnv = process.env): string {
  return socketPathFor(LEGACY_SOCKET_PREFIX, env);
}

/** Is something accepting connections on this path right now? */
export function socketServed(path: string, timeoutMs = 500): Promise<boolean> {
  return new Promise((resolve) => {
    const sock = net.connect(path);
    let settled = false;
    const fin = (v: boolean) => {
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
 * The socket this client should use. Canonical unless a live core still owns the
 * legacy name — in that case returning the legacy path lets the normal adopt path
 * take over, which is the whole point of the compatibility window.
 */
export async function resolveSocketPath(
  env: NodeJS.ProcessEnv = process.env
): Promise<string> {
  const canonical = defaultSocketPath(env);
  if (await socketServed(canonical)) return canonical;
  const legacy = legacySocketPath(env);
  if (legacy !== canonical && (await socketServed(legacy))) return legacy;
  return canonical;
}
