#!/usr/bin/env node
// Gate for scripts/tui-demo.sh. Headless mode: without a TTY the panel exits
// immediately, which exercises the cleanup path — the check asserts that the
// launcher announced the core and that the socket is gone after exit.
// Guarded spawn: `tui-demo.sh` backgrounds a core, so an interrupted gate would
// otherwise orphan that core — the shell holding its pid and socket path is gone.
import { spawn } from './lib/child-guard.mjs';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createChecks } from './lib/gate-harness.mjs';

const gate = createChecks();
const { check } = gate;

mkdtempSync(join(tmpdir(), 'tui-demo-check-'));
const proc = spawn('bash', [join(process.cwd(), 'scripts/tui-demo.sh')], {
  cwd: process.cwd(), stdio: ['ignore', 'pipe', 'pipe'],
});
let out = '';
proc.stdout.on('data', (d) => (out += d));
proc.stderr.on('data', (d) => (out += d));
const code = await new Promise((r) => proc.on('exit', (c) => r(c)));
const sockLine = out.match(/dry core on (\S+)/);
check('launcher announces the dry core', !!sockLine, sockLine?.[1] ?? '');
check('panel exits headless without panic', code !== null && !out.includes('panic'), `exit=${code}`);
if (sockLine) check('socket cleaned up after exit', !existsSync(sockLine[1]), sockLine[1]);
const fails = gate.failures;
console.log(fails ? `${fails} FAIL` : 'all pass');
process.exit(fails ? 1 : 0);
