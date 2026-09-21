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
 * Numbered to match the printed section labels:
 *   1. Refuses a missing --dest, a nonexistent one, and a symlinked one; refuses
 *      --dest inside the repo (the 2026-09-17 incident shape), --dest equal to
 *      the source tree, and a source inside --dest.
 *   2. --dry-run writes nothing.
 *   3. A real backup produces manifest + archive + BACKUP.json.
 *   4. `--verify` passes on a good backup and FAILS on a tampered one.
 *   5. Pruning removes only our own strict-named dirs; a stranger's directory
 *      sitting in dest survives, as does a symlink of a valid backup name.
 *   6. --exclude-archive drops the archive subtree.
 *   7. Live drift: the capture core appends to `data/archive/events.jsonl`
 *      continuously, so the source tree may be NEWER than any backup the
 *      moment it is finished. The manifest describes the archive, not the
 *      tree — an append after creation must not invalidate `--verify`
 *      (the exact failure of the first scheduled-era backup, 2026-09-20).
 *   8. `--status` (issue #217): empty/absent/stale/unreadable tiers and a failed
 *      scheduler each exit 1 with a token naming the cause, a fresh pair of
 *      artifacts exits 0, and a MANUAL run's record is never scheduler evidence.
 *   9. The attempt record format shared by the launchd launcher, the resident
 *      loop and the CLI (one writer, one format, best-effort by contract).
 *  10. The checkout guard accepts a linked worktree (`.git` is a FILE, #231).
 *  11. The resident loop (route B) really backs up, records `source=loop`, is
 *      read back as scheduler evidence by `--status`, and is loud on failure —
 *      the fallback route must not be broken too.
 *
 * Run: node scripts/data-backup-check.mjs
 */
import { execFileSync } from 'node:child_process';
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  appendFileSync,
  readFileSync,
  existsSync,
  rmSync,
  symlinkSync,
  readdirSync,
  chmodSync,
  utimesSync,
  copyFileSync,
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

/** Run the script, capturing exit code + output instead of throwing.
 *  `script` defaults to the real one; the guard section runs a copy inside a
 *  synthetic checkout, because the guard tests the tree the script LIVES in and
 *  that cannot be faked from the outside. */
function run(args, env = {}, script = SCRIPT) {
  try {
    const out = execFileSync('bash', [script, ...args], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, ...env },
    });
    return { code: 0, out };
  } catch (e) {
    return { code: e.status ?? 1, out: `${e.stdout ?? ''}${e.stderr ?? ''}` };
  }
}

