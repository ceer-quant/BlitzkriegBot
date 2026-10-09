#!/usr/bin/env node
/**
 * no-venue-branch-check — issue #427 reverse acceptance (global source scan).
 *
 * The contract is "venue is data": any logic that BRANCHES on which platform
 * a leg came from is a static-venue kernel — adding Kalshi or predict.fun
 * support would mean re-editing trading code. The architecture answer is the
 * venue field on the data plane (#427 part 1): `CryptoMarket.venue`,
 * `MarketDescriptor.venue`, `UnifiedEvent.listings[Venue]` — data the
 * strategy layer reads, never a kernel `if`.
 *
 * Rule: outside the ALLOWLISTED seams, no trading-path source may name a
 * concrete venue token (`Polymarket` / `Kalshi` / `PredictFun` / their
 * snake_case plugin prefixes) in CODE (comments are stripped — prose may
 * discuss venues). A hit is a kernel `if kalshi` by another spelling.
 *
 * The allowlist is the architecture, spelled out:
 *   core/market_api/src/unified/**   — the venue-agnostic unified model
 *     (Venue enum, PlatformListing, matcher, ledger). It names every venue
 *     once as DATA; it never branches on one.
 *   core/blitzkrieg_core/src/market/mod.rs — the ONE registration seam
 *     (register_builtin_markets): the single place the binary assembles
 *     plugins, feature-gated. Everything else in core must not name one.
 *   extensions/**                    — each plugin IS one venue; naming
 *     itself is its whole job.
 *   tests/**, *_tests.rs, test_fixtures, benches — fixtures construct
 *     venue-tagged data; that is the point.
 *   scripts/**                       — gate/tooling quoting the vocabulary.
 *   .md, .jsonl, .toml               — docs/config, not code.
 *
 * Everything else scanned (core trading path, strategy_logic, lua_runtime,
 * strategies, web, tui, strategy_api, onchain converter): a venue token in
 * code goes red.
 *
 * Usage:
 *   node scripts/no-venue-branch-check.mjs             # the real verdict
 *   node scripts/no-venue-branch-check.mjs --self-test # fixtures, no tree
 *   node scripts/no-venue-branch-check.mjs --teeth     # must go red
 * Exit: 0 pass / 1 verdict failure / 2 usage.
 */

import { readdirSync, readFileSync } from 'fs';
import { join, sep } from 'path';
import { fileURLToPath } from 'url';

const ROOT = join(fileURLToPath(import.meta.url), '..', '..');

// Code-bearing trees. Files outside these are not scanned (docs/config).
const SCOPES = [
  'core',
  'user_layer',
  'extensions',
  'web',
  'tui',
  'scripts',
];

const EXTS = ['.rs', '.lua', '.mjs', '.js', '.ts', '.vue'];

// A venue token in CODE. Leading \b only: no trailing boundary, because `_`
// is a word character and the plugin-prefix branch —
// `token.starts_with("polymarket_")`, `use kalshi_extension::…` — is exactly
// the `if kalshi` shape #427 outlaws in another spelling.
const VENUE_TOKEN = /\b(Polymarket|Kalshi|PredictFun|polymarket|kalshi|predictfun|predict_fun)/;

// What is outlawed is a BRANCH on a venue, not the word: a data field, a log
// line, a help page or a fee-source citation may NAME one (#427 targets
// `if kalshi` — decisions, not vocabulary). Each rule is a decision shape
// with a venue token in it:
const VEN = '(?:Polymarket|Kalshi|PredictFun|polymarket|kalshi|predictfun|predict_fun)';
// No trailing \b anywhere: `_` is a word character and `polymarket_` is the
// prefix-branch spelling. The leading \b comes from VENUE_TOKEN gating the
// line; these rules only classify HOW the token decides.
const BRANCH_SHAPES = [
  // guard keyword with a venue token on the same line:
  // `if venue == "kalshi"`, `if k == "polymarket" then`
  [new RegExp(`\\b(?:if|while|elseif|elsif)\\b[^\\n]*\\b${VEN}`), 'venue guard'],
  // dispatch: Rust match arm `Venue::Kalshi =>`, JS `case "polymarket":`
  [new RegExp(`Venue::(?:Polymarket|Kalshi|PredictFun)\\s*=>`), 'venue match arm'],
  [new RegExp(`\\bcase\\s*['"]?${VEN}`), 'venue match arm'],
  // equality/comparison, token on the right: `venue == "kalshi"`,
  // `!= Venue::Polymarket`, Lua `~= 'polymarket'`
  [new RegExp(`(?:==|!=|~=)\\s*(?:Venue::|['"]|&)?\\s*${VEN}`), 'venue comparison'],
  // token on the left: `"polymarket" == v` — the closing quote may sit
  // between the token and the operator.
  [new RegExp(`\\b${VEN}['"]?\\s*(?:==|!=|~=|\\.eq\\(|\\.ne\\()`), 'venue comparison'],
  // routing by venue identity: `starts_with("polymarket_")`,
  // `contains(&Venue::Kalshi)`
  [new RegExp(`\\b(?:starts_with|ends_with|startswith|endswith|contains|includes)\\s*\\(\\s*['"&]*\\s*(?:Venue::)?${VEN}`), 'venue routing'],
];

