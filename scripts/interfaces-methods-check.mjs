#!/usr/bin/env node
/**
 * interfaces-methods-check — the #644 contract-ledger gate.
 *
 * The contract it guards: `docs/rust-core/INTERFACES.md` §2 claims to be the
 * single source of truth for the UDS JSON-RPC surface, and
 * `core/blitzkrieg_core/src/ipc/schema.rs` IS the compiled one. When the two
 * drift apart the documentation quietly stops being a contract (the 2026-10-26
 * audit D-2: 79 constants, 36 documented — including money-sensitive verbs
 * like `risk.setExit` living outside the ledger). A new `pub const *_METHOD`
 * — actually, a new `pub const <NAME>: &str` inside `pub mod method` — without
 * a matching §2 table row must turn a PR red BEFORE merge, the same way
 * lua-decimal-check turns a divergent decimal block red.
 *
 * Extraction, by structure not by guessing:
 *   schema.rs   → the `pub mod method { … }` block; every `pub const NAME:
 *                 &str = "…"` whose string literal contains a '.' (all real
 *                 RPC method names do) is a ledgered method. `EVENT_METHOD`
 *                 lives outside the block and the i32 error-code consts carry
 *                 no `&str`, so neither needs a special case — but both stay
 *                 excluded even if moved, via the block boundary + literal
 *                 shape.
 *   INTERFACES.md → §2 only (from the `## 2.` heading to the `## 3.` one),
 *                 and only TABLE ROWS (lines starting with `|`): the ledger
 *                 is the method tables, not prose. Backtick-quoted dotted
 *                 names that look like methods (`<word>.<word>…`, later
 *                 segments case-sensitive — `core.feeQuote`, `risk.setExit`).
 *                 Table-row restriction also keeps the extraction immune to
 *                 ``` code fences, whose unpaired backticks would flip the
 *                 quoting parity for everything after them (first draft bug:
 *                 `net.check` and the whole §2.9.1/2.9.3 tables vanished).
 *                 Anything outside §2 (examples, §4 prose) cannot satisfy the
 *                 gate on purpose: a method that left §2 IS undocumented.
 *
 * Usage:
 *   node scripts/interfaces-methods-check.mjs            # verdict
 *   node scripts/interfaces-methods-check.mjs --teeth    # must go red (and does, itself)
 *
 * Exit: 0 ledger complete · 1 methods missing (or teeth failed to redden) ·
 *       2 environment error (a source file unreadable)
 */

import { readFileSync } from 'fs';
import { dirname, join, resolve } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SCHEMA = join(ROOT, 'core', 'blitzkrieg_core', 'src', 'ipc', 'schema.rs');
const DOC = join(ROOT, 'docs', 'rust-core', 'INTERFACES.md');

const TEETH = process.argv.includes('--teeth');

// ── extraction: schema.rs ────────────────────────────────────────────────────

/** Every method name constant inside `pub mod method`, in declaration order. */
function schemaMethods(source) {
  const start = source.indexOf('pub mod method {');
  if (start === -1) throw new Error('schema.rs: no `pub mod method {` block found');
  // Block boundary: the matching close of the module — the next line that is
  // exactly `}` at column 0 after the block starts. The module holds only
  // consts and doc comments, never a nested brace column-0 line.
  const end = source.indexOf('\n}', start);
  if (end === -1) throw new Error('schema.rs: `pub mod method` block never closes');
  const block = source.slice(start, end);
  const methods = [];
  const re = /pub const [A-Z0-9_]+\s*:\s*&str\s*=\s*"([^"]+)"/g;
  for (let m; (m = re.exec(block)) !== null; ) {
    const name = m[1];
    // A real RPC method is dotted ("orders.place"). Excludes nothing today
    // (every constant in the block is a dotted method) but keeps the gate
    // honest if a non-method string constant ever moves in.
    if (name.includes('.')) methods.push(name);
  }
  if (methods.length === 0) throw new Error('schema.rs: zero method constants extracted');
  return methods;
}

// ── extraction: INTERFACES.md §2 ─────────────────────────────────────────────

// Dotted, method-shaped: first segment lowercase snake (the family), later
// segments allow mixed case — the wire's own spellings `core.feeQuote`,
// `risk.setExit`, `blueprint.loadSource` are all legal and must extract.
const METHOD_LIKE = /^[a-z][a-z0-9_]*(?:\.[a-zA-Z][a-zA-Z0-9_]*)+$/;

/**
 * §2 family names — the FIRST segment of every ledgered method (`orders`,
 * `account`, `shadow_evolution`, …). A §2 table row quoting a dotted name in
 * a family the schema never defines is a ghost (a typo nobody compiled
 * against); the gate refuses to be greened by one.
 */
