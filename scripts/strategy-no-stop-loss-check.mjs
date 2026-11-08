#!/usr/bin/env node
/**
 * strategy:no-stop-loss-check — DEV_V0_3 §16.4 gate 1 (E24 / #330).
 *
 * The contract is STRATEGY_GUIDE's "止损不归你管": a strategy computes no
 * stop, monitors no stop, and expresses no stop — survival belongs to the
 * kernel (exit_policy + position). The other two layers of the seal live in
 * the kernel: `foreign.rs` warn!s and DROPS the three reserved intent keys
 * (reverse acceptance B), and this gate is the static third layer (§2.4
 * step 3): a text scan over the strategy sources.
 *
 * LITERAL SCOPE — what a PASS does and does not mean (honest statement):
 *   R1/R2 are literal text patterns. A PASS certifies "no stop-named binding
 *   and no reserved intent key in the scanned sources", NOT "no stop-loss
 *   logic exists". A real hard stop whose identifiers avoid `stop`/止损
 *   evades R2 by naming alone — known instances: lua_momentum binds
 *   `sl = dec_param(params, "stop_loss_pct", …)` (a protective stop
 *   sanctioned by the operator, ruling dated 2026-11-06), and #999 shipped
 *   coin_flip_exit_lab's hard stop as DEFAULT_HALT/halt (reverted in the
 *   #999 follow-up). Whether this gate should grow a semantic detector
 *   (call-shape / reason-string scan) is an OPERATOR ruling, not a gate
 *   maintainer's: it would also red the sanctioned lua_momentum stop. Until
 *   that ruling, this header is the gate's honest claim.
 *
 * lab_only exemption (#999 follow-up — no more identifier-naming luck): a
 *   package whose manifest.json declares "lab_only": true (strict boolean)
 *   is SKIPPED with a loud per-package skip line (package name + reason)
 *   that the summary repeats. The flag is the operator's explicit
 *   "replay/research lab, never a live default config" declaration; the
 *   gate does NOT certify a skipped package and does not rule on any
 *   stop-loss logic inside it. No flag = no skip: renaming
 *   DEFAULT_STOP→DEFAULT_HALT no longer flips any verdict here.
 *
 * Scan scope — the trees a strategy author ships (.rs/.lua):
 *   user_layer/strategies/**, user_layer/strategies_lua/** (lands with E30),
 *   user_layer/examples/**, user_layer/parity_strategy/**
 * Deliberately NOT scanned, with reasons:
 *   * user_layer/strategy_logic — kernel-LINKED shared decision library
 *     (blitzkrieg-core depends on it); its contract already pins
 *     `hard_stop_loss_pct = 0 // not ours`. Not strategy source.
 *   * user_layer/configs — kernel exit config; `exit.stop_loss_pct` there is
 *     the kernel legally owning stops.
 *   * user_layer/strategy_api — the interface crate itself.
 *
 * Rules, applied to comment-stripped text (a comment may DISCUSS the
 * boundary — the reference example's header does; strings are INCLUDED,
 * because a stop smuggled into an `exits` reason is still a stop):
 *   R1  reserved keys   /suggested_(stop_loss|take_profit|max_hold_sec)/
 *   R2  stop binding    an identifier containing `stop` bound with `=`
 *                       (never `==`/`<=`/`>=`/`!=`), or 止损… bound with
 *                       `=`/`：` — this is the shape of
 *                       `local stop = price * 0.965`.
 *
 * Usage:
 *   node scripts/strategy-no-stop-loss-check.mjs             # the real verdict
 *   node scripts/strategy-no-stop-loss-check.mjs --self-test # fixtures + lab_only sandbox, no repo tree
 *   node scripts/strategy-no-stop-loss-check.mjs --teeth     # must go red
 * Exit: 0 pass / 1 verdict failure / 2 usage.
 */

import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { basename, dirname, join, relative, sep } from 'path';
import { tmpdir } from 'os';
import { fileURLToPath } from 'url';

const ROOT = join(fileURLToPath(import.meta.url), '..', '..');

// (repo-relative directory, family) — family picks the comment stripper.
const SCOPES = [
  ['user_layer/strategies', 'rust'],
  ['user_layer/parity_strategy', 'rust'],
  ['user_layer/examples', 'rust'],
  ['user_layer/strategies_lua', 'lua'],
];
const EXTS = { rust: ['.rs'], lua: ['.lua'] };