/** Run a snippet of shell, for the sourced-library checks. */
function shell(code, env = {}) {
  try {
    const out = execFileSync('bash', ['-c', code], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, ...env },
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

// ── 7. live drift ───────────────────────────────────────────────────────────
console.log('');
console.log('[7] an append to the live capture stream does not invalidate verify');
// A backup is a point-in-time capture of a tree that keeps growing: the core
// appends to `data/archive/events.jsonl` continuously. The manifest describes
// the ARCHIVE (what was captured), not the live tree — so growing the tree
// after creation is normal and must still verify. This pins the exact failure
// the first scheduled-era backup hit on 2026-09-20 (manifest hashed from the
// live tree pre-tar, the core appended in between, verify failed on
// events.jsonl).
const before7 = new Set(backups());
appendFileSync(join(SRC, 'archive', 'events.jsonl'), 'grown-after-capture' + LF);
r = run(['--data', SRC, '--dest', DEST, '--keep', '0']);
assert(r.code === 0, 'backup succeeds while the live tree is mid-append');
const drift = backups().filter((n) => !before7.has(n));
assert(drift.length === 1, `the drift run created exactly one backup (got ${drift.length})`);
const live = drift[0];
const meta7 = JSON.parse(readFileSync(join(DEST, live, 'BACKUP.json'), 'utf8'));
assert(meta7.fileCount === 4, `manifest still describes all captured files (${meta7.fileCount})`);
r = run(['--verify', join(DEST, live)]);
assert(r.code === 0 && /backup verifies/.test(r.out), '--verify passes on the backup despite a newer live tree');
assert(/all 4 file\(s\) match/.test(r.out), 'every captured file was checked');

// ── 8. the freshness verdict (issue #217) ───────────────────────────────────
console.log('');
console.log('[8] --status tells "no fresh backup" from "healthy", and says which');
// The defect this section pins: on 2026-09-21 both LaunchAgents were installed,
// `launchctl list` reported exit code 0, and NOT ONE backup had ever been
// produced. A check that only looked at "is a schedule installed" would have
// called that healthy, so every assertion below drives a state where the schedule
// is present and the artifact is not (or is old).

/** A fresh fixture per sub-case: shared state between these cases is exactly how
 *  a gate starts asserting something other than what it claims. */
function statusFixture() {
  const w = mkdtempSync(join(tmpdir(), 'data-backup-status-'));
  const backups = join(w, 'backups');
  const logs = join(w, 'logs');
  const state = join(w, 'state');
  mkdirSync(join(backups, 'light'), { recursive: true });
  mkdirSync(join(backups, 'full'), { recursive: true });
  mkdirSync(logs, { recursive: true });
  mkdirSync(state, { recursive: true });
  return { w, backups, logs, state };
}
function status(f, extra = []) {
  return run(['--status', ...extra], {
    BK_BACKUP_DIR: f.backups,
    BK_BACKUP_LOG_DIR: f.logs,
    BK_BACKUP_STATUS_DIR: f.state,
  });
}
function makeArtifact(f, tier, ageHours = 0) {
  const dir = join(f.backups, tier, 'blitzkrieg-data-20260101T000000Z');
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'data.tar.gz'), 'x');
  const t = new Date(Date.now() - ageHours * 3600 * 1000);
  utimesSync(dir, t, t);
  return dir;
}

// (a) empty tier dirs → NONE, exit 1. This is the incident state: registered,
// installed, and never once successful.
let f = statusFixture();
let rs = status(f, ['--quiet']);
assert(rs.code === 1, 'empty tiers exit 1 (a schedule with no artifact is not healthy)');
assert(/light=NONE/.test(rs.out) && /full=NONE/.test(rs.out), `both tiers report NONE (${rs.out.trim()})`);
assert(rs.out.trim().split(LF).length === 1, '--quiet is exactly one line (the health check parses it)');
assert(/^backup: /.test(rs.out), 'the line starts with "backup: " (the token soak-health.sh extracts)');

// (b) a directory that is not there at all → ABSENT, and NOT the same word as
// "never produced": one is a missing volume, the other a scheduler that never ran.
f = statusFixture();
rs = run(['--status', '--quiet'], {
  BK_BACKUP_DIR: join(f.w, 'no-such-root'),
  BK_BACKUP_LOG_DIR: f.logs,
  BK_BACKUP_STATUS_DIR: f.state,
});
assert(rs.code === 1 && /light=ABSENT/.test(rs.out), 'a missing backup root reports ABSENT, exit 1');

// (c) fresh artifacts both tiers → exit 0. Without this the whole check could be
// "always red", which is the KI-30 failure mode: an alarm that cannot clear.
f = statusFixture();
makeArtifact(f, 'light');
makeArtifact(f, 'full');
rs = status(f, ['--quiet']);
assert(rs.code === 0 && /light=ok\(0h\)/.test(rs.out) && /full=ok\(0h\)/.test(rs.out), `fresh artifacts clear the check (${rs.out.trim()})`);

