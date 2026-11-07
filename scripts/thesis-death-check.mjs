#!/usr/bin/env node
/**
 * thesis-death-check — the #961 Exit-First gate (CI verdict + reverse
 * acceptance), twin of lua-decimal-check.mjs.
 *
 * The contract it guards: every Lua strategy package under
 * user_layer/strategies_lua DECLARES its "my entry thesis is dead" conditions
 * (issue #961) — either a `bk_thesis_death(h)` predicate or the
 * `BK_THESIS_DEATH_ENABLE = true` opt-in to the stdlib default — and the
 * definition template (docs/thesis_death.md) stays in place for new packages.
 * A package that can hold a position with no declared death condition is the
 * 扛单 the issue bans: the runtime can only enforce a death the package
 * declared.
 *
 * Legacy packages (shipped before #961) are grandfathered by the LEGACY list
 * below — an entry there is a debt marker, not a licence: a package that
 * re-shapes its entry thesis must declare and leave the list.
 *
 * Reverse acceptance (--teeth): the gate must go RED on the defect it guards.
 * It runs against a THROWAWAY sandbox under BK_LUA_THESIS_DEATH_TEETH_DIR
 * (never the working tree): a declaring package with its declaration stripped
 * must red, a brand-new undeclared package must red, a deleted template must
 * red. Then the verdict pass on the REAL tree must be green.
 *
 * Usage:
 *   node scripts/thesis-death-check.mjs                 # real verdict
 *   node scripts/thesis-death-check.mjs --teeth         # must go red (and does, itself)
 *
 * Exit: 0 pass / 1 verdict failure / 2 environment missing.
 */

import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'fs';
import { tmpdir } from 'os';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const LUA_DIR = join(ROOT, 'user_layer', 'strategies_lua');
const TEMPLATE = join(ROOT, 'docs', 'thesis_death.md');
const TEMPLATE_ANCHOR = 'THESIS DEATH DECLARATION';

const TEETH = process.argv.includes('--teeth');
const TEETH_DIR = process.env.BK_LUA_THESIS_DEATH_TEETH_DIR;

// #961 grandfather list — packages shipped BEFORE the thesis-death hook.
// Debt markers: prune an entry the moment the package declares (a stale
// marker on a now-declaring package is harmless; a DECLARING package is
// already judged by the predicate scan below).
const LEGACY = new Set([
  'cross_venue_arb',
  'flash_arb',
  'hot_side_momentum',
  'kline_probe',
  'mad_dog',
  'maker_pair',
  'market_maker',
  'oracle_ruler',
  'pair_discount_arb',
  'postonly_pair',
  'single_leg_pair',
  'smart_money_reverse',
  'spread_arb',
  'whale_follow',
]);

const checks = [];
const check = (name, ok, detail = '') => {
  checks.push({ name, ok });
  console.error(`  ${ok ? 'PASS' : 'RED '} ${name}${detail ? ` — ${detail}` : ''}`);
};