const RESERVED_KEYS = /suggested_(stop_loss|take_profit|max_hold_sec)/;
// `stop…` identifier directly bound (`=` but not `==`/`<=`/`>=`/`!=`), and the
// Chinese binding shape (`止损… =`). Prose like "不计算止损" has no binding
// operator and survives; "止损不归你管" survives; `p.set("hard_stop_loss_pct",
// dec!(0))` survives (the token ends at a quote, no `=` follows it).
const STOP_BINDING = [
  [/[A-Za-z0-9_]*stop[A-Za-z0-9_]*\s*=(?!=)/i, 'stop-loss logic found'],
  [/止损[\u4e00-\u9fff]{0,6}\s*[:：=＝]/, 'stop-loss logic found'],
];

/** Strip line/block comments (not string literals) so prose can never trip R2. */
function stripComments(text, family) {
  const lineMark = family === 'lua' ? '--' : '//';
  const out = [];
  let block = false; // inside /* … */ (rust only)
  for (const raw of text.split('\n')) {
    let line = raw;
    if (family === 'rust') {
      if (block) {
        const end = line.indexOf('*/');
        if (end < 0) { out.push(''); continue; }
        line = line.slice(end + 2);
        block = false;
      }
      const open = line.indexOf('/*');
      if (open >= 0 && line.indexOf('*/', open + 2) < 0) block = true;
    }
    const cut = line.indexOf(lineMark);
    out.push(cut >= 0 ? line.slice(0, cut) : line);
  }
  return out;
}

/** All hits in one file's text: { rule, line (1-based), snippet } */
export function scanText(text, family) {
  const hits = [];
  const lines = stripComments(text, family);
  lines.forEach((code, i) => {
    if (RESERVED_KEYS.test(code)) {
      hits.push({ rule: 'R1 reserved intent key', line: i + 1, snippet: code.trim().slice(0, 120) });
    }
    for (const [re, msg] of STOP_BINDING) {
      if (re.test(code)) {
        hits.push({ rule: `R2 ${msg}`, line: i + 1, snippet: code.trim().slice(0, 120) });
      }
    }
  });
  return hits;
}

function scopeFiles(root = ROOT, scopes = SCOPES) {
  const files = [];
  for (const [dir, family] of scopes) {
    const base = join(root, dir);
    let walk;
    try {
      walk = (d) => {
        for (const e of readdirSync(d, { withFileTypes: true })) {
          const p = join(d, e.name);
          if (e.isDirectory()) {
            if (e.name === 'target') continue; // build output, not source
            walk(p);
          } else if (EXTS[family].some((x) => e.name.endsWith(x))) {
            files.push({
              path: p,
              rel: `${dir}/${p.slice(base.length + 1)}`,
              family,
              scopeDir: dir,
              scopeBase: base,
            });
          }
        }
      };
      walk(base);
    } catch {
      // a scope that does not exist yet (strategies_lua lands with E30) is fine
    }
  }
  return files;
}

// ── lab_only exemption (#999 follow-up) ─────────────────────────────────────
// The nearest manifest.json walking UP from a file's directory (bounded at the
// scope root, inclusive) decides the package. Flag rules:
//   * "lab_only": true — STRICT boolean. "true" (string) or 1 is NOT a skip:
//     no flag = no skip, and a near-miss flag must not silently widen it.
//   * An absent or unparseable manifest is NO flag — the files stay under the
//     scan (fail-closed for a text scanner; malformed manifests are the Lua
//     loader's own gate territory).
//   * The NEAREST manifest wins, so an inner package with its own manifest is
//     judged by its own flag, not the outer one.
const manifestCache = new Map(); // scopeBase\0dir → parsed manifest object | null

function labOnlyInfo(f) {
  let dir = dirname(f.path);
  for (;;) {
    const key = `${f.scopeBase}\u0000${dir}`;
    let manifest = manifestCache.get(key);
    if (manifest === undefined) {
      try {
        const parsed = JSON.parse(readFileSync(join(dir, 'manifest.json'), 'utf8'));
        manifest = parsed && typeof parsed === 'object' ? parsed : null;
      } catch {
        manifest = null; // no manifest.json here, or unparseable → no flag
      }
      manifestCache.set(key, manifest);
    }
    if (manifest) {
      if (manifest.lab_only !== true) return null;
      const rel = relative(f.scopeBase, dir);
      const pkgRel = [f.scopeDir, ...rel.split(sep)].filter(Boolean).join('/');
      const name =
        typeof manifest.name === 'string' && manifest.name ? manifest.name : pkgRel || basename(dir);
      return { name, pkgRel };
    }
    if (dir === f.scopeBase) return null; // scope root reached — no package above it
    dir = dirname(dir);
  }
}

