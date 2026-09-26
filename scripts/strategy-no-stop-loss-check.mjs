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
 *   node scripts/strategy-no-stop-loss-check.mjs --self-test # fixtures, no tree
 *   node scripts/strategy-no-stop-loss-check.mjs --teeth     # must go red
 * Exit: 0 pass / 1 verdict failure / 2 usage.
 */

import { readdirSync, readFileSync } from 'fs';
import { join } from 'path';
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

function scopeFiles() {
  const files = [];
  for (const [dir, family] of SCOPES) {
    const base = join(ROOT, dir);
    let walk;
    try {
      walk = (d) => {
        for (const e of readdirSync(d, { withFileTypes: true })) {
          const p = join(d, e.name);
          if (e.isDirectory()) {
            if (e.name === 'target') continue; // build output, not source
            walk(p);
          } else if (EXTS[family].some((x) => e.name.endsWith(x))) {
            files.push({ path: p, rel: `${dir}/${p.slice(base.length + 1)}`, family });
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

function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  if (argv.includes('--teeth')) return teeth();

  const files = scopeFiles();
  let hits = 0;
  for (const f of files) {
    for (const h of scanText(readFileSync(f.path, 'utf8'), f.family)) {
      hits += 1;
      console.log(`  ${h.rule} @ ${f.rel}:${h.line}: ${h.snippet}`);
    }
  }
  console.log(`no-stop-loss: scanned ${files.length} strategy source file(s) across ${SCOPES.length} scopes`);
  if (hits > 0) {
    console.error(
      `\nRESULT: FAIL — ${hits} stop-loss hit(s) in strategy sources.\n` +
        'A strategy computes no stop and monitors no stop; the kernel owns exits\n' +
        '(STRATEGY_GUIDE「止损不归你管」). Move the logic out of the strategy.',
    );
    process.exit(1);
  }
  console.log('RESULT: PASS — zero stop-loss logic, zero reserved intent keys in strategy sources');
}

main();