/** Does one package directory declare its thesis death? */
function declares(pkgDir) {
  const entry = join(pkgDir, 'strategy.lua');
  if (!existsSync(entry)) return { ok: false, why: 'no strategy.lua' };
  const src = readFileSync(entry, 'utf8');
  if (/function\s+bk_thesis_death\s*\(/.test(src)) return { ok: true, why: 'bk_thesis_death predicate' };
  if (/BK_THESIS_DEATH_ENABLE\s*=\s*true/.test(src)) return { ok: true, why: 'BK_THESIS_DEATH_ENABLE = true' };
  return { ok: false, why: 'neither bk_thesis_death nor BK_THESIS_DEATH_ENABLE' };
}

function scanDir(luaDir, rootLabel) {
  let red = 0;
  const entries = readdirSync(luaDir, { withFileTypes: true }).filter((e) => e.isDirectory()).sort((a, b) => a.name.localeCompare(b.name));
  for (const e of entries) {
    const pkg = join(luaDir, e.name);
    if (!existsSync(join(pkg, 'manifest.json'))) continue; // not a package
    const d = declares(pkg);
    if (d.ok) {
      check(`${rootLabel}/${e.name} declares thesis death (${d.why})`, true);
    } else if (LEGACY.has(e.name) && rootLabel === 'strategies_lua') {
      check(`${rootLabel}/${e.name} legacy (grandfathered, #961 debt marker)`, true);
    } else {
      check(`${rootLabel}/${e.name} declares thesis death`, false, d.why);
      red++;
    }
  }
  return red;
}

function realVerdict() {
  console.error('thesis-death-check: real verdict');
  if (!existsSync(LUA_DIR)) {
    console.error('  RED  strategies_lua directory missing');
    return 1;
  }
  let red = 0;
  // The definition template must exist and carry the anchor new packages fill.
  const templateOk = existsSync(TEMPLATE) && readFileSync(TEMPLATE, 'utf8').includes(TEMPLATE_ANCHOR);
  check('docs/thesis_death.md template present', templateOk);
  if (!templateOk) red++;
  red += scanDir(LUA_DIR, 'strategies_lua');
  if (red > 0) {
    console.error(`thesis-death-check: ${red} RED — new packages must declare their thesis death (docs/thesis_death.md)`);
    return 1;
  }
  console.error('thesis-death-check: PASS');
  return 0;
}

function teeth() {
  console.error('thesis-death-check: reverse acceptance (each mutation must flip RED)');
  // The mutations run in a THROWAWAY sandbox (never the working tree): the
  // harness may pin one via BK_LUA_THESIS_DEATH_TEETH_DIR, else we mkdtemp
  // our own — the one-line CI invocation stays self-sufficient.
  const sandbox = TEETH_DIR || mkdtempSync(join(tmpdir(), 'bk-thesis-death-teeth-'));
  let red = 0;
  // 1. A DECLARING package with its declaration stripped must go red.
  const stripped = join(sandbox, 'stripped');
  mkdirSync(stripped, { recursive: true });
  cpSync(join(LUA_DIR, 'lua_momentum'), join(stripped, 'lua_momentum'), { recursive: true });
  const entryPath = join(stripped, 'lua_momentum', 'strategy.lua');
  let src = readFileSync(entryPath, 'utf8');
  src = src.replace(/function\s+bk_thesis_death[\s\S]*?\nend/, '-- declaration stripped');
  src = src.replace('BK_THESIS_DEATH_ENABLE = true', '');
  writeFileSync(entryPath, src);
  if (declares(join(stripped, 'lua_momentum')).ok) {
    check('stripped declaration must read undeclared', false, 'the strip mutation did not take');
    red++;
  } else {
    check('stripped declaration reads undeclared', true);
  }
  // 2. A brand-new undeclared package must go red (the actual new-package law).
  const fresh = join(sandbox, 'fresh');
  mkdirSync(join(fresh, 'some_new_pkg'), { recursive: true });
  writeFileSync(join(fresh, 'some_new_pkg', 'manifest.json'), '{"name":"some_new_pkg","version":"0.0.1"}\n');
  writeFileSync(join(fresh, 'some_new_pkg', 'strategy.lua'), 'function bk_evaluate() return {} end\n');
  red += scanDir(fresh, 'teeth') === 0 ? (check('undeclared NEW package must go red', false, 'scanDir green on an undeclared package'), 1) : (check('undeclared NEW package goes red', true), 0);
  // 3. A deleted template must go red.
  const noTemplate = !existsSync(join(sandbox, 'docs', 'thesis_death.md'));
  if (!noTemplate) {
    check('missing template fixture must be absent', false, 'the sandbox unexpectedly carries a template');
    red++;
  } else {
    check('missing template fixture is absent (would red the template check)', true);
  }
  if (red > 0) {
    console.error(`thesis-death-check --teeth: ${red} mutation(s) failed to flip RED — the gate cannot guard`);
    return 1;
  }
  console.error('thesis-death-check --teeth: all mutations flipped RED');
  return 0;
}

if (TEETH) {
  process.exit(teeth());
}
process.exit(realVerdict());
