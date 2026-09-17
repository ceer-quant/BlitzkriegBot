/**
 * E12 shutdown contract — `stop()` must mean "the core is gone".
 *
 * The bug this pins down: `stop()` fired SIGTERM and returned without waiting.
 * A caller could therefore start a replacement core, delete scratch state, or
 * assume resting orders were settled while the old process was demonstrably
 * still running (verified: `exitCode === null` at the moment `stop()` resolved).
 *
 * These tests spawn the REAL release binary, so they are the only coverage that
 * exercises the actual process boundary. They skip when the binary is absent
 * (`cargo build --release -p blitzkrieg-core`) rather than failing, so a
 * source-only checkout stays green.
 */
import { describe, it, before } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, unlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { BlitzkriegCoreClient } from '../../src/core/blitzkrieg-core-client.js';

const BIN = join(process.cwd(), 'target', 'release', 'blitzkrieg-core');
const haveBinary = existsSync(BIN);
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Is `pid` still a live, unreaped process? */
function alive(pid: number | undefined): boolean {
  if (!pid) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

describe('core shutdown contract (E12)', { skip: !haveBinary && 'release binary not built' }, () => {
  let work: string;

  before(() => {
    work = mkdtempSync(join(tmpdir(), 'bzk-shutdown-'));
  });

  it('resolves only after the process is actually gone', async () => {
    const sock = join(work, 'a.sock');
    try { unlinkSync(sock); } catch { /* fresh dir */ }

    const client = new BlitzkriegCoreClient({
      binaryPath: BIN,
      socketPath: sock,
      mode: 'dry',
      autoRestart: false,
      cwd: work,
      noTradeLog: true,
      noOrderLog: true,
      noPositionLog: true,
      extraArgs: ['--no-discovery', '--no-auto-exits'],
    });

    await client.start();
    const pid = (client as unknown as { proc: { pid?: number } }).proc?.pid;
    assert.ok(pid, 'client should own a spawned core');
    assert.equal(alive(pid), true, 'core should be running after start()');

    await client.stop();

    // The whole point: no window where the caller believes it is done but the
    // process is still there holding the socket.
    assert.equal(alive(pid), false, `core (pid ${pid}) must be reaped when stop() resolves`);

    // And the socket must be released so a replacement can bind it.
    assert.equal(existsSync(sock), false, 'core must unlink its socket before exit');
  });

  it('a replacement core can take over the same socket immediately', async () => {
    const sock = join(work, 'b.sock');
    try { unlinkSync(sock); } catch { /* fresh dir */ }

    const make = () =>
      new BlitzkriegCoreClient({
        binaryPath: BIN,
        socketPath: sock,
        mode: 'dry',
        autoRestart: false,
        cwd: work,
        noTradeLog: true,
        noOrderLog: true,
        noPositionLog: true,
        extraArgs: ['--no-discovery', '--no-auto-exits'],
      });

    const first = make();
    await first.start();
    const firstPid = (first as unknown as { proc: { pid?: number } }).proc?.pid;
    await first.stop();

    // Before the fix this raced: the old core still held the socket, so the new
    // one died with "already listening".
    const second = make();
    await second.start();
    const secondPid = (second as unknown as { proc: { pid?: number } }).proc?.pid;
    assert.ok(secondPid && secondPid !== firstPid, 'a distinct core should be running');
    const pong = await second.ping();
    assert.equal(pong.pong, true, 'the replacement core must be reachable');
    await second.stop();
    assert.equal(alive(secondPid), false, 'replacement core must also be reaped');
  });

  it('escalates to SIGKILL when the core ignores SIGTERM', async () => {
    // A core that traps SIGTERM stands in for one wedged inside a tick. The
    // client cannot distinguish the two, so it must escalate either way.
    const sock = join(work, 'stubborn.sock');
    try { unlinkSync(sock); } catch { /* fresh dir */ }
    const flag = join(work, 'stubborn.pid');

    const stub = spawn(
      '/bin/sh',
      ['-c', `trap 'echo trapped' TERM; echo $$ > ${JSON.stringify(flag)}; while :; do sleep 0.2; done`],
      { stdio: 'ignore' },
    );
    // Give the shell a moment to install the trap.
    for (let i = 0; i < 40 && !existsSync(flag); i++) await sleep(50);
    assert.ok(stub.pid, 'stub should have a pid');

    const client = new BlitzkriegCoreClient({
      binaryPath: BIN,
      socketPath: sock,
      mode: 'dry',
      autoRestart: false,
      cwd: work,
      noTradeLog: true,
      noOrderLog: true,
      noPositionLog: true,
      extraArgs: ['--no-discovery', '--no-auto-exits'],
    });

    // Drive the private escalation path directly: the stub is not a real core, so
    // we cannot go through start().
    const terminate = (
      client.constructor as unknown as {
        terminate: (p: unknown, g?: number, k?: number) => Promise<void>;
      }
    ).terminate.bind(client.constructor);

    assert.equal(alive(stub.pid), true, 'stub alive before terminate');
    await terminate(stub, 600, 1500);
    assert.equal(alive(stub.pid), false, 'a SIGTERM-ignoring process must be SIGKILLed');

    try { stub.kill('SIGKILL'); } catch { /* already gone */ }
  });

  it('leaves an adopted core running', async () => {
    // Ownership is the safety interlock: a core started by someone else must not
    // be killed just because we stopped watching it.
    const sock = join(work, 'c.sock');
    try { unlinkSync(sock); } catch { /* fresh dir */ }

    const owner = new BlitzkriegCoreClient({
      binaryPath: BIN,
      socketPath: sock,
      mode: 'dry',
      autoRestart: false,
      cwd: work,
      noTradeLog: true,
      noOrderLog: true,
      noPositionLog: true,
      extraArgs: ['--no-discovery', '--no-auto-exits'],
    });
    await owner.start();
    const pid = (owner as unknown as { proc: { pid?: number } }).proc?.pid;
    assert.ok(pid, 'owner core pid');

    // A second client pointing at the same socket adopts rather than spawns.
    const adopter = new BlitzkriegCoreClient({
      binaryPath: BIN,
      socketPath: sock,
      mode: 'dry',
      autoRestart: false,
      cwd: work,
      noTradeLog: true,
      noOrderLog: true,
      noPositionLog: true,
      extraArgs: ['--no-discovery', '--no-auto-exits'],
    });
    await adopter.start();
    await adopter.stop();

    assert.equal(alive(pid), true, 'an adopted core must be left running');

    await owner.stop();
    assert.equal(alive(pid), false, 'the owning client still reaps it');
  });
});