// Path (repo-relative, /-separated) rules where a venue token is legal —
// the data seams and the self-naming plugins. Everything else is red.
//   PREFIX entries: file or directory — everything under it is allowed.
//   SEGMENT entries: a `/…/` directory segment marking test code anywhere in
//     the tree (endsWith('/' + seg + '/') on the file's directory part).
const ALLOW_PREFIXES = [
  'core/market_api/src/unified/',
  'core/blitzkrieg_core/src/market/mod.rs',
  'extensions/',
  'scripts/', // this gate and its family quote the vocabulary
  // the onchain converter is a single-venue ARCHIVE tool (its whole contract
  // is "Polymarket wallet history → JSONL"); it produces data for the
  // venue-agnostic core and never trades.
  'core/blitzkrieg_core/src/onchain.rs',
];
const TEST_SEGMENTS = ['tests', 'benches'];

// Inline-test carve-out, matched on stripped code, not on file path: a
// `#[cfg(test)] mod tests { … }` block (and any `#[test]`-bearing module)
// constructs venue-tagged fixtures BY NAME — that is the point of a fixture,
// not a kernel branch. Production code between such blocks stays in scope.
const INLINE_TEST_MODULE = /#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*\n\s*mod\s+\w+\s*\{/;

// Comment strippers per extension family, so prose may discuss venues.
function stripComments(text, ext) {
  const lines = text.split('\n');
  const out = [];
  if (ext === '.rs') {
    let block = false;
    for (const raw of lines) {
      let line = raw;
      if (block) {
        const end = line.indexOf('*/');
        if (end < 0) { out.push(''); continue; }
        line = line.slice(end + 2);
        block = false;
      }
      const open = line.indexOf('/*');
      if (open >= 0 && line.indexOf('*/', open + 2) < 0) block = true;
      const cut = line.indexOf('//');
      out.push(cut >= 0 ? line.slice(0, cut) : line);
    }
    return out;
  }
  if (ext === '.lua') {
    for (const raw of lines) {
      const cut = raw.indexOf('--');
      out.push(cut >= 0 ? raw.slice(0, cut) : raw);
    }
    return out;
  }
  // js/mjs/ts/vue: `//` line comments, `/* */` blocks; keep strings (a venue
  // smuggled into a string is still a branch).
  let block = false;
  for (const raw of lines) {
    let line = raw;
    if (block) {
      const end = line.indexOf('*/');
      if (end < 0) { out.push(''); continue; }
      line = line.slice(end + 2);
      block = false;
    }
    const open = line.indexOf('/*');
    if (open >= 0 && line.indexOf('*/', open + 2) < 0) block = true;
    const cut = line.indexOf('//');
    out.push(cut >= 0 ? line.slice(0, cut) : line);
  }
  return out;
}

/** Drop every `#[cfg(test)] mod … {` block's body (brace-balanced), keeping
 * production code between blocks. Fixture code constructs venue-tagged data
 * by name — that is a fixture's job, not a kernel branch. */
