#!/usr/bin/env node
/**
 * Which code is under test (#172 / #179).
 *
 * This gate exists because of one specific incident and one specific class of
 * failure. The incident: `scripts/core-parity.mjs` reported eight gate.failures that
 * all descended from a taker order being correctly REJECTED — the gate's script
 * came from one checkout while the binary it drove had been built from another,
 * and nothing in the output said so. The class: every gate in this repository is
 * a claim about a binary, and a claim whose subject cannot name its own revision
 * is not evidence about anything in particular.
 *
 * So this gate asserts the three identities that make the other gates readable:
 *
 *   1. the binary under test EMBEDS a revision (`<semver>+g<sha>`, stamped at
 *      compile time by the `core/build_info` crate's build.rs — see #179);
 *   2. that revision is THIS checkout's revision (or `BK_EXPECT_SHA`'s), so the
 *      script and the binary are the same code state, not two different ones;
 *   3. a core STARTED FROM that binary reports the same revision over the wire
 *      (`core.ready.build` / `.commit`), and prints it on its startup line — the
 *      process that answers a gate is the code the gate is about.
 *
 * It then reports the lineage around that revision: how it stands against the
 * deployment source (`scripts/upgrade.sh`'s `SOURCE_REF`, read only — this gate
 * never changes the deployment) and against `main`. That part is INFORMATIONAL by
 * default, because the deployment line and `main` legitimately diverge today
 * (#179's convergence half is tracked separately); a caller that can require
 * containment asks for it explicitly with `--require-ancestor <ref>`, which turns
 * a missing ancestor into a non-zero exit.
 *
 * What it cannot do: make a gate honest about code it never built. Running this
 * before the parity gates is what does that, and CI does exactly that.
 *
 * Usage:
 *   node scripts/core-provenance-check.mjs                        # report + check
 *   node scripts/core-provenance-check.mjs --require-ancestor origin/main
 *   BK_CORE_BIN=/elsewhere/blitzkrieg-core node scripts/core-provenance-check.mjs
 *
 * Exit 0 only if every blocking check passes.
 */
import { execFileSync } from 'child_process';
import { existsSync, appendFileSync, readFileSync } from 'fs';
import { tmpdir } from 'os';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';
import { mkdtempSync } from 'fs';
import { CoreClient, rpc } from './lib/core-client.mjs';
import { scratchSocketPath } from './lib/core-socket.mjs';
import { createChecks } from './lib/gate-harness.mjs';
import { pollUntil } from './lib/wait.mjs';
import {
  coreBinaryPath,
  binaryVersion,
  revisionOf,
  checkoutRevision,
  expectedRevision,
  checkCoreProvenance,
} from './lib/core-provenance.mjs';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const BIN = coreBinaryPath(ROOT);

// Lines worth a lineage statement: the deployment source (read from upgrade.sh,
// never written) and the long-lived integration branch.
const LINES = ['main', 'feat/trading-safety-selfcheck'];

const argv = process.argv.slice(2);
const requiredAncestors = [];
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === '--require-ancestor' && argv[i + 1]) {
    requiredAncestors.push(argv[i + 1]);
    i++;
  }
}
const summaryLines = [];
function summary(text) {
  summaryLines.push(text);
  if (process.env.GITHUB_STEP_SUMMARY) {
    try { appendFileSync(process.env.GITHUB_STEP_SUMMARY, text + '\n'); } catch { /* summary is best-effort */ }
  }
}

const gate = createChecks();
const { check } = gate;

