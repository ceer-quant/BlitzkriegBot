#!/usr/bin/env node
/**
 * data-backup gate — the safety properties of `scripts/data-backup.sh`.
 *
 * Kl-24's lesson was that the *refusal* branches were never tested: the
 * destructive path was exercised before the guard was verified. This gate
 * therefore leads with refusals and only then checks that a backup works.
 *
 * Everything runs in a throwaway fixture tree; the real `data/` is never read
 * or written.
 *
 *   1. Refuses a missing --dest, a nonexistent one, and a symlinked one.
 *   2. Refuses --dest inside the repo (the exact 2026-09-17 incident shape).
 *   3. Refuses --dest equal to the source tree, and a source inside --dest.
 *   4. --dry-run writes nothing.
 *   5. A real backup produces manifest + archive + BACKUP.json.
 *   6. The archive hash in the manifest matches the archive on disk.
 *   7. `--verify` passes on a good backup and FAILS on a tampered one.
 *   8. Pruning removes only our own strict-named dirs; a stranger's directory
 *      sitting in dest survives, as does a symlink of a valid backup name.
 *
 * Run: node scripts/data-backup-check.mjs
 */
import { execFileSync } from 'node:child_process';
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  existsSync,
  rmSync,
  symlinkSync,
  readdirSync,
} from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

const ROOT = process.cwd();
const SCRIPT = join(ROOT, 'scripts', 'data-backup.sh');
const LF = String.fromCharCode(10);

const WORK = mkdtempSync(join(tmpdir(), 'data-backup-check-'));
const SRC = join(WORK, 'data');
const DEST = join(WORK, 'dest');

let failures = 0;
function ok(msg) {
  console.log(`  ok   ${msg}`);
}
function bad(msg) {
  console.error(`  FAIL ${msg}`);
  failures++;
}
function assert(cond, msg) {
  cond ? ok(msg) : bad(msg);
}

