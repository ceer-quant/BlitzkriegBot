#!/usr/bin/env node
/**
 * contract-tree-check — the workspace dependency-graph freeze (#840 PR0).
 *
 * The contract-freeze campaign freezes THREE things people usually only
 * freeze in docs: the wire JSON, the ABI layout, and now the CRATE GRAPH.
 * The graph is the fourth guardrail: PR1..PR3 migrate types, readers and the
 * strategy ABI between crates, and the proof that a migration moved TYPES —
 * not DEPENDENCIES — is this gate. It pins, byte-exact and version-free:
 *
 *   [internal]  every workspace-internal edge `member > member`
 *               (catches: a contract crate growing a dependency on the core,
 *               blitzkrieg-core reaching into extensions/, any re-parenting
 *               the four-extension "zero reverse dependency" rule forbids);
 *   [external]  every DIRECT external dependency name of every workspace
 *               member, `member :: dep`
 *               (catches: tokio/reqwest riding into blitzkrieg-market-api /
 *               blitzkrieg-strategy-api / blitzkrieg-data-model, a new
 *               heavyweight landing in a leaf crate — #840's freeze proposal
 *               rule "three crates with zero tokio/reqwest" becomes red here).
 *
 * VERSIONS ARE DELIBERATELY STRIPPED: a Cargo.lock bump must not red this
 * gate — it freezes the SHAPE of the graph, not the lockfile. Any diff is a
 * disclosed graph change: re-baseline with `--rebaseline` INSIDE the PR that
 * moves the edge, and say so in the PR body.
 *
 * Determinism: edges are derived from
 *   cargo tree --workspace --edges normal,build,dev --target all
 * (`--target all` so platform-conditional edges are host-independent), the
 * per-package subtrees are flattened, versions and tree glyphs stripped,
 * then sorted + deduped. The baseline lives in
 * scripts/contract-tree-baseline.txt.
 *
 * Usage:
 *   node scripts/contract-tree-check.mjs                # the real verdict
 *   node scripts/contract-tree-check.mjs --rebaseline   # rewrite the baseline
 *   node scripts/contract-tree-check.mjs --self-test    # in-memory teeth, no cargo
 *
 * Exit: 0 pass / 1 violations.
 */