function schemaFamilies(methods) {
  return new Set(methods.map((name) => name.slice(0, name.indexOf('.'))));
}

/**
 * Every method-shaped quoted token in the FIRST COLUMN of §2's table rows
 * (from `## 2.` to the next `## `, lines starting with `|`). First column
 * only — that column IS the method cell (`| method | params | result |`);
 * prose in the params/result columns quotes field paths (`blocked.byStrategy`)
 * and file names (`summary.json`) that are not methods and must not satisfy —
 * or fail — the ledger. First-column extraction is also immune to ``` code
 * fences, whose unpaired backticks would flip the quoting parity for
 * everything after them (first draft bug: `net.check` and the whole
 * §2.9.1/2.9.3 tables vanished).
 */
function documentedMethods(docSource) {
  const section = docSource.match(/^## 2\..*?$/m);
  if (!section) throw new Error('INTERFACES.md: no `## 2.` section found');
  const from = section.index;
  const next = docSource.slice(from + 1).match(/^## /m);
  const body = docSource.slice(from, next ? from + 1 + next.index : undefined);
  const methods = new Set();
  for (const line of body.split('\n')) {
    if (!line.startsWith('|')) continue;
    // Escaped pipes (`\|` inside a cell) must not shift the column split.
    const firstCell = line.replace(/\\\|/g, '\u0000').split('|')[1] ?? '';
    for (const m of firstCell.matchAll(/`([^`]+)`/g)) {
      const token = m[1];
      if (METHOD_LIKE.test(token)) methods.add(token);
    }
  }
  return methods;
}

// ── verdict ──────────────────────────────────────────────────────────────────

function diff(schemaSrc, docSrc) {
  const inSchema = schemaMethods(schemaSrc);
  const families = schemaFamilies(inSchema);
  const inDoc = documentedMethods(docSrc);
  const missing = inSchema.filter((name) => !inDoc.has(name));
  const ghosts = [...inDoc].filter((name) => !families.has(name.slice(0, name.indexOf('.'))));
  return { inSchema, inDoc, missing, ghosts };
}

function run(schemaSrc, docSrc) {
  const { inSchema, inDoc, missing, ghosts } = diff(schemaSrc, docSrc);
  console.error(`  schema.rs 方法常量: ${inSchema.length}`);
  console.error(`  INTERFACES.md §2 已记载: ${inDoc.size}`);
  if (missing.length === 0 && ghosts.length === 0) {
    console.error('  PASS 契约账本完整 —— schema.rs 的每个方法在 §2 都有表格行，且无幽灵行');
    return 0;
  }
  let code = 1;
  if (missing.length > 0) {
    console.error(`  RED §2 缺 ${missing.length} 个方法（schema.rs 有、文档无）:`);
    for (const name of missing) console.error(`    - ${name}`);
    console.error('  在 docs/rust-core/INTERFACES.md §2 补表格行（方法名/参数/响应/权限类别）后再合并。');
  }
  if (ghosts.length > 0) {
    console.error(`  RED §2 有 ${ghosts.length} 个幽灵行（schema.rs 无此方法族）:`);
    for (const name of ghosts) console.error(`    - ${name}`);
    console.error('  删除或更正 §2 的这些行 —— 账本不能记载实现里不存在的方法。');
    code = 1;
  }
  return code;
}

function fail(env, message) {
  console.error(`  environment: ${message}`);
  process.exit(env);
}

try {
  const schemaSrc = readFileSync(SCHEMA, 'utf8');
  const docSrc = readFileSync(DOC, 'utf8');

  if (TEETH) {
    // Reverse acceptance: the gate must go RED on the defect it guards. One
    // synthetically added method constant (never written to disk) must flip
    // the verdict; the unmutated pair must stay green. In-memory only — the
    // working tree is never touched.
    const MUTANT = 'pub const TEETH_NOT_A_REAL_METHOD: &str = "teeth.not_a_real_method";';
    const mutated = schemaSrc.replace('pub mod method {', `pub mod method {\n    ${MUTANT}`);
    if (mutated === schemaSrc) fail(2, 'teeth: could not inject the mutant constant');
    const red = run(mutated, docSrc);
    const green = run(schemaSrc, docSrc);
    if (red !== 1) {
      console.error('  TEETH FAILED: the mutated tree did not turn red — the gate cannot detect drift.');
      process.exit(1);
    }
    if (green !== 0) {
      console.error('  TEETH FAILED: the real tree is not green — fix the ledger first (this is the gate talking).');
      process.exit(1);
    }
    console.error('  PASS teeth: 注入一个未记载方法即红、现状全绿 —— 门禁活着');
    process.exit(0);
  }

  process.exit(run(schemaSrc, docSrc));
} catch (e) {
  fail(2, e.message);
}
