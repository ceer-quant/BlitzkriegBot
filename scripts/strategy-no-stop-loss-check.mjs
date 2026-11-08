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
 * THREE TIERS — the operator's declared stop-loss ruling (2026-11-08) re-reads
 * E24's intent: not "zero stop-loss" but "DECLARED stop-loss only". The
 * nearest manifest.json walking UP from a file's directory picks the tier
 * (strict flags; an absent or unparseable manifest is the default tier):
 *   none (default, implicit)  no declaration → the literal scan below is the
 *                             whole verdict: any stop-named binding = FAIL.
 *   declared                  `"stop_loss_policy": "declared"` (EXACT string)
 *                             + `"stop_loss_knobs": ["stop_loss_pct", …]`
 *                             (non-empty array of non-empty names): the scan
 *                             still runs everywhere, but an R2 stop binding
 *                             whose right-hand side is EXACTLY
 *                             `dec_param(params, "<declared knob>", …)` is
 *                             ALLOWED — loudly: a per-package DECLARED line
 *                             (name + knob list + ruling reference) prints
 *                             and the summary repeats it. Every allowed
 *                             binding also prints its own line. Hardcoded
 *                             stop math, other knobs, and arithmetic layered
 *                             on top of the knob call stay RED.
 *   lab                       `"lab_only": true` (STRICT boolean) → loud
 *                             SKIP, no certification (#1006 mechanism
 *                             unchanged; lab outranks declared if both are
 *                             declared — never certify a lab package).
 * ALWAYS RED in EVERY tier:
 *   * R1 reserved intent keys (/suggested_(stop_loss|take_profit|
 *     max_hold_sec)/) — absolute, never exempt;
 *   * hardcoded stop math — a numeric stop floor computed from literals
 *     (the `price * 0.965` shape) never becomes legal by declaring a policy;
 *   * undeclared knob names used as a stop source — the declared list is the
 *     whole authority; naming games and tier games buy nothing.
 *
 * What a PASS certifies per tier (honest claim): a none-tier package has no
 * stop-named binding and no reserved key; a declared package additionally
 * shows every knob-bound dec_param binding it carries, and nothing else
 * stop-shaped; a lab package is skipped unseen. The scan is still LITERAL
 * text matching — in a none-tier package a stop whose identifiers avoid
 * `stop`/止损 still evades R2 by naming alone (the loader's sha256 gate and
 * the kernel's foreign.rs remain the other two layers of the seal).
 *
 * Endgame (documented direction, NOT implemented here): stop logic belongs in
 * the kernel. The E26 READOUT precedent (apply_physics binds stop/force-exit
 * from the kernel-side READOUT) is the shape — when stop_loss_pct becomes a
 * kernel param, this tier system retires.
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
 *                       Declared-tier exemption: R2's binding operator also
 *                       splits off the right-hand side, and ONLY the exact
 *                       double-quoted `dec_param(…, "<declared knob>", …)`
 *                       call consuming the whole RHS is exemptible — any
 *                       other shape stays RED.
 *
 * Usage:
 *   node scripts/strategy-no-stop-loss-check.mjs             # the real verdict
 *   node scripts/strategy-no-stop-loss-check.mjs --self-test # fixtures + lab_only/declared sandboxes, no repo tree
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

// ── declared-tier R2 exemption (operator ruling 2026-11-08) ─────────────────
// The ONLY exemptible R2 shape: the binding's whole right-hand side is ONE
// `dec_param(<params>, "<knob>", …)` call whose second argument is a
// double-quoted string literal naming a DECLARED knob. Everything else —
// literal math (`price * 0.965`), an undeclared knob, arithmetic layered on
// top of the call, a knob read some other way — stays RED. Fail-closed: an
// unusual shape is not an exemption, it is a finding.
function declaredKnobDecParam(rhs, knobs) {
  const head = /^dec_param\s*\(\s*[A-Za-z_][A-Za-z0-9_.]*\s*,\s*"([^"]*)"\s*,/.exec(rhs);
  if (!head || !knobs.has(head[1])) return null;
  // The call must CONSUME the whole RHS: walk to the balanced close paren
  // (skipping string literals) and allow only whitespace after it.
  let depth = 0;
  let i = rhs.indexOf('(');
  for (; i < rhs.length; i++) {
    const c = rhs[i];
    if (c === '"' || c === "'") {
      const q = c;
      i++;
      while (i < rhs.length && rhs[i] !== q) { if (rhs[i] === '\\') i++; i++; }
      continue;
    }
    if (c === '(') depth++;
    else if (c === ')') { depth--; if (depth === 0) break; }
  }
  if (depth !== 0 || i >= rhs.length) return null;
  if (rhs.slice(i + 1).trim() !== '') return null;
  return head[1];
}