/** Run the script, capturing exit code + output instead of throwing. */
function run(args) {
  try {
    const out = execFileSync('bash', [SCRIPT, ...args], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return { code: 0, out };
  } catch (e) {
    return { code: e.status ?? 1, out: `${e.stdout ?? ''}${e.stderr ?? ''}` };
  }
}

function seed() {
  rmSync(SRC, { recursive: true, force: true });
  mkdirSync(join(SRC, 'archive'), { recursive: true });
  mkdirSync(join(SRC, 'trades'), { recursive: true });
  mkdirSync(join(SRC, 'orders'), { recursive: true });
  writeFileSync(join(SRC, 'trades', 'trades.jsonl'), '{"a":1}' + LF);
  writeFileSync(join(SRC, 'orders', 'orders.jsonl'), '{"b":2}' + LF);
  writeFileSync(join(SRC, 'archive', 'events.1.jsonl'), 'evt' + LF);
  writeFileSync(join(SRC, 'archive', 'events.jsonl'), 'evt2' + LF);
}

function backups() {
  return readdirSync(DEST).filter((n) => n.startsWith('blitzkrieg-data-')).sort();
}

console.log('─'.repeat(72));
console.log('data-backup gate (safety properties of scripts/data-backup.sh)');
console.log('─'.repeat(72));

if (!existsSync(SCRIPT)) {
  console.error(`FAIL: ${SCRIPT} not found`);
  process.exit(1);
}

// ── 0. fixture ──────────────────────────────────────────────────────────────
mkdirSync(DEST, { recursive: true });
seed();
console.log('');
console.log('[0] fixture');
assert(existsSync(join(SRC, 'archive', 'events.1.jsonl')), 'fixture source tree has files');

// ── 1. --dest validation ────────────────────────────────────────────────────
console.log('');
console.log('[1] destination is validated before anything is written');

let r = run(['--data', SRC]);
assert(r.code === 2 && /--dest is required/.test(r.out), 'refuses a missing --dest (exit 2)');

r = run(['--data', SRC, '--dest', join(WORK, 'nope-does-not-exist')]);
assert(r.code === 2 && /not an existing directory/.test(r.out), 'refuses a nonexistent --dest');

const LINK = join(WORK, 'link-dest');
symlinkSync(DEST, LINK);
r = run(['--data', SRC, '--dest', LINK]);
assert(r.code === 2 && /symlink/.test(r.out), 'refuses a symlinked --dest');

r = run(['--data', SRC, '--dest', ROOT]);
assert(r.code === 2 && /repository root/.test(r.out), 'refuses the repository root');

// `scripts/` is used rather than `target/`: the gate must not depend on the
// build having run, and a missing directory would fail with the wrong message
// ("not an existing directory") and mask what is actually being asserted.
r = run(['--data', SRC, '--dest', join(ROOT, 'scripts')]);
assert(
  r.code === 2 && /inside the repository/.test(r.out),
  'refuses a --dest inside the repo (the 2026-09-17 incident shape)'
);

r = run(['--data', SRC, '--dest', SRC]);
assert(r.code === 2 && /data directory itself/.test(r.out), 'refuses --dest == source tree');

r = run(['--data', join(DEST, 'inner-data'), '--dest', DEST]);
assert(
  r.code === 2 && /inside --dest|source tree/.test(r.out),
  'refuses a source tree nested inside --dest (recursive copy)'
);

// ── 2. dry run ──────────────────────────────────────────────────────────────
console.log('');
console.log('[2] --dry-run writes nothing');
r = run(['--data', SRC, '--dest', DEST, '--dry-run']);
assert(r.code === 0 && /DRY RUN/.test(r.out), '--dry-run completes');
assert(backups().length === 0, '--dry-run created no backup directory');
assert(r.out.includes('files  :'), '--dry-run reports the file count it would copy');

// ── 3. a real backup ────────────────────────────────────────────────────────
console.log('');
console.log('[3] a real backup produces a verifiable artifact set');
r = run(['--data', SRC, '--dest', DEST]);
assert(r.code === 0 && /BACKUP OK/.test(r.out), 'backup succeeds');

const made = backups();
assert(made.length === 1, `exactly one backup directory created (got ${made.length})`);
const BK = join(DEST, made[0]);

for (const f of ['MANIFEST.sha256', 'data.tar.gz', 'BACKUP.json']) {
  assert(existsSync(join(BK, f)), `backup contains ${f}`);
}

const manifest = readFileSync(join(BK, 'MANIFEST.sha256'), 'utf8');
const recHash = (manifest.split(LF).find((l) => l.startsWith('# archive-sha256 ')) || '').split(' ')[2];
assert(/^[0-9a-f]{64}$/.test(recHash || ''), 'manifest records an archive sha256');

// Recompute independently so we are not trusting the script's own arithmetic.
const { createHash } = await import('node:crypto');
const actual = createHash('sha256').update(readFileSync(join(BK, 'data.tar.gz'))).digest('hex');
assert(actual === recHash, 'recorded archive hash matches an independent recomputation');

const meta = JSON.parse(readFileSync(join(BK, 'BACKUP.json'), 'utf8'));
assert(meta.fileCount === 4, `BACKUP.json records the file count (${meta.fileCount} == 4)`);
assert(meta.archiveSha256 === actual, 'BACKUP.json archive sha256 agrees with the manifest');
assert(typeof meta.repoHead === 'string' && meta.repoHead.length >= 7, 'BACKUP.json records the repo revision');

// The archive must actually contain the data, not just exist.
const listing = execFileSync('tar', ['-tzf', join(BK, 'data.tar.gz')], { encoding: 'utf8' });
assert(/archive\/events\.1\.jsonl/.test(listing), 'archive holds the nested archive/ segment');
assert(/trades\/trades\.jsonl/.test(listing), 'archive holds trades/trades.jsonl');

// ── 4. verify mode ──────────────────────────────────────────────────────────
console.log('');
console.log('[4] --verify passes on a good backup, fails on a tampered one');
r = run(['--verify', BK]);
assert(r.code === 0 && /backup verifies/.test(r.out), '--verify passes on the fresh backup');
assert(/all 4 file\(s\) match/.test(r.out), '--verify checked every manifest entry');

// Corrupt one byte of the archive: verification must notice.
const arcPath = join(BK, 'data.tar.gz');
const buf = readFileSync(arcPath);
buf[buf.length - 1] = buf[buf.length - 1] ^ 0xff;
writeFileSync(arcPath, buf);
r = run(['--verify', BK]);
assert(r.code === 1, '--verify FAILS on a tampered archive (exit 1)');
assert(/mismatch|does not extract|do not match/.test(r.out), '--verify explains the tamper');

// ── 5. pruning safety ───────────────────────────────────────────────────────
console.log('');
console.log('[5] pruning removes only our own strict-named backups');

// A stranger's directory and a symlink wearing a valid backup name must survive.
const STRANGER = join(DEST, 'someone-elses-folder');
mkdirSync(STRANGER, { recursive: true });
writeFileSync(join(STRANGER, 'keep-me.txt'), 'important' + LF);

const LINKED_FAKE = join(DEST, 'blitzkrieg-data-19990101T000000Z');
symlinkSync(STRANGER, LINKED_FAKE);

// Two more real backups, then keep only the newest.
run(['--data', SRC, '--dest', DEST, '--keep', '0']);
await new Promise((res) => setTimeout(res, 1100)); // distinct UTC second
run(['--data', SRC, '--dest', DEST, '--keep', '0']);
assert(backups().length >= 3, `multiple real backups exist to prune (${backups().length})`);

r = run(['--data', SRC, '--dest', DEST, '--keep', '1']);
assert(r.code === 0, 'a --keep 1 run succeeds');
const after = backups();
// The symlink shares the name prefix but is filtered by -type d / guards.
const realDirs = after.filter((n) => n !== 'blitzkrieg-data-19990101T000000Z');
assert(realDirs.length === 1, `pruning kept exactly one real backup (got ${realDirs.length})`);
assert(existsSync(STRANGER), "a stranger's directory survives pruning");
assert(existsSync(join(STRANGER, 'keep-me.txt')), "the stranger's file survives pruning");
assert(existsSync(LINKED_FAKE), 'a symlink wearing a backup name survives pruning');

// ── 6. exclude-archive ──────────────────────────────────────────────────────
console.log('');
console.log('[6] --exclude-archive drops the archive subtree');
// Identify the directory this run created by set difference, not by name order.
// Two runs inside one UTC second get a `-N` suffix, and a bare-name re-run sorts
// *before* an already-suffixed sibling — so "lexically last" can point at the
// previous backup and silently check the wrong tarball.
const before6 = new Set(backups());
r = run(['--data', SRC, '--dest', DEST, '--exclude-archive', '--keep', '0']);
assert(r.code === 0, 'backup with --exclude-archive succeeds');
const created = backups().filter((n) => !before6.has(n));
assert(created.length === 1, `--exclude-archive created exactly one backup (got ${created.length})`);
const light = created[0];
const meta6 = JSON.parse(readFileSync(join(DEST, light, 'BACKUP.json'), 'utf8'));
assert(meta6.excludedArchive === true, 'BACKUP.json records that the archive was excluded');
// The manifest must describe what the archive holds: an excluded subtree must
// not be listed among the captured files either.
const man6 = readFileSync(join(DEST, light, 'MANIFEST.sha256'), 'utf8');
assert(!/^[0-9a-f]+ {2}data\/archive\//m.test(man6), 'the manifest lists no excluded archive file');
assert(/^[0-9a-f]+ {2}data\/trades\/trades\.jsonl$/m.test(man6), 'the manifest lists non-excluded files');
const list2 = execFileSync('tar', ['-tzf', join(DEST, light, 'data.tar.gz')], { encoding: 'utf8' });
assert(!/archive\/events/.test(list2), 'excluded archive segment is absent from the tarball');
assert(/trades\/trades\.jsonl/.test(list2), 'non-excluded data is still present');
// A light backup must still verify: excluding bytes is a packaging choice, not
// permission to ship an archive that disagrees with its own manifest.
r = run(['--verify', join(DEST, light)]);
assert(r.code === 0, '--verify passes on an --exclude-archive backup');

// ── result ──────────────────────────────────────────────────────────────────
rmSync(WORK, { recursive: true, force: true });

console.log('─'.repeat(72));
if (failures > 0) {
  console.error(`RESULT: FAIL — ${failures} assertion(s) failed`);
  process.exit(1);
}
console.log('RESULT: PASS — data-backup refuses dangerous destinations, verifies its');
console.log('        output, and prunes only directories it created.');
console.log('─'.repeat(72));