function selfTest() {
  const clean = [
    ['a clean reference strategy passes', 'rust', 'fn evaluate(&mut self, _c: &RoundContext) -> Intents { Intents::none() }\n'],
    ['prose about the boundary in comments passes', 'rust',
      '//! 读 STRATEGY_GUIDE 的「止损不归你管」章节：不计算止损、不监视止损、不表达止损。\nlet shares = None;\n'],
    ['the not-ours disclaimer shape passes', 'rust',
      'p.set("hard_stop_loss_pct", dec!(0)); // not ours\n'],
    ['a Lua strategy with only entries passes', 'lua',
      'local band = 0.62\nif mid < band then bk.entry(token, price, "band") end\n'],
  ];
  const dirty = [
    ['the injected teeth (§16.6) is caught', 'rust',
      'let exits = vec![Exit { token, reason: "hit" }];\nlocal stop = price * 0.965\n', 'stop-loss logic found'],
    ['a stop binding in Rust is caught', 'rust', 'let stop_price = entry * dec!(0.965);\n', 'stop-loss logic found'],
    ['a stop smuggled into an exits reason is caught', 'rust',
      'reason: format!("momentum died, stop = {} hit", price)', 'stop-loss logic found'],
    ['a reserved intent key is caught', 'rust',
      'let payload = json!({"token": t, "suggested_stop_loss": "0.30"});', 'R1 reserved intent key'],
  ];
  let bad = 0;
  for (const [name, family, text] of clean) {
    const hits = scanText(text, family);
    if (hits.length > 0) { bad += 1; console.error(`  FAIL ${name} — flagged ${JSON.stringify(hits)}`); }
    else console.log(`  ok   ${name}`);
  }
  for (const [name, family, text, want] of dirty) {
    const hits = scanText(text, family);
    const ok = hits.some((h) => h.rule.includes(want));
    if (!ok) { bad += 1; console.error(`  FAIL ${name} — scanner saw nothing`); }
    else console.log(`  ok   ${name}`);
  }
  if (bad > 0) { console.error(`\nno-stop-loss self-test: ${bad} fixture(s) failed`); process.exit(1); }
  console.log(`\nno-stop-loss self-test: ${clean.length + dirty.length} fixtures passed`);

  labOnlySandboxTest();
}