/** Read a git command's stdout, or null when it cannot answer. */
function git(args) {
  try {
    return execFileSync('git', args, {
      cwd: ROOT, encoding: 'utf8', timeout: 20_000, stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  } catch {
    return null;
  }
}

/** Is `ref` an ancestor of HEAD? null when the ref cannot be resolved at all. */
function isAncestor(ref) {
  if (git(['rev-parse', '--verify', '--quiet', `${ref}^{commit}`]) === null) return null;
  try {
    execFileSync('git', ['merge-base', '--is-ancestor', ref, 'HEAD'],
      { cwd: ROOT, timeout: 20_000, stdio: 'ignore' });
    return true;
  } catch {
    // Exit 1 is "not an ancestor" (the answer); anything else we also read as
    // "not contained", which is the safe direction for a required ancestor.
    return false;
  }
}

/**
 * The deployment source this tree ships from, read out of `scripts/upgrade.sh`
 * rather than restated here: #179's other half is that this reference should stop
 * pointing at a feature branch, and a gate that hard-coded the branch would keep
 * agreeing with itself after the switch. Read-only — this gate never edits it.
 */
function deploySourceRef() {
  try {
    const src = readFileSync(join(ROOT, 'scripts', 'upgrade.sh'), 'utf8');
    const line = src.split('\n').find((l) => l.startsWith('SOURCE_REF='));
    if (!line) return null;
    const m = /\$\{[A-Z_]+:-([^}]+)\}/.exec(line) ?? /=["']?([^"'\s]+)["']?/.exec(line);
    return m ? m[1] : null;
  } catch {
    return null;
  }
}

/** Where a deployment-source string resolves locally, best effort. */
function localRefFor(deployRef) {
  if (!deployRef) return null;
  const candidates = [deployRef, deployRef.replace(/^[^/]+\//, 'origin/'), `origin/${deployRef}`];
  for (const c of candidates) {
    if (git(['rev-parse', '--verify', '--quiet', `${c}^{commit}`]) !== null) return c;
  }
  return null;
}

/**
 * Every ref that resolves for a line NAME: the local branch first, then each
 * remote-tracking ref of that name (`ceer/x`, `origin/x`, …). A local branch that
 * predates its upstream is the normal state of a clone nobody fetched, and
 * reporting only that one would answer "what does this line contain" with a
 * stale sha — so both are shown when they disagree, each named, and the reader
 * can see which is which instead of trusting the first hit.
 */
function lineageRefs(name) {
  const out = [];
  const push = (c) => {
    if (out.includes(c)) return;
    if (git(['rev-parse', '--verify', '--quiet', `${c}^{commit}`]) !== null) out.push(c);
  };
  push(name);
  const tracked = (git(['for-each-ref', '--format=%(refname:short)', 'refs/remotes']) ?? '')
    .split('\n')
    .map((r) => r.trim())
    .filter((r) => r.endsWith(`/${name}`));
  for (const r of tracked) push(r);
  return out;
}

/** How many commits each side of `ref...HEAD` has, or null when it cannot be read. */
function divergence(ref) {
  const out = git(['rev-list', '--left-right', '--count', `${ref}...HEAD`]);
  if (out === null) return null;
  const [behind, ahead] = out.split(/\s+/);
  return { behind: Number(behind), ahead: Number(ahead) };
}

// ── 1/2. The binary, and whether it is this checkout ─────────────────────────
checkCoreProvenance(BIN, check);
const version = binaryVersion(BIN);
if (version !== null) summary(`- binary under test: \`${version}\` (${BIN})`);

// ── 3. The serving process reports the same revision ─────────────────────────
const WORKDIR = mkdtempSync(join(tmpdir(), 'blitzkrieg-prov-'));
const core = new CoreClient({
  binaryPath: BIN,
  socketPath: scratchSocketPath('prov'),
  mode: 'dry',
  seedBalance: 100,
  tickMs: 50,
  autoRestart: false,
  cwd: WORKDIR,
  noTradeLog: true,
  noOrderLog: true,
  noPositionLog: true,
  extraArgs: ['--no-discovery', '--no-event-archive'],
});

try {
  await core.start();
  const ready = await rpc.ready(core);
  console.log(`  core version=${ready.version} build=${ready.build ?? '?'} ` +
    `commit=${ready.commit ?? '?'} dirty=${ready.dirty ?? '?'}`);
  const binRevision = revisionOf(version);
  const binSha = binRevision !== null && binRevision !== 'nogit' ? binRevision.slice(1) : null;

  check('the serving core reports the binary\'s build string',
    binRevision !== null && ready.build === version,
    `core.ready.build ${JSON.stringify(ready.build)} != --version ${JSON.stringify(version)}`);
  check('the serving core reports the binary\'s revision as its commit',
    binSha !== null && ready.commit === binSha,
    `core.ready.commit ${JSON.stringify(ready.commit)} != ${JSON.stringify(binSha)}`);
  check('the serving core reports a semantic version',
    typeof ready.version === 'string' && /^\d+\.\d+\.\d+$/.test(ready.version),
    JSON.stringify(ready.version));

  // The startup line (#179) is the provenance record an incident review reads
  // back out of a log, so its presence is asserted rather than assumed. It is
  // written to stderr before the socket is bound, so by now it is there — the
  // short poll only absorbs a pipe flush.
  await pollUntil(() => Boolean(binRevision && core.lastStderr.includes(binRevision)), { timeoutMs: 1000 });
  check('startup line names the revision it is running',
    binRevision !== null && core.lastStderr.includes(binRevision),
    `stderr did not mention ${binRevision}: ${JSON.stringify(core.lastStderr.slice(-200))}`);
  // VERSIONING.md V3-4: the same process, asked the newer, richer question.
  // `system.version` and `core.ready` describe ONE build, so they must agree;
  // the update half must arrive three-state (null = not checked) with the
  // switch off on a default stack — never a fabricated `false`. An older core
  // answers "unknown method": a note, not a failure, the method is additive
  // (and BK_CORE_BIN routinely points this gate at frozen baselines).
  try {
    const sv = await rpc.systemVersion(core);
    console.log(`  core ${sv.version}+g${sv.gitHash}${sv.gitDirty ? ' (dirty)' : ''} ` +
      `built ${sv.buildDate} for ${sv.target}`);
    check('system.version agrees with core.ready about the build',
      sv.version === ready.version && sv.gitHash === ready.commit && sv.gitDirty === ready.dirty,
      `system.version ${JSON.stringify(sv)} vs core.ready ${JSON.stringify(ready)}`);
    check('system.version keeps the update state three-state on a default stack',
      sv.updateAvailable === null && sv.lastCheckMs === null && sv.checkEnabled === false,
      `updateAvailable=${JSON.stringify(sv.updateAvailable)} checkEnabled=${sv.checkEnabled} — ` +
      'a default stack checks nothing and reports "not checked", never "up to date"');
    summary(`- system.version: \`${sv.version}+g${sv.gitHash}\` ` +
      `(checkEnabled ${sv.checkEnabled}, updateAvailable ${JSON.stringify(sv.updateAvailable)})`);
  } catch (e) {
    console.log(`  note system.version not answered (${e.message}) — older core, additive method`);
  }
  if (ready.build) summary(`- serving process reports: \`${ready.build}\` (commit ${ready.commit ?? '?'}, dirty ${ready.dirty ?? '?'})`);
} catch (e) {
  check('serving-core provenance', false, e?.stack || e);
} finally {
  await core.stop();
}

// ── Lineage: what this revision contains, and what it is missing ─────────────
// Informational unless an ancestor is explicitly required. The deployment-source
// ref is reported because "the gate is green" is only useful next to "and this is
// what the deployment builds from".
const expected = expectedRevision(ROOT);
const head = checkoutRevision(ROOT);
const deployRef = deploySourceRef();
const deployLocal = localRefFor(deployRef);
console.log(`  head ${head ?? '(no repository)'} ` +
  `(expected: ${expected.source ?? 'unknown'}${expected.sha ? `=${expected.sha}` : ''})`);
console.log(`  deploy source (scripts/upgrade.sh SOURCE_REF): ${deployRef ?? 'not in this tree'}` +
  `${deployRef && !deployLocal ? ' (ref not fetched here)' : ''}`);
summary(`- checkout ${head ?? 'unknown'}; expected from ${expected.source ?? 'unknown'}` +
  `${expected.sha ? ` (${expected.sha})` : ''}`);
summary(`- deployment source: \`${deployRef ?? 'scripts/upgrade.sh is not in this tree (see #179)'}\``);

const refs = [...new Set([...LINES, ...requiredAncestors].flatMap((l) => {
  const all = lineageRefs(l);
  return all.length ? all : [l];
}))];
for (const ref of refs) {
  if (git(['rev-parse', '--verify', '--quiet', `${ref}^{commit}`]) === null) {
    console.log(`  line ${ref}: not present in this clone (fetch it to compare)`);
    continue;
  }
  const d = divergence(ref);
  if (d === null) {
    console.log(`  line ${ref}: unreadable`);
    continue;
  }
  const sha = git(['rev-parse', '--short=12', ref]);
  console.log(`  line ${ref} @ ${sha}: HEAD is ${d.ahead} ahead / ${d.behind} behind ` +
    `(${d.behind === 0 ? 'contains' : 'does NOT contain'} ${ref})`);
  summary(`- \`${ref}\` @ ${sha}: HEAD ${d.ahead} ahead / ${d.behind} behind` +
    `${d.behind === 0 ? ' (contains it)' : ' (does NOT contain it)'}`);
}

for (const ref of requiredAncestors) {
  const local = localRefFor(ref) ?? ref;
  if (git(['rev-parse', '--verify', '--quiet', `${local}^{commit}`]) === null) {
    // A ref this clone does not have cannot be checked, and must not be turned
    // into a failure: whoever asked for the check fetches the ref first.
    console.log(`  ancestor check ${ref}: ref is not present in this clone — skipped`);
    continue;
  }
  check(`HEAD contains ${ref}`,
    isAncestor(local) === true,
    `${ref} does not contain HEAD's base — this branch is built on a code state that is ` +
    'missing it (that is the #172 mismatch, stated before any gate can report green about it)');
}

console.log(gate.failures === 0
  ? '\nCORE PROVENANCE OK — the binary under test is this checkout, and it says so itself.'
  : `\nCORE PROVENANCE FAILED (${gate.failures})`);
if (gate.failures !== 0) summary(`- **provenance gate failed (${gate.failures})**`);
process.exit(gate.failures === 0 ? 0 : 1);