// (d) stale artifact: the same tier, backdated past the tolerance.
f = statusFixture();
makeArtifact(f, 'light', 3);
makeArtifact(f, 'full');
rs = status(f, ['--quiet', '--stale-hours', '1', '--full-stale-hours', '1']);
assert(rs.code === 1, 'a stale artifact exits 1');
assert(/light=STALE\(/.test(rs.out), `the stale tier is named (${rs.out.trim()})`);
assert(!/full=STALE/.test(rs.out), 'the fresh tier is not dragged red with it');

// (e) an unreadable tier directory → DENIED, not "empty". This is the state that
// used to be indistinguishable from a healthy one for a launchd-spawned checker.
// (Meaningless as root, where the mode bits are not enforced.)
if (process.getuid?.() !== 0) {
  f = statusFixture();
  makeArtifact(f, 'light');
  mkdirSync(join(f.backups, 'full'), { recursive: true });
  chmodSync(join(f.backups, 'full'), 0o000);
  rs = status(f, ['--quiet']);
  assert(/full=DENIED/.test(rs.out) && rs.code === 1, `an unreadable tier reports DENIED (${rs.out.trim()})`);
  chmodSync(join(f.backups, 'full'), 0o755);
}

// (f) SCHEDULER evidence. The artifact alone answers "when was the last backup";
// only this answers "is the thing that is supposed to make them working", which
// is the question the incident actually asked.
f = statusFixture();
makeArtifact(f, 'light');
makeArtifact(f, 'full');
const tccLine =
  '/bin/sh: /Volumes/Hard Disk/BlitzkriegBot/scripts/data-backup-cli.sh: Operation not permitted';
writeFileSync(join(f.logs, 'blitzkrieg-data-backup-light.log'), tccLine + LF);
const old = new Date(Date.now() - 2 * 3600 * 1000);
utimesSync(join(f.logs, 'blitzkrieg-data-backup-light.log'), old, old);
rs = status(f, ['--quiet']);
assert(rs.code === 1, 'a fresh artifact does NOT mask a failed scheduler');
assert(/light=ok\(0h\)\/sched=FAILED\(/.test(rs.out), `the tier's own scheduler failure is named (${rs.out.trim()})`);
assert(
  /sched=FAILED\([0-9]+h:Operation not permitted\)/.test(rs.out),
  'the token carries the FAILURE SIGNATURE, not an arbitrary truncation that cuts it off'
);
assert(!/full=ok\(0h\)\/sched=FAILED/.test(rs.out), 'a failing light scheduler does not blame the full tier');

// (g) ...and it must CLEAR: a later successful line is the newest evidence.
const okLine = '==> blitzkrieg backup (light) → /x/light' + LF + 'BACKUP OK';
writeFileSync(join(f.logs, 'blitzkrieg-data-backup-light.log'), okLine + LF);
rs = status(f, ['--quiet']);
assert(rs.code === 0, 'a newer successful run clears the scheduler verdict (KI-30)');
assert(/light=ok\(0h\)\/sched=ok/.test(rs.out), `the scheduler reads ok again (${rs.out.trim()})`);

// (h) a MANUAL run's record must never be scheduler evidence. Its own success is
// not evidence the schedule works — the exact false comfort of the incident.
f = statusFixture();
makeArtifact(f, 'light', 999); // stale artifact, so only the record could clear it
makeArtifact(f, 'full');
const now = Math.floor(Date.now() / 1000);
writeFileSync(
  join(f.state, 'light.status'),
  `version=1${LF}tier=light${LF}source=cli${LF}at=2026-01-01 00:00:00${LF}` +
    `attempt_epoch=${now}${LF}result=ok${LF}detail=${LF}backup_dir=/x/light${LF}`
);
rs = status(f, ['--quiet']);
assert(rs.code === 1, 'a manual (source=cli) success does not clear a stale scheduler');
assert(!/sched=ok/.test(rs.out), 'a manual success is not scheduler evidence at all');

// (i) the same record with source=loop IS evidence, and a failed one is loud.
f = statusFixture();
makeArtifact(f, 'light');
makeArtifact(f, 'full');
writeFileSync(
  join(f.state, 'light.status'),
  `version=1${LF}tier=light${LF}source=loop${LF}at=2026-01-01 00:00:00${LF}` +
    `attempt_epoch=${now}${LF}result=fail${LF}detail=exit 2: destination missing${LF}backup_dir=/x/light${LF}`
);
rs = status(f, ['--quiet']);
assert(rs.code === 1 && /light=ok\(0h\)\/sched=FAILED\(.*exit 2: destination missing/.test(rs.out), `a resident-loop failure is evidence and is quoted (${rs.out.trim()})`);

// (j) a non-integer tolerance is a usage error, not a silent zero.
f = statusFixture();
rs = status(f, ['--stale-hours', 'soon']);
assert(rs.code === 2, 'a bad --stale-hours is refused (exit 2)');

// ── 9. the attempt record (shared by every actor) ───────────────────────────
console.log('');
console.log('[9] the attempt record is one format, written by all three actors');
const LIB = join(ROOT, 'scripts', 'lib', 'backup-attempt.sh');
assert(existsSync(LIB), 'scripts/lib/backup-attempt.sh exists');
function attempt(f, args, env = {}) {
  // Sourced exactly the way the actors do it, so a syntax error or a renamed
  // function fails here rather than in a scheduled run nobody is watching.
  return shell(`. "${LIB}"; bk_attempt_write ${args}`, {
    BK_BACKUP_STATUS_DIR: f.state,
    ...env,
  });
}
f = statusFixture();
let ra = attempt(f, `light loop fail "exit 126: Operation not permitted" /x/light`);
assert(ra.code === 0, 'bk_attempt_write returns 0 even for a failure it records');
let rec = readFileSync(join(f.state, 'light.status'), 'utf8');
assert(/^version=1$/m.test(rec) && /^tier=light$/m.test(rec), 'record identifies its format version and tier');
assert(/^source=loop$/m.test(rec), 'record names the actor that ran it');
assert(/^result=fail$/m.test(rec) && /Operation not permitted/.test(rec), 'record carries the outcome and the reason');
assert(/^attempt_epoch=\d+$/m.test(rec), 'record carries a machine-readable epoch (no locale date parsing)');

ra = attempt(f, `light loop ok "" /x/light`);
rec = readFileSync(join(f.state, 'light.status'), 'utf8');
assert(/^result=ok$/m.test(rec) && /^success_epoch=\d+$/m.test(rec), 'a success records success_at/success_epoch');

ra = attempt(f, `light loop fail "exit 1: disk full" /x/light`);
rec = readFileSync(join(f.state, 'light.status'), 'utf8');
assert(/^result=fail$/m.test(rec) && /^success_epoch=\d+$/m.test(rec), 'a later failure keeps the last SUCCESS date (how old is the newest good copy)');

// A multi-line detail (a shell error is rarely one line) must not break the
// format: the record is read back with one `sed` per key.
ra = attempt(f, `full launchd fail "line one${'\\n'}line two${'\\n'}Operation not permitted" /x/full`);
rec = readFileSync(join(f.state, 'full.status'), 'utf8');
assert(rec.trim().split(LF).every((l) => /^[a-z_]+=/.test(l)), 'every record line is one key=value pair');
assert(!/^line two$/m.test(rec), 'a multi-line detail is collapsed, not written raw');

// Best-effort by contract: an unwritable record directory must not stop a backup.
ra = shell(`. "${LIB}"; bk_attempt_write light cli ok "" /x/light`, {
  BK_BACKUP_STATUS_DIR: '/no-such-dir-for-records',
});
assert(ra.code === 0, 'an unwritable record dir does not fail the caller (bookkeeping never blocks a backup)');

// ── 10. the checkout guard, including the linked-worktree shape (#231) ──────
console.log('');
console.log('[10] the checkout guard accepts a linked worktree (.git as a file)');
// `[ -d .git ]` alone made every linked worktree exit 2 — and exit 2 is
// indistinguishable from "ran fine" to a scheduler, so backups silently did not
// happen (issue #231). Both shapes must pass; neither may be inferred from the
// checked-out tree this gate happens to be running in.
function fakeRepo(gitShape) {
  const w = mkdtempSync(join(tmpdir(), 'data-backup-guard-'));
  writeFileSync(join(w, 'Cargo.toml'), '[package]' + LF + 'name = "fixture"' + LF);
  mkdirSync(join(w, 'scripts'), { recursive: true });
  copyFileSync(SCRIPT, join(w, 'scripts', 'data-backup.sh'));
  mkdirSync(join(w, 'data', 'trades'), { recursive: true });
  writeFileSync(join(w, 'data', 'trades', 'trades.jsonl'), '{"a":1}' + LF);
  if (gitShape === 'dir') mkdirSync(join(w, '.git'), { recursive: true });
  if (gitShape === 'file') writeFileSync(join(w, '.git'), 'gitdir: /elsewhere/.git/worktrees/fixture' + LF);
  // The destination must live OUTSIDE this synthetic repo, or the run would be
  // refused for a different reason and the guard under test would never be reached.
  const dest = join(mkdtempSync(join(tmpdir(), 'data-backup-guard-dest-')), 'dest');
  mkdirSync(dest, { recursive: true });
  return { root: w, dest, script: join(w, 'scripts', 'data-backup.sh') };
}
let repo = fakeRepo('file');
let rg = run(['--data', join(repo.root, 'data'), '--dest', repo.dest], {}, repo.script);
assert(rg.code === 0 && /BACKUP OK/.test(rg.out), `a worktree-shaped checkout (.git is a file) is accepted (${rg.out.trim().split(LF).pop()})`);
repo = fakeRepo('dir');
rg = run(['--data', join(repo.root, 'data'), '--dest', repo.dest], {}, repo.script);
assert(rg.code === 0, 'a normal clone (.git is a directory) is still accepted');
repo = fakeRepo('none');
rg = run(['--data', join(repo.root, 'data'), '--dest', repo.dest], {}, repo.script);
assert(rg.code === 2 && /not a BlitzkriegBot checkout/.test(rg.out), 'a tree with no .git at all is still refused');

// ── 11. the resident loop, route B (issue #217) ─────────────────────────────
console.log('');
console.log('[11] the resident loop runs a real backup, and a failure is loud');
// Route B is the one that works with no permission change, so it must be tested
// end to end rather than by inspection: a `local tier="$1" log="…$tier…"` slip in
// its first version made the very first `start --once` die with "tier: unbound
// variable" — i.e. the fallback route would have been broken too, silently.
const LOOP = join(ROOT, 'scripts', 'data-backup-loop.sh');
assert(existsSync(LOOP), 'scripts/data-backup-loop.sh exists');

/** A synthetic checkout: the real scripts, a few bytes of data, and a destination
 *  OUTSIDE it — data-backup.sh refuses a destination inside the repository (a
 *  workspace-level accident must not take the backup with it), which is the correct
 *  refusal this fixture first tripped over. The loop honours BK_REPO_ROOT, so this
 *  drives the real code against a tree that costs nothing to back up (this
 *  repository's own data/ is 8 GB). */
function loopRepo() {
  const root = mkdtempSync(join(tmpdir(), 'data-backup-loop-'));
  const dest = `${root}-dest`;
  mkdirSync(join(root, 'scripts', 'lib'), { recursive: true });
  mkdirSync(join(root, '.git'), { recursive: true });
  writeFileSync(join(root, 'Cargo.toml'), '[package]' + LF + 'name = "fixture"' + LF);
  for (const f of ['data-backup.sh', 'data-backup-cli.sh', 'data-backup-loop.sh']) {
    copyFileSync(join(ROOT, 'scripts', f), join(root, 'scripts', f));
  }
  copyFileSync(join(ROOT, 'scripts', 'lib', 'backup-attempt.sh'), join(root, 'scripts', 'lib', 'backup-attempt.sh'));
  mkdirSync(join(root, 'data', 'trades'), { recursive: true });
  writeFileSync(join(root, 'data', 'trades', 'trades.jsonl'), '{"a":1}' + LF);
  mkdirSync(join(root, 'logs'), { recursive: true });
  mkdirSync(join(dest, 'light'), { recursive: true });
  mkdirSync(join(dest, 'full'), { recursive: true });
  // A fresh FULL artifact, so the green verdict below is the whole verdict (a
  // fixture that only ever has a light tier would exit 1 for the full tier's
  // absence and hide whether the light half was actually read).
  // Name must match data-backup.sh's BACKUP_NAME_RE (…YYYYMMDDTHHMMSSZ): a
  // stamp without its trailing Z is a directory the verdict cannot see at all.
  const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+/, '');
  mkdirSync(join(dest, 'full', `blitzkrieg-data-${stamp}`), { recursive: true });
  writeFileSync(join(dest, 'full', `blitzkrieg-data-${stamp}`, 'data.tar.gz'), 'fixture');
  return { root, dest, logs: join(root, 'logs'), light: join(dest, 'light') };
}
function runLoop(f, args, env = {}) {
  // Invoked through bash: the loop is not required to carry the executable bit
  // for the gate to drive it, and a gate should test behaviour, not modes.
  return shell(`bash "${LOOP}" ${args}`, {
    BK_REPO_ROOT: f.root,
    BLITZKRIEG_BACKUP_DIR: f.dest,
    BK_BACKUP_LOG_DIR: f.logs,
    BK_BACKUP_STATUS_DIR: f.logs,
    ...env,
  });
}
let lf = loopRepo();
let rl = runLoop(lf, 'start --once');
let loopLog = join(lf.logs, 'blitzkrieg-data-backup-light-loop.log');
let lastLine = readFileSync(loopLog, 'utf8').trim().split(LF).pop();
assert(rl.code === 0, `start --once exits 0 (${rl.out.trim().split(LF)[0]})`);
assert(/light backup ok rc=0$/.test(lastLine), `the run log ends with a success marker (${lastLine})`);
const loopMade = readdirSync(lf.light).filter((n) => n.startsWith('blitzkrieg-data-'));
assert(loopMade.length === 1, `the loop produced exactly one backup (got ${loopMade.length})`);
assert(existsSync(join(lf.light, loopMade[0] || 'x', 'data.tar.gz')), 'the backup it produced holds a data.tar.gz');
let loopRec = readFileSync(join(lf.logs, 'light.status'), 'utf8');
assert(/^source=loop$/m.test(loopRec) && /^result=ok$/m.test(loopRec), 'the loop identifies itself as `loop` and records ok');

// The loop's own log is SCHEDULER evidence, so --status must read it as such
// (an artifact-only verdict would already be green here — this is the check that
// still works when the artifact is old and the scheduler is dead).
rs = run(['--status', '--quiet'], {
  BK_BACKUP_DIR: lf.dest,
  BK_BACKUP_LOG_DIR: lf.logs,
  BK_BACKUP_STATUS_DIR: lf.logs,
});
assert(rs.code === 0 && /light=ok\(0h\)\/sched=ok\(/.test(rs.out), `--status reads the loop as the scheduler (${rs.out.trim()})`);

// A failing run must be loud in all three places the operator can look.
lf = loopRepo();
rl = runLoop(lf, 'start --once', { BLITZKRIEG_BACKUP_DIR: '/dev/null/not-a-dir' });
loopLog = join(lf.logs, 'blitzkrieg-data-backup-light-loop.log');
lastLine = readFileSync(loopLog, 'utf8').trim().split(LF).pop();
assert(rl.code !== 0, 'a failed run exits non-zero');
assert(/^\[[^\]]+\] FAIL: light backup rc=/.test(lastLine), `the run log's last line is a FAIL line (${lastLine})`);
loopRec = readFileSync(join(lf.logs, 'light.status'), 'utf8');
assert(/^result=fail$/m.test(loopRec), 'the attempt record records the failure');
assert(/cannot be created|not an existing directory|destination/.test(loopRec), 'the record names the reason');

// status with no live loop is not "fine": it must say so and point at the check.
lf = loopRepo();
rl = runLoop(lf, 'status');
assert(rl.code === 1 && /NOT RUNNING/.test(rl.out), 'status reports a stopped loop with exit 1');
assert(/--status/.test(rl.out), 'a stopped loop points at the freshness check, not just at itself');

// Usage errors stay usage errors (a typo must not start a backup, and a typo that
// LOOKS like a time must not silently disable a tier either: `25:99` passes a
// shape-only check, python then raises inside seconds_until, the `|| echo 86400`
// fallback swallows it, and the tier never runs again).
lf = loopRepo();
rl = runLoop(lf, 'start --light-at 25:99');
assert(rl.code === 2 && /invalid --light-at/.test(rl.out), 'an out-of-range schedule time is refused (exit 2)');
rl = runLoop(lf, 'start --light-at 4:5');
assert(rl.code === 2 && /invalid --light-at/.test(rl.out), 'a malformed schedule time is refused (exit 2)');
assert(!existsSync(join(lf.logs, 'blitzkrieg-data-backup-loop.pid')), 'a refused start left no pidfile behind');

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