/**
 * All hits in one file's text: { rule, line (1-based), snippet }.
 * `exempt` (optional) is the declared-tier authority `{ knobs: Set<string> }`:
 * R2 hits whose RHS is exactly a declared-knob dec_param call come back with
 * `allowed: true, knob` — still reported, never silent, never counted as a
 * verdict hit. R1 is NEVER exempt, any tier.
 */
export function scanText(text, family, exempt = null) {
  const hits = [];
  const lines = stripComments(text, family);
  lines.forEach((code, i) => {
    if (RESERVED_KEYS.test(code)) {
      hits.push({ rule: 'R1 reserved intent key', line: i + 1, snippet: code.trim().slice(0, 120) });
    }
    for (const [re, msg] of STOP_BINDING) {
      const m = re.exec(code);
      if (m) {
        if (exempt) {
          const knob = declaredKnobDecParam(code.slice(m.index + m[0].length).trim(), exempt.knobs);
          if (knob) {
            hits.push({ rule: `R2 ${msg}`, line: i + 1, snippet: code.trim().slice(0, 120), allowed: true, knob });
            continue;
          }
        }
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

// ── per-package tier resolution (#1006 walk, generalized to both flags) ─────
// The nearest manifest.json walking UP from a file's directory (bounded at the
// scope root, inclusive) decides the package — ONE walk reads lab_only AND the
// stop-loss declaration. Flag rules (both strict, fail-closed):
//   * "lab_only": true — STRICT boolean. "true" (string) or 1 is NOT a skip.
//   * "stop_loss_policy": "declared" — EXACT string, and "stop_loss_knobs"
//     must be a non-empty array of non-empty strings. A near-miss spelling
//     ("Declared", a bare string knob list, an empty array) is NOT a
//     declaration: the package stays on the default (none) tier.
//   * An absent or unparseable manifest is NO flag — default tier, fail-closed
//     (malformed manifests are the Lua loader's own gate territory).
//   * The NEAREST manifest wins, so an inner package with its own manifest is
//     judged by its own flags, not the outer one.
const manifestCache = new Map(); // scopeBase\0dir → parsed manifest object | null

function packageFlags(f) {
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
      const rel = relative(f.scopeBase, dir);
      const pkgRel = [f.scopeDir, ...rel.split(sep)].filter(Boolean).join('/');
      const name =
        typeof manifest.name === 'string' && manifest.name ? manifest.name : pkgRel || basename(dir);
      const labOnly = manifest.lab_only === true;
      const knobsRaw = manifest.stop_loss_knobs;
      const knobsOk =
        Array.isArray(knobsRaw) &&
        knobsRaw.length > 0 &&
        knobsRaw.every((k) => typeof k === 'string' && k !== '');
      const declared = manifest.stop_loss_policy === 'declared' && knobsOk;
      const knobs = declared ? [...new Set(knobsRaw)] : null;
      const ruling =
        typeof manifest.stop_loss_ruling === 'string' && manifest.stop_loss_ruling
          ? manifest.stop_loss_ruling
          : '';
      return { name, pkgRel, labOnly, declared, knobs, ruling };
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
  declaredTierSandboxTest();
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

// The declared tier exercised through the REAL scan path (scanTree over a temp
// sandbox): the operator ruling's five cases plus two strictness near-misses.
//   (a) declared + knob-source binding  → allowed + loud DECLARED line
//   (b) declared + hardcoded stop math  → RED
//   (c) declared + UNDECLARED knob      → RED
//   (d) undeclared + knob-source binding → RED (tier games buy nothing)
//   (e) R1 reserved key in a declared package → RED (absolute, never exempt)
function declaredTierSandboxTest() {
  const root = mkdtempSync(join(tmpdir(), 'no-stop-loss-declared-'));
  const scopes = [['user_layer/strategies_lua', 'lua']];
  const write = (pkg, manifest, strategy) => {
    const dir = join(root, 'user_layer', 'strategies_lua', pkg);
    mkdirSync(dir, { recursive: true });
    if (manifest !== null) writeFileSync(join(dir, 'manifest.json'), manifest);
    writeFileSync(join(dir, 'strategy.lua'), strategy);
  };
  const ruling = 'operator ruling 2026-11-08: sandbox fixture';
  const declared = JSON.stringify({
    name: 'PLACEHOLDER',
    stop_loss_policy: 'declared',
    stop_loss_knobs: ['stop_loss_pct'],
    stop_loss_ruling: ruling,
  });
  const knobBinding = 'local DEFAULT_SL = { m = 50, s = 0 }\nlocal stop = dec_param(params, "stop_loss_pct", DEFAULT_SL)\n';
  const dm = (pkg, extra = {}) =>
    JSON.stringify({ name: pkg, stop_loss_policy: 'declared', stop_loss_knobs: ['stop_loss_pct'], stop_loss_ruling: ruling, ...extra });
  write('declared_ok', dm('declared_ok'), '-- (a) declared, knob-source binding\n' + knobBinding);
  write('declared_hardcoded', dm('declared_hardcoded'), '-- (b) declared, hardcoded stop math\nlocal stop = price * 0.965\n');
  write('declared_other_knob', dm('declared_other_knob'), '-- (c) declared, UNDECLARED knob as stop source\nlocal DEFAULT_X = { m = 5, s = 0 }\nlocal stop = dec_param(params, "secret_sl_pct", DEFAULT_X)\n');
  write('plain_knob_pkg', JSON.stringify({ name: 'plain_knob_pkg' }), '-- (d) undeclared, knob-source binding\n' + knobBinding);
  write('declared_r1_pkg', dm('declared_r1_pkg'), '-- (e) declared, R1 reserved key\nlocal payload = json.encode({ suggested_stop_loss = "0.30" })\n');
  // strictness near-misses: the exact spellings are the whole authority
  write('declared_casey', dm('declared_casey', { stop_loss_policy: 'Declared' }), '-- policy near-miss: must stay RED\n' + knobBinding);
  write('declared_stringy', dm('declared_stringy', { stop_loss_knobs: 'stop_loss_pct' }), '-- knobs near-miss (string): must stay RED\n' + knobBinding);
  try {
    const { hits, skipped, declared: declaredPkgs } = scanTree(scopeFiles(root, scopes));
    let bad = 0;
    const fail = (m) => { bad += 1; console.error(`  FAIL ${m}`); };
    const pkgHits = (pkg) => hits.filter((h) => h.rel.includes(pkg));
    const verdictHits = (pkg) => pkgHits(pkg).filter((h) => !h.allowed);

    // (a) allowed + DECLARED line
    if (verdictHits('declared_ok').length > 0) fail('(a) declared knob-source binding was flagged — exemption missing');
    else if (!pkgHits('declared_ok').some((h) => h.allowed)) fail('(a) declared knob-source binding produced no allowed-hit line');
    else console.log('  ok   (a) declared knob-source binding allowed, loud allowed line printed');
    const decA = declaredPkgs.find((d) => d.name === 'declared_ok');
    if (!decA) fail('(a) no DECLARED line for declared_ok');
    else if (!/declared/.test(decA.reason) || !/\[stop_loss_pct\]/.test(decA.reason) || !decA.ruling) {
      fail(`(a) DECLARED line incomplete: ${decA.reason}`);
    } else console.log(`  ok   (a) DECLARED line loud: ${decA.line.trim()}`);

    // (b) hardcoded math stays RED
    if (verdictHits('declared_hardcoded').length < 1) fail('(b) hardcoded stop math in a declared package was NOT flagged');
    else console.log(`  ok   (b) declared + hardcoded stop math stays RED (${verdictHits('declared_hardcoded').length} hit(s))`);

    // (c) undeclared knob stays RED
    if (verdictHits('declared_other_knob').length < 1) fail('(c) UNDECLARED knob as stop source in a declared package was NOT flagged');
    else console.log(`  ok   (c) declared + undeclared knob stays RED (${verdictHits('declared_other_knob').length} hit(s))`);

    // (d) tier games buy nothing
    if (verdictHits('plain_knob_pkg').length < 1) fail('(d) knob-source binding in an UNDECLARED package was NOT flagged');
    else console.log(`  ok   (d) undeclared + knob-source binding stays RED (${verdictHits('plain_knob_pkg').length} hit(s))`);

    // (e) R1 absolute
    const r1 = verdictHits('declared_r1_pkg').filter((h) => h.rule.includes('R1 reserved intent key'));
    if (r1.length < 1) fail('(e) R1 reserved key inside a declared package was NOT flagged');
    else console.log(`  ok   (e) R1 reserved key in a declared package stays RED (${r1.length} hit(s))`);

    // strictness near-misses
    if (verdictHits('declared_casey').length < 1) fail('strictness: policy "Declared" (wrong case) was treated as a declaration');
    else console.log('  ok   strictness: stop_loss_policy "Declared" (wrong case) is NOT a declaration — stays RED');
    if (verdictHits('declared_stringy').length < 1) fail('strictness: string stop_loss_knobs was treated as a declaration');
    else console.log('  ok   strictness: stop_loss_knobs as a bare string is NOT a declaration — stays RED');
    if (skipped.length > 0) fail(`no package in this sandbox should be skipped, got ${JSON.stringify(skipped.map((s) => s.name))}`);

    if (bad > 0) { console.error(`\nno-stop-loss self-test: declared-tier sandbox ${bad} case(s) failed`); process.exit(1); }
    console.log('no-stop-loss self-test: declared-tier sandbox cases passed');
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
  // that stays green here is toothless. No manifest context → no tier → no
  // exemption: the injected literal stop math must stay RED in every tier.
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
// (rule @ file:line), every lab_only package skipped with a loud line, every
// declared package announced with a loud DECLARED line (name + knobs + ruling)
// and every allowed knob-bound binding printed. Returns { hits, skipped,
// declared } so --self-test can drive the real verdict path.
function scanTree(files) {
  const hits = [];
  const skipped = [];
  const declared = [];
  const printedDeclared = new Set();
  for (const f of files) {
    const pkg = packageFlags(f);
    if (pkg && pkg.labOnly) {
      const reason = `lab_only — manifest.json declares "lab_only": true (operator-declared replay/research lab); gate does NOT certify this package`;
      const line = `  SKIP ${pkg.name} (${f.scopeDir}/…): ${reason}`;
      console.log(line);
      skipped.push({ name: pkg.name, pkgRel: pkg.pkgRel, reason, line });
      continue;
    }
    if (pkg && pkg.declared && !printedDeclared.has(pkg.pkgRel)) {
      printedDeclared.add(pkg.pkgRel);
      const knobsTxt = `[${pkg.knobs.join(', ')}]`;
      const rulingTxt = pkg.ruling || '(ruling reference missing)';
      const reason = `stop_loss_policy=declared, knobs=${knobsTxt} (${rulingTxt})`;
      const line = `  DECLARED ${pkg.name} (${pkg.pkgRel}): ${reason}`;
      console.log(line);
      declared.push({ name: pkg.name, pkgRel: pkg.pkgRel, knobs: pkg.knobs, ruling: pkg.ruling, reason, line });
    }
    const exempt = pkg && pkg.declared ? { knobs: new Set(pkg.knobs) } : null;
    for (const h of scanText(readFileSync(f.path, 'utf8'), f.family, exempt)) {
      if (h.allowed) {
        const line = `  ok declared binding allowed (knob "${h.knob}" via dec_param) @ ${f.rel}:${h.line}: ${h.snippet}`;
        console.log(line);
        hits.push({ ...h, rel: f.rel, allowedLine: line });
      } else {
        console.log(`  ${h.rule} @ ${f.rel}:${h.line}: ${h.snippet}`);
        hits.push({ ...h, rel: f.rel });
      }
    }
  }
  return { hits, skipped, declared };
}

function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  if (argv.includes('--teeth')) return teeth();

  const files = scopeFiles();
  const { hits, skipped, declared } = scanTree(files);
  const verdictHits = hits.filter((h) => !h.allowed);
  const allowedCount = hits.length - verdictHits.length;
  console.log(
    `no-stop-loss: scanned ${files.length - skipped.length} strategy source file(s) ` +
      `across ${SCOPES.length} scopes` +
      (skipped.length ? `; ${skipped.length} lab_only package(s) SKIPPED` : '') +
      (declared.length ? `; ${declared.length} declared stop-loss package(s)` : '') +
      (allowedCount ? `; ${allowedCount} declared-knob binding(s) allowed` : ''),
  );
  if (skipped.length) {
    for (const s of skipped) console.log(`  lab_only skip in summary: ${s.name} (${s.pkgRel}) — ${s.reason}`);
  }
  if (declared.length) {
    for (const d of declared) console.log(`  DECLARED in summary: ${d.name} (${d.pkgRel}): ${d.reason}`);
  }
  if (verdictHits.length > 0) {
    console.error(
      `\nRESULT: FAIL — ${verdictHits.length} stop-loss hit(s) in strategy sources.\n` +
        'A strategy computes no stop and monitors no stop; the kernel owns exits\n' +
        '(STRATEGY_GUIDE「止损不归你管」). A declared tier exempts ONLY knob-bound\n' +
        'dec_param stop bindings — R1 reserved keys and hardcoded stop math are\n' +
        'never exempt, any tier. Move the logic out of the strategy or declare it.',
    );
    process.exit(1);
  }
  console.log('RESULT: PASS — zero undeclared stop-loss bindings, zero reserved intent keys in strategy sources');
}

main();