// The lab_only exemption exercised through the REAL scan path (scanTree over a
// temp sandbox, not a stub): the lab package's skip line must print loudly and
// reach the summary; the unflagged package with the SAME stop binding must be
// flagged (no flag = no skip — renaming stop→halt buys nothing here).
function labOnlySandboxTest() {
  const root = mkdtempSync(join(tmpdir(), 'no-stop-loss-lab-'));
  const scopes = [['user_layer/strategies_lua', 'lua']];
  const write = (pkg, manifest, strategy) => {
    const dir = join(root, 'user_layer', 'strategies_lua', pkg);
    mkdirSync(dir, { recursive: true });
    if (manifest !== null) writeFileSync(join(dir, 'manifest.json'), manifest);
    writeFileSync(join(dir, 'strategy.lua'), strategy);
  };
  const stopBinding = 'local DEFAULT_STOP = { m = 20, s = 0 }\nlocal stop = dec_param(params, "hard_stop_loss_pct", DEFAULT_STOP)\n';
  write('flip_lab', JSON.stringify({ name: 'flip_lab', lab_only: true }), '-- lab package with a real hard stop\n' + stopBinding);
  write('plain_pkg', JSON.stringify({ name: 'plain_pkg' }), '-- unflagged package, same stop\n' + stopBinding);
  write('stringy_flag', JSON.stringify({ name: 'stringy_flag', lab_only: 'true' }), '-- "true" is not true: must scan\n' + stopBinding);
  try {
    const { skipped, hits } = scanTree(scopeFiles(root, scopes));
    let bad = 0;
    const fail = (m) => { bad += 1; console.error(`  FAIL ${m}`); };
    if (skipped.length !== 1 || skipped[0].name !== 'flip_lab') {
      fail(`lab_only skip: expected exactly [flip_lab], got ${JSON.stringify(skipped.map((s) => s.name))}`);
    } else if (!/lab_only/.test(skipped[0].reason)) {
      fail(`lab_only skip reason must name the flag: ${skipped[0].reason}`);
    } else {
      console.log(`  ok   lab_only skip is loud: [${skipped[0].name}] ${skipped[0].reason}`);
    }
    const flagged = hits.filter((h) => h.rel.includes('plain_pkg')).length;
    if (flagged < 1) fail('unflagged package with the same stop binding was NOT flagged (no flag = no skip violated)');
    else console.log(`  ok   unflagged package with the same stop binding still flagged (${flagged} hit(s))`);
    const stringy = hits.filter((h) => h.rel.includes('stringy_flag')).length;
    if (stringy < 1) fail('lab_only: "true" (string) wrongly treated as a skip — no flag = no skip violated');
    else console.log('  ok   lab_only:"true" (string) is NOT a skip — strict-boolean flag enforced');
    if (bad > 0) { console.error(`\nno-stop-loss self-test: lab_only sandbox ${bad} case(s) failed`); process.exit(1); }
    console.log('no-stop-loss self-test: lab_only sandbox cases passed');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

const TEETH_SOURCE = [
  '// --teeth: the bad implementation §16.6 row 1 demands the gate catch.',
  'local stop = price * 0.965',
  'table.insert(out.exits, { token = t, reason = "stop = price * 0.965 hit" })',
].join('\n');

function teeth() {
  // Feed the BAD implementation to the judge and EXPECT failure (§A.4 step 3):
  // the injected source must be flagged with the §16.6 message, and a gate
  // that stays green here is toothless.
  const hits = scanText(TEETH_SOURCE, 'lua');
  const ok = hits.some((h) => h.rule.includes('stop-loss logic found'));
  console.log(`${ok ? '  ok  ' : '  FAIL'} teeth: injected "local stop = price * 0.965" into an exits reason`);
  if (ok) for (const h of hits) console.log(`       ${h.rule} @ line ${h.line}: ${h.snippet}`);
  if (ok) {
    console.log('\nno-stop-loss --teeth: the bad implementation was caught — the gate has teeth');
    process.exit(0);
  }
  console.error('\nno-stop-loss --teeth: the injected stop-loss logic was NOT flagged — the gate has no teeth');
  process.exit(1);
}

// One pass over the scope files: every hit printed LOUDLY as it is found
// (rule @ file:line), every lab_only package skipped with a loud line.
// Returns { hits, skipped } so --self-test can drive the real verdict path.
function scanTree(files) {
  const hits = [];
  const skipped = [];
  for (const f of files) {
    const lab = labOnlyInfo(f);
    if (lab) {
      const reason = `lab_only — manifest.json declares "lab_only": true (operator-declared replay/research lab); gate does NOT certify this package`;
      const line = `  SKIP ${lab.name} (${f.scopeDir}/…): ${reason}`;
      console.log(line);
      skipped.push({ name: lab.name, pkgRel: lab.pkgRel, reason, line });
      continue;
    }
    for (const h of scanText(readFileSync(f.path, 'utf8'), f.family)) {
      console.log(`  ${h.rule} @ ${f.rel}:${h.line}: ${h.snippet}`);
      hits.push({ ...h, rel: f.rel });
    }
  }
  return { hits, skipped };
}

function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  if (argv.includes('--teeth')) return teeth();

  const files = scopeFiles();
  const { hits, skipped } = scanTree(files);
  console.log(
    `no-stop-loss: scanned ${files.length - skipped.length} strategy source file(s) ` +
      `across ${SCOPES.length} scopes` +
      (skipped.length ? `; ${skipped.length} lab_only package(s) SKIPPED` : ''),
  );
  if (skipped.length) {
    for (const s of skipped) console.log(`  lab_only skip in summary: ${s.name} (${s.pkgRel}) — ${s.reason}`);
  }
  if (hits.length > 0) {
    console.error(
      `\nRESULT: FAIL — ${hits.length} stop-loss hit(s) in strategy sources.\n` +
        'A strategy computes no stop and monitors no stop; the kernel owns exits\n' +
        '(STRATEGY_GUIDE「止损不归你管」). Move the logic out of the strategy.',
    );
    process.exit(1);
  }
  console.log('RESULT: PASS — zero stop-loss logic, zero reserved intent keys in strategy sources');
}

main();
