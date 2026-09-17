/**
 * The core socket name is a four-language contract. A mismatch makes a client
 * spawn a second core beside a healthy one — two cores on one cwd interleave
 * their order/position logs, and the archive's single-writer lock silently stops
 * one of them from recording.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { createServer, type Server } from 'node:net';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  SOCKET_PREFIX,
  socketPathFor,
  defaultSocketPath,
  socketServed,
} from '../../src/core/core-socket.js';

const env = { USER: 'tester', TMPDIR: '/var/tmp/probe' } as NodeJS.ProcessEnv;

describe('core socket naming', () => {
  it('canonical name carries the brand only', () => {
    const p = defaultSocketPath(env);
    assert.equal(p, '/var/tmp/probe/blitzkrieg-core-tester.sock');
    assert.ok(!p.includes('legacy-socket-name'), `path must not carry a legacy name: ${p}`);
  });

  it('falls back to the OS temp dir when TMPDIR is empty or missing', () => {
    for (const e of [{ USER: 'u' }, { USER: 'u', TMPDIR: '' }] as NodeJS.ProcessEnv[]) {
      assert.equal(
        defaultSocketPath(e),
        join(tmpdir(), 'blitzkrieg-core-u.sock')
      );
    }
  });

  it('trims a trailing slash so the path never doubles up', () => {
    assert.equal(
      socketPathFor(SOCKET_PREFIX, { USER: 'u', TMPDIR: '/var/tmp/probe/' } as NodeJS.ProcessEnv),
      '/var/tmp/probe/blitzkrieg-core-u.sock'
    );
  });

  it('USER fallback matches the Rust side', () => {
    // Rust uses "user"; a client and core that disagree here would never meet.
    assert.ok(defaultSocketPath({} as NodeJS.ProcessEnv).endsWith('blitzkrieg-core-user.sock'));
  });
});

describe('socketServed', () => {
  it('is false for a path nobody bound', async () => {
    assert.equal(await socketServed(join(tmpdir(), 'blitzkrieg-not-bound-4f1c.sock')), false);
  });

  it('is true while a server is listening there', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'bzk-sock-'));
    const sock = join(dir, 'live.sock');
    const server: Server = createServer();
    await new Promise<void>((res) => server.listen(sock, res));
    try {
      assert.equal(await socketServed(sock), true);
    } finally {
      await new Promise<void>((res) => server.close(() => res()));
    }
  });

  it('is false again once the server is gone', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'bzk-sock-'));
    const sock = join(dir, 'gone.sock');
    const server: Server = createServer();
    await new Promise<void>((res) => server.listen(sock, res));
    await new Promise<void>((res) => server.close(() => res()));
    assert.equal(await socketServed(sock), false);
  });
});