function stripInlineTestBlocks(lines) {
  const text = lines.join('\n');
  const out = [];
  let at = 0;
  while (true) {
    const m = text.slice(at).match(INLINE_TEST_MODULE);
    if (!m) break;
    const start = at + m.index;
    // copy production code before the block
    out.push(...text.slice(at, start).split('\n'));
    // walk to the block's closing brace
    const open = text.indexOf('{', start + m[0].length - 1);
    let depth = 0;
    let i = open;
    for (; i < text.length; i += 1) {
      if (text[i] === '{') depth += 1;
      else if (text[i] === '}') { depth -= 1; if (depth === 0) break; }
    }
    // blank out the block, keeping line count for numbering
    const blanked = text.slice(start, i + 1).replace(/[^\n]/g, ' ');
    out.push(...blanked.split('\n'));
    at = i + 1;
  }
  out.push(...text.slice(at).split('\n'));
  return out.join('\n').split('\n');
}

export function scanText(text, ext) {
  const hits = [];
  let lines = stripComments(text, ext);
  if (ext === '.rs') lines = stripInlineTestBlocks(lines);
  lines.forEach((code, i) => {
    if (!VENUE_TOKEN.test(code)) return;
    for (const [re, msg] of BRANCH_SHAPES) {
      if (re.test(code)) {
        hits.push({ rule: msg, line: i + 1, snippet: code.trim().slice(0, 120) });
        return;
      }
    }
  });
  return hits;
}

/** Path allowlist, exported so the self-test can pin the seams by name. */
export function isAllowedPath(rel) {
  const norm = rel.split(sep).join('/');
  if (ALLOW_PREFIXES.some((p) => norm === p || norm.startsWith(p))) return true;
  // a /tests/ or /benches/ directory segment anywhere marks test code
  const dir = norm.slice(0, norm.lastIndexOf('/') + 1);
  if (TEST_SEGMENTS.some((s) => dir.includes(`/${s}/`))) return true;
  // integrations test files live as tests_<x>.rs or <x>_tests.rs
  const base = norm.slice(norm.lastIndexOf('/') + 1);
  return /^tests?_.*\.rs$/.test(base) || /_tests\.rs$/.test(base);
}

function allowed(rel) {
  return isAllowedPath(rel);
}

function scopeFiles() {
  const files = [];
  for (const dir of SCOPES) {
    const base = join(ROOT, dir);
    let walk;
    try {
      walk = (d) => {
        for (const e of readdirSync(d, { withFileTypes: true })) {
          const p = join(d, e.name);
          if (e.isDirectory()) {
            if (e.name === 'target' || e.name === 'node_modules') continue;
            walk(p);
          } else if (EXTS.some((x) => e.name.endsWith(x))) {
            const rel = p.slice(ROOT.length + 1);
            if (allowed(rel)) continue;
            files.push({ path: p, rel });
          }
        }
      };
      walk(base);
    } catch {
      // a scope that does not exist in this checkout is fine
    }
  }
  return files;
}

