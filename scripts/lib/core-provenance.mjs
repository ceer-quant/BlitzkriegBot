/**
 * Which code is under test (#172 / #179).
 *
 * The incident this exists to prevent: `node scripts/core-parity.mjs` reported
 * eight failures, all descended from a taker order that was correctly rejected —
 * because the gate script came from one checkout while the binary it drove was
 * built from another. Nothing in the gate's output said so, and the only way to
 * work it out was `nm | grep` on the binary. A gate whose conclusion cannot name
 * the code it describes is not evidence.
 *
 * So every gate that drives the release binary now starts by stating the
 * revision the binary carries (`<semver>+g<sha>`, stamped by the
 * `core/build_info` crate's build.rs — see VERSIONING.md §3.2) and, when the
 * checkout's own revision is knowable, asserting the two are the same. A
 * mismatch is a RED gate with the cause named in the message, not a silent
 * wrong answer.
 *
 * The pin is resolved with this precedence:
 *   BK_EXPECT_SHA   — an explicit expectation (what the deployment line is)
 *   git HEAD        — the tree the gate and the binary were built from
 *   GITHUB_SHA      — the CI checkout's revision
 *   (none)          — nothing to compare against: the revision is printed, and
 *                     the check reports that it could not be pinned rather than
 *                     passing silently.
 */

import { execFileSync } from 'child_process';
import { existsSync } from 'fs';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');

/** `<semver>+g<sha>` / `<semver>+nogit`, as `--version` prints it. The semver
 * half may carry a pre-release `-rc.N` (VERSIONING.md V8-5) — accept it BEFORE
 * any rc is ever cut, so the first rc does not sail through with the gate
 * silently refusing its self-description. */
export const VERSION_RE = /^\d+\.\d+\.\d+(?:-rc\.\d+)?\+(?:g[0-9a-f]{4,40}|nogit)$/;

/**
 * Run `<bin> --version`. Returns null when the binary is missing or does not
 * answer — a caller decides whether that is fatal (it always is, for a gate that
 * is about to drive that binary).
 */
export function binaryVersion(binPath) {
  if (!existsSync(binPath)) return null;
  try {
    return execFileSync(binPath, ['--version'], {
      encoding: 'utf8',
      timeout: 20_000,
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  } catch {
    return null;
  }
}

/** The revision token (`g<sha>` / `nogit`) out of a version string, or null. */
export function revisionOf(versionString) {
  const m = /^(\d+\.\d+\.\d+)\+(g[0-9a-f]{4,40}|nogit)$/.exec(versionString ?? '');
  return m ? m[2] : null;
}

/** The checkout's revision, short form, or null when there is no repository. */
export function checkoutRevision(repoRoot = ROOT) {
  try {
    return execFileSync('git', ['rev-parse', '--short=12', 'HEAD'], {
      cwd: repoRoot,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim() || null;
  } catch {
    return null;
  }
}

/** The revision a gate should require, by the precedence documented above. */
export function expectedRevision(repoRoot = ROOT) {
  const explicit = (process.env.BK_EXPECT_SHA ?? '').replace(/^g/, '').trim();
  if (explicit) return { sha: explicit, source: 'BK_EXPECT_SHA' };
  const head = checkoutRevision(repoRoot);
  if (head) return { sha: head, source: 'git HEAD' };
  const ci = (process.env.GITHUB_SHA ?? '').trim();
  if (ci) return { sha: ci.slice(0, 12), source: 'GITHUB_SHA' };
  return { sha: null, source: null };
}

/**
 * Print the binary's provenance and return the problems found (empty = the
 * binary is the one this checkout describes).
 *
 * `check` is the caller's own assertion function, so the failures land in the
 * gate's existing count and formatting rather than in a second reporting style.
 */
export function checkCoreProvenance(binPath, check, { repoRoot = ROOT } = {}) {
  const version = binaryVersion(binPath);
  if (version === null) {
    check('binary answers --version', false, `${binPath} (missing or not executable — build it: cargo build --release -p blitzkrieg-core)`);
    return { version: null, revision: null };
  }
  check('binary reports <semver>+g<sha> provenance', VERSION_RE.test(version), `got ${JSON.stringify(version)}`);

  const revision = revisionOf(version);
  const expected = expectedRevision(repoRoot);
  console.log(`  bin  ${binPath} -> ${version}`);

  if (revision === null) {
    return { version, revision: null };
  }
  if (revision === 'nogit') {
    check(
      'binary names the revision it was built from',
      false,
      'build.rs found no git and no BLITZKRIEG_GIT_SHA — a binary that cannot name its code cannot be gated (#179)',
    );
    return { version, revision };
  }
  if (expected.sha === null) {
    console.log(`  note expected revision unknown (no git, no BK_EXPECT_SHA): pinning skipped, revision=${revision}`);
    return { version, revision };
  }
  console.log(`  want ${expected.source}=${expected.sha}`);
  check(
    `binary under test is the ${expected.source} revision`,
    revision === `g${expected.sha}`,
    `binary says ${revision}, ${expected.source} says g${expected.sha} — the gate and the binary come from ` +
    'different code states (rebuild the core, or set BK_CORE_BIN / BK_EXPECT_SHA deliberately; see #172)',
  );
  return { version, revision };
}

/**
 * The binary a gate should drive: `BK_CORE_BIN` when set (used to test a binary
 * built elsewhere, and by the fee-model reverse test), else the release path
 * this repository builds.
 */
export function coreBinaryPath(repoRoot = ROOT) {
  return process.env.BK_CORE_BIN
    ? process.env.BK_CORE_BIN
    : join(repoRoot, 'target', 'release', 'blitzkrieg-core');
}