import { execFileSync } from 'child_process';
import { readFileSync, writeFileSync } from 'fs';
import { resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BASELINE = resolve(ROOT, 'scripts', 'contract-tree-baseline.txt');

// ── extraction ───────────────────────────────────────────────────────────────

/** Strip the version (and any `(path)`/`(proc-macro)` suffix) from a tree
 * line body, leaving the bare package name. */
export function pkgName(body) {
  const m = body.match(/\s+v\d+(\.\d+)+/);
  return (m ? body.slice(0, m.index) : body).trim();
}

/**
 * Flatten `cargo tree` text into edge records.
 * Returns { internal: Set<"parent > child">, external: Map<member, Set<dep>> }.
 * Indent unit is 4 (ascii glyphs `|-- ` / `|   ` / ``-- `); the per-root stack
 * resolves each line's parent; a trailing `(*)` (subtree elided — already
 * printed once) still contributes its edge, it just has no children to walk.
 */
export function extractEdges(treeText, memberNames) {
  const members = new Set(memberNames);
  const internal = new Set();
  const external = new Map((memberNames.map((m) => [m, new Set()])));
  // Stack of {depth, name}; reset at every depth-0 root.
  const stack = [];
  for (const raw of treeText.split('\n')) {
    if (!raw.trim()) continue;
    const lead = raw.match(/^[|`\s\\-]*/)[0].length;
    const depth = Math.floor(lead / 4);
    const body = raw.slice(lead).replace(/\s+\(\*\)\s*$/, '');
    const name = pkgName(body);
    while (stack.length && stack[stack.length - 1].depth >= depth) stack.pop();
    const parent = stack.length ? stack[stack.length - 1].name : null;
    stack.push({ depth, name });
    if (!parent) continue; // a workspace root has no parent edge
    const parentIsMember = members.has(parent);
    const childIsMember = members.has(name);
    if (parentIsMember && childIsMember) internal.add(`${parent} > ${name}`);
    else if (parentIsMember && external.has(parent)) external.get(parent).add(name);
  }
  return { internal, external };
}

/** The normalized freeze set as sorted text lines (the baseline body). */
export function freezeLines({ internal, external }) {
  const lines = [...internal].sort();
  for (const member of [...external.keys()].sort()) {
    for (const dep of [...external.get(member)].sort()) {
      lines.push(`${member} :: ${dep}`);
    }
  }
  return lines;
}

// ── live sources ─────────────────────────────────────────────────────────────

function workspaceMembers() {
  const meta = JSON.parse(
    execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {
      cwd: ROOT,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    }),
  );
  // "path+file:///...#name@0.4.1" → "name"
  return meta.workspace_members.map((s) => s.split('#')[1].split('@')[0]);
}

function cargoTreeText() {
  return execFileSync(
    'cargo',
    ['tree', '--workspace', '--edges', 'normal,build,dev', '--target', 'all', '--charset', 'ascii'],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
}

// ── baseline I/O ─────────────────────────────────────────────────────────────

const HEADER = `# contract-tree-baseline — the frozen workspace dependency graph (#840 PR0).
# Generated by: node scripts/contract-tree-check.mjs --rebaseline
#   [internal]  member > member        — every workspace-internal edge
#   [external]  member :: external-dep — every DIRECT external dep name
# Versions are deliberately absent (Cargo.lock bumps must stay green). A diff
# means the CRATE GRAPH changed: re-baseline inside the PR that moved the edge
# and disclose it in the PR body.
`;

function parseBaseline(text) {
  const section = { internal: new Set(), external: new Set() };
  let which = null;
  for (const raw of text.split('\n')) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    if (line === '[internal]') { which = 'internal'; continue; }
    if (line === '[external]') { which = 'external'; continue; }
    if (which) section[which].add(line);
  }
  return section;
}

function renderBaseline(freeze) {
  const lines = freezeLines(freeze);
  const internal = lines.filter((l) => l.includes(' > '));
  const external = lines.filter((l) => l.includes(' :: '));
  return (
    HEADER +
    '[internal]\n' + (internal.length ? internal.join('\n') + '\n' : '') +
    '[external]\n' + (external.length ? external.join('\n') + '\n' : '')
  );
}

// ── the judge (shared by the real verdict and --self-test) ───────────────────

export function judge(baselineText, freeze) {
  const want = parseBaseline(baselineText);
  const gotInternal = freezeLines(freeze).filter((l) => l.includes(' > '));
  const gotExternal = freezeLines(freeze).filter((l) => l.includes(' :: '));
  const problems = [];
  for (const edge of gotInternal) {
    if (!want.internal.has(edge)) problems.push(`+ internal edge added:   ${edge}`);
  }
  for (const edge of want.internal) {
    if (!freeze.internal.has(edge)) problems.push(`- internal edge removed: ${edge}`);
  }
  for (const dep of gotExternal) {
    if (!want.external.has(dep)) problems.push(`+ external dep added:    ${dep}`);
  }
  for (const dep of want.external) {
    const [member, name] = dep.split(' :: ');
    if (!freeze.external.get(member)?.has(name)) problems.push(`- external dep removed:  ${dep}`);
  }
  return problems;
}

// ── --self-test: prove the teeth on in-memory fixtures (§A.4) ────────────────

function selfTest() {
  const members = ['a-crate', 'b-crate', 'ext'];
  const treeA = [
    'a-crate v1.0.0 (path)',
    '    |-- b-crate v1.0.0 (path)',
    '    |   `-- serde v1.0.0',
    '    `-- serde v1.0.0',
    'b-crate v1.0.0 (path)',
    '    `-- serde v1.0.0',
    'ext v1.0.0 (path)',
    '    `-- libc v1.0.0',
  ].join('\n');
  const base = extractEdges(treeA, members);
  const baseText = renderBaseline(base);
  const checks = [];
  const expect = (name, ok) => checks.push({ name, ok });

  // 1. identical tree ⇒ green.
  expect('identical graph passes', judge(baseText, base).length === 0);

  // 2. a NEW internal edge (a-crate starts depending on ext) ⇒ red.
  const grown = extractEdges(
    treeA.replace('    `-- serde v1.0.0', '    |-- serde v1.0.0\n    `-- ext v1.0.0 (path)'),
    members,
  );
  const p2 = judge(baseText, grown);
  expect('added internal edge reds', p2.length === 1 && p2[0].includes('a-crate > ext'));

  // 3. a REMOVED internal edge ⇒ red.
  const cut = extractEdges(
    treeA.replace('    |-- b-crate v1.0.0 (path)\n    |   `-- serde v1.0.0\n', ''),
    members,
  );
  const p3 = judge(baseText, cut);
  expect('removed internal edge reds', p3.some((p) => p.includes('a-crate > b-crate')));

  // 4. a NEW external dep on a contract crate (tokio!) ⇒ red.
  const tokio = extractEdges(
    treeA.replace('ext v1.0.0 (path)', 'ext v1.0.0 (path)\n    |-- tokio v1.0.0'),
    members,
  );
  const p4 = judge(baseText, tokio);
  expect('new external dep (tokio) reds', p4.some((p) => p.includes('ext :: tokio')));

  // 5. an external dep REMOVED ⇒ red (the ratchet bites both ways).
  const slim = extractEdges(
    treeA.replace('    `-- libc v1.0.0', ''),
    members,
  );
  const p5 = judge(baseText, slim);
  expect('removed external dep reds', p5.some((p) => p.includes('ext :: libc')));

  // 6. versions bumping must NOT red (the lockfile-noise guarantee).
  const bumped = extractEdges(treeA.replaceAll('v1.0.0', 'v9.9.9'), members);
  expect('version bumps stay green', judge(baseText, bumped).length === 0);

  const failed = checks.filter((c) => !c.ok);
  for (const c of checks) console.log(`${c.ok ? 'ok  ' : 'RED '} self-test: ${c.name}`);
  if (failed.length) {
    console.error(`\ncontract-tree-check self-test: ${failed.length} RED`);
    process.exit(1);
  }
  console.log('\ncontract-tree-check self-test: all green');
}

// ── main ─────────────────────────────────────────────────────────────────────

if (process.argv.includes('--self-test')) {
  selfTest();
  process.exit(0);
}

const members = workspaceMembers();
const freeze = extractEdges(cargoTreeText(), members);

if (process.argv.includes('--rebaseline')) {
  writeFileSync(BASELINE, renderBaseline(freeze));
  const n = freezeLines(freeze).length;
  console.log(`contract-tree-check: baseline rewritten — ${members.length} members, ${n} frozen lines`);
  process.exit(0);
}

const problems = judge(readFileSync(BASELINE, 'utf8'), freeze);
if (problems.length) {
  console.error(`contract-tree-check: ${problems.length} graph change(s) vs baseline —\n`);
  for (const p of problems) console.error(`  ${p}`);
  console.error(
    '\nIntentional? Re-baseline INSIDE the PR that moves the edge' +
      ' (`node scripts/contract-tree-check.mjs --rebaseline`) and disclose the diff in the PR body.',
  );
  process.exit(1);
}
console.log(
  `contract-tree-check: PASS — ${members.length} members, ` +
    `${freeze.internal.size} internal edges, ` +
    `${[...freeze.external.values()].reduce((n, s) => n + s.size, 0)} direct external deps`,
);