function selfTest() {
  let bad = 0;
  const clean = [
    ['a market-agnostic scanner passes', '.rs',
      'let venue = m.venue.clone();\nlet pair = min(depth_a, depth_b);\n'],
    ['prose naming venues in comments passes', '.rs',
      '// Kalshi and Polymarket both publish 15m rounds; the core never branches.\n'],
    ['a Lua strategy reading the venue FIELD passes', '.lua',
      'local v = listing.venue\nif cost < ceiling then bk.entry(token, price, v) end\n'],
    ['a data field / log / help line naming a venue passes', '.rs',
      'venue: "polymarket".into(),\ninfo!("settled on kalshi");\nlet help = "pull Polymarket fills";\n'],
    ['an inline #[cfg(test)] fixture naming venues passes', '.rs',
      '#[cfg(test)]\nmod tests {\n    let e = listing(Venue::Polymarket, "pm");\n    assert_eq!(e.venue, Venue::Kalshi);\n}\nlet size = compute_size(book);\n'],
  ];
  const dirty = [
    ['a kernel equality branch on a venue is caught', '.rs',
      'if venue == "Kalshi" { size *= 2 }\n', 'venue guard'],
    ['a comparison with the token on the left is caught', '.rs',
      'let hit = "polymarket" == leg.venue;\n', 'venue comparison'],
    ['a plugin-prefix routing branch is caught', '.rs',
      'let ok = token_id.starts_with("polymarket_");\n', 'venue routing'],
    ['a Lua equality branch on venue identity is caught', '.lua',
      "if venue == 'kalshi' then price = price - 0.01 end\n", 'venue guard'],
    ['a Rust match arm dispatching on Venue is caught', '.rs',
      'match leg.venue { Venue::Kalshi => size *= 2, _ => {} }\n', 'venue match arm'],
    ['a venue != comparison is caught', '.rs',
      'assert!(v != Venue::PredictFun);\n', 'venue comparison'],
  ];
  // Path-level seams: the unified model and the registration ring the
  // allowlist bell (the Venue enum naming every venue as DATA lives there);
  // the same code one directory over would NOT. Integration-test files are
  // out of scope by their /tests/ segment or tests_*_tests.rs name.
  const seamOk = isAllowedPath('core/market_api/src/unified/model.rs')
    && isAllowedPath('core/blitzkrieg_core/src/market/mod.rs')
    && isAllowedPath('extensions/kalshi/src/lib.rs')
    && isAllowedPath('core/blitzkrieg_core/tests/hedge_fault_matrix.rs')
    && isAllowedPath('core/market_api/tests/unified_reverse_acceptance.rs')
    && !isAllowedPath('core/blitzkrieg_core/src/engine.rs')
    && !isAllowedPath('core/strategy_logic/src/lib.rs');
  if (seamOk) console.log('  ok   the unified model / registration seam / plugins are allowlisted by path; the trading path is not');
  else { bad += 1; console.error('  FAIL path allowlist does not separate the seams from the trading path'); }
  for (const [name, ext, text] of clean) {
    const hits = scanText(text, ext);
    if (hits.length > 0) { bad += 1; console.error(`  FAIL ${name} — flagged ${JSON.stringify(hits)}`); }
    else console.log(`  ok   ${name}`);
  }
  for (const [name, ext, text, want] of dirty) {
    const hits = scanText(text, ext);
    const ok = hits.some((h) => h.rule.includes(want));
    if (!ok) { bad += 1; console.error(`  FAIL ${name} — scanner saw nothing`); }
    else console.log(`  ok   ${name}`);
  }
  if (bad > 0) { console.error(`\nno-venue-branch self-test: ${bad} fixture(s) failed`); process.exit(1); }
  console.log(`\nno-venue-branch self-test: ${clean.length + dirty.length} fixtures passed`);
}

const TEETH_SOURCE = [
  '// --teeth: the kernel `if kalshi` the #427 reverse-acceptance row demands',
  '// this gate catch.',
  'let size = if venue == "Kalshi" { shares * 2 } else { shares };',
].join('\n');

function teeth() {
  const hits = scanText(TEETH_SOURCE, '.rs');
  const ok = hits.length > 0;
  console.log(`${ok ? '  ok  ' : '  FAIL'} teeth: injected "if venue == \\"Kalshi\\"" into the trading path`);
  if (ok) for (const h of hits) console.log(`       line ${h.line}: ${h.snippet}`);
  if (ok) {
    console.log('\nno-venue-branch --teeth: the bad implementation was caught — the gate has teeth');
    process.exit(0);
  }
  console.error('\nno-venue-branch --teeth: the injected venue branch was NOT flagged — the gate has no teeth');
  process.exit(1);
}

function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('--self-test')) return selfTest();
  if (argv.includes('--teeth')) return teeth();

  const files = scopeFiles();
  let hits = 0;
  for (const f of files) {
    for (const h of scanText(readFileSync(f.path, 'utf8'), f.rel.slice(f.rel.lastIndexOf('.')))) {
      hits += 1;
      console.log(`  venue branch @ ${f.rel}:${h.line}: ${h.snippet}`);
    }
  }
  console.log(`no-venue-branch: scanned ${files.length} source file(s) across ${SCOPES.length} scopes`);
  if (hits > 0) {
    console.error(
      `\nRESULT: FAIL — ${hits} venue-name hit(s) in trading-path source.\n` +
        'Venue is data (#427): the kernel never branches on a concrete platform.\n' +
        'Legal homes are the unified model, the registration seam, the plugin\n' +
        'crates and tests — see scripts/no-venue-branch-check.mjs ALLOW_PREFIXES.',
    );
    process.exit(1);
  }
  console.log('RESULT: PASS — zero venue branches in the trading path');
}

main();
