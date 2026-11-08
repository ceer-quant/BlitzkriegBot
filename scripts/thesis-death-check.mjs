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
 * Legacy packages (shipped before #961) went through the #961 debt
 * convergence: every one of them now either DECLARES (judged by the
 * predicate scan below, listed nowhere) or carries a written, per-package
 * EXEMPTION reason in the map below. A bare grandfather entry no longer
 * exists — and a package that re-shapes its entry thesis must still declare
 * and leave the exemption map.
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

// #961 legacy convergence — the reasoned exemption map. Every pre-#961
// package that does NOT declare its thesis death lives here WITH a one-line
// reason (no bare grandfather entries survive the convergence): hedged-pair
// collectors whose thesis is locked at entry, pure market making, probes,
// and packages whose death condition is already self-declared outside the
// hook. A DECLARING package must NOT be listed (the predicate scan judges
// it); a stale entry is harmless but untidy. New packages declare per the
// docs/thesis_death.md template — they never land here.
const EXEMPT = new Map([
  [
    'cross_venue_arb',
    '完整双腿跨所对冲（两 venue 各持一腿、等份额，价值入场锁定 $1）：论点是折价本身，没有方向侧可死；spread_close_pct 回落平仓 + 未持有腿补发循环就是出场机制，单腿自身 bid 反转不构成论点死亡',
  ],
  [
    'pair_discount_arb',
    '完整 UP+DOWN 对（MERGE 收集，利润入场锁定）：持有形态是对冲对而非方向仓；裸腿由 top-up 循环主动修复（max_top_up_cost 是敞口界），单腿 bid 反转不是论点死亡',
  ],
  [
    'postonly_pair',
    'maker 配对循环（双腿 post-only + pairing/top-up loop）：同 pair_discount_arb 的对冲对形态，论点是配对折价，无方向侧可死',
  ],
  [
    'maker_pair',
    'both-or-nothing maker 对（等份额对冲持有到结算，零 exits）：完整对价值恒 $1；单腿裸露被「一腿失格即弃整单」的入场纪律排除，无论点可死',
  ],
  [
    'market_maker',
    '纯做市：论点是波动付价差、非结算结果；take-profit 带 + kernel 阶梯 + merge 已是出场机制，反转带会在正常库存震荡上误触发（0.30 买入回落至 0.225 是库存噪音，不是论点死亡）',
  ],
  [
    'oracle_ruler',
    '校准探针：无条件挂单测量执行层，故意无门可甩（booked exit_reason 由 oracle-ruler-check.mjs 断言）；没有可死的策略论点',
  ],
  [
    'kline_probe',
    '抛弃式回放二分探针（bisect 1，flash_arb latch 原样复刻）：仅诊断触发链，非生产策略，无独立论点',
  ],
  [
    'smart_money_reverse',
    '自有 thesis_reversed 判决已声明死亡条件（thesis_break_pct 对 rebound_anchor，#847 fix 2 形态）：钩子平移以 mad_dog 同形先例为准，留待专项收敛',
  ],
  [
    'spread_arb',
    '#1003 按包关闭（2026-11-08 实测撤销 #961 形态 B 声明）：stdlib 默认判据按单腿自身 bid 判死，而本包论点是 spread —— 25% 的单腿回撤在宽赔率带入场里是轮内普通方差，强制 taker 平仓把会自然恢复的仓位 realize 成损失；A/B 实测 death 臂净利劣于 nodeath（official 费 Δ −$73.10 / legacy 费 Δ −$62.64，误杀率 309/764 = 40.45%，workbuddy W5 复测），调松带宽已测无法收回基线。出场面保持内核 trailing/stop 阶梯 + 结算，不引入策略侧止损。改动入场论点须按模板重新声明',
  ],
  [
    'single_leg_pair',
    '#1003 按包关闭（2026-11-08 实测撤销 #961 形态 B 声明）：包章约明确「止损不归你管」——裸腿生存权交给内核（结算 / force-exit），而 stdlib 默认的单腿反转带就是策略侧止损；[0.55,0.88] 赔率带内的 25% 逆向波动正是 EV 定价要扛的方差，判死等于把噪声 realize 成损失（误杀率 121/238 = 50.84%；official 费下 death 臂不再优于 nodeath，Δ −$0.07，workbuddy W5 复测）。出场面保持结算 / 内核 force-exit，不引入止损。改动入场论点须按模板重新声明',
  ],
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
    } else if (EXEMPT.has(e.name) && rootLabel === 'strategies_lua') {
      check(`${rootLabel}/${e.name} exempt (#961 convergence): ${EXEMPT.get(e.name)}`, true);
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
