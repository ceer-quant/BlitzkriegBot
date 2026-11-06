#!/usr/bin/env node
/**
 * dep-direction-check — the 0.4.2 boundary, machine-enforced (#844 scope 1+2).
 *
 * `contract-tree-check.mjs` (#840 PR0) freezes the SHAPE of the crate graph
 * against a byte-exact baseline: every edge change is a disclosed change.
 * That is the net; THIS gate is the direction police. The freeze can be
 * re-baselined around a rule violation by hand; a direction violation
 * cannot — each edge is judged against the rules below independently of any
 * baseline, so no re-baseline can launder one.
 *
 * Source of truth: `cargo metadata --no-deps` (manifests only, no build).
 * dev- and build-edges count (a dev edge still compiles the forbidden crate
 * into the test graph — #825 proved the dev seam is a real seam). Optional
 * edges count (feature-gated deps ship under `default`: blitzkrieg-core's
 * market extensions ride exactly such optional edges and are carved by
 * name, not ignored).
 *
 * Rules:
 *   R1  core → concrete plugin / UI / strategy IMPLEMENTATION is forbidden.
 *       blitzkrieg-core may depend only on (checked by crate name):
 *         blitzkrieg-build-support / blitzkrieg-build-info   (launch plumbing)
 *         blitzkrieg-market-api / blitzkrieg-data-model      (contract lane)
 *         blitzkrieg-strategy-api                            (frozen C ABI)
 *         blitzkrieg-lua-runtime                             (sandbox host, E30:
 *                                                             it hosts, it does
 *                                                             not decide)
 *         strategy-logic           (shared reference math the external cdylib
 *                                  runs too — interface side, not the kernel's
 *                                  private implementation)
 *         blitzkrieg-onchain-pull  (#648 C: the `--onchain-pull` dispatch
 *                                  entry point only; no pull lives in the
 *                                  kernel)
 *         blitzkrieg-market-api / blitzkrieg-data-model       (contract lane)
 *       Dev-only grandfather (the #236 producer/consumer seam tests, never
 *       linked into the shipped lib): blitzkrieg-ui-kit, parity-logic — but
 *       ONLY as dev-dependencies; a normal/build edge to either reds.
 *   R2  plugin / UI / strategy → blitzkrieg-core is forbidden, and
 *       plugin → plugin is forbidden (the four-extension "zero reverse
 *       dependency" family rule, #840 Wave 1). Downstream touches ONLY the
 *       contract lane (bk_contract directly, or market_api which re-exports
 *       it).
 *   R3  cross-boundary DTO ownership: a public serde Serialize/Deserialize
 *       struct/enum COMPILED into a non-contract crate whose NAME collides
 *       with a bk_contract-owned serde type reds — same wire name + two
 *       definitions = drift with no owner. The scan follows the REAL module
 *       tree (inline `mod x { … }` blocks contribute an `x/` path segment
 *       for their children, per the Rust reference), so dead files no `mod`
 *       reaches — market_api's pre-#840 `unified/{matching,model}.rs`
 *       orphan copies, byte-identical to the canonical files in bk_contract
 *       — are NOT flagged: they are not compiled.
 *   R4  NO CYCLES over the whole internal edge graph, dev edges included.
 *       cargo itself rejects workspace cycles at resolve time, so the live
 *       tree must pass trivially — the rule exists for its teeth, which the
 *       negative fixture proves (it names the exact offending path).
 *
 * Every failure names the exact offending edge/type with both endpoints —
 * a red is an answer, not a puzzle.
 *
 * Usage:
 *   node scripts/dep-direction-check.mjs               # the real verdict
 *   node scripts/dep-direction-check.mjs --self-test   # in-memory teeth (§A.4)
 *
 * Exit: 0 pass / 1 violations.
 */

import { execFileSync } from 'child_process';
import { existsSync, readFileSync } from 'fs';
import { dirname, join, relative, resolve } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// ── lane membership ──────────────────────────────────────────────────────────

export const LANE = {
  CONTRACT: 'contract',
  CORE: 'core',
  PLUGIN: 'plugin',
  UI: 'ui',
  STRATEGY: 'strategy',
  TOOLS: 'tools',
  OTHER: 'other',
};

/** Classify a workspace member by package name and manifest path. */
export function laneOf(name, manifestPath, root = ROOT) {
  const rel = relative(root, manifestPath).split('\\').join('/');
  if (name === 'blitzkrieg-bk-contract' || name === 'blitzkrieg-market-api') return LANE.CONTRACT;
  if (name === 'blitzkrieg-core') return LANE.CORE;
  if (rel.startsWith('extensions/')) return LANE.PLUGIN;
  if (rel.startsWith('ui/')) return LANE.UI;
  if (rel.startsWith('tools/')) return LANE.TOOLS;
  if (rel.startsWith('user_layer/')) return LANE.STRATEGY;
  return LANE.OTHER; // core/build_support, core/build_info
}

/** R1: core's allowed targets, each a carved reason to NOT be a concrete
 * implementation. Everything else under R1 needs a carve here, in review. */
export const CORE_ALLOWED = new Set([
  'blitzkrieg-build-support',
  'blitzkrieg-build-info',
  'blitzkrieg-market-api',
  'blitzkrieg-data-model',
  'blitzkrieg-strategy-api',
  'blitzkrieg-lua-runtime',
  'strategy-logic',
  'blitzkrieg-onchain-pull',
]);

/** R1 dev-only grandfather (#236 seam tests). */
export const CORE_DEV_SEAM = new Set(['blitzkrieg-ui-kit', 'parity-logic']);

/** R1 optional-assembly carve (the one seam the boundary itself defines):
 * the core never binds a market plugin statically — the runtime resolves the
 * operative venue through the market_api traits — but the shipped BINARY
 * must contain the four built-in market extensions, linked in through
 * opt-in features (blitzkrieg-core/Cargo.toml: polymarket/kalshi/
 * predictfun/polysimulator). This is assembly, not a dependency on plugin
 * internals. The carve is DELIBERATELY narrow:
 *   - the edge must be feature-OPTIONAL (non-optional ships the plugin
 *     through every core consumer and is NOT carved) and
 *   - only these four built-in extensions (a fifth "core -> plugin" edge —
 *     even optional — must extend this list in review, which is the point). */
export const CORE_PLUGIN_ASSEMBLY = new Set([
  'polymarket-extension',
  'blitzkrieg-kalshi',
  'blitzkrieg-predictfun',
  'blitzkrieg-polysimulator',
]);

// ── metadata → edges ─────────────────────────────────────────────────────────

export function workspaceMetadata(cwd = ROOT) {
  return JSON.parse(
    execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {
      cwd,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    }),
  );
}

/** One edge record per resolved member pair: {from,to,kinds:Set,optional}. */
export function extractGraph(meta) {
  const members = new Set(meta.packages.map((p) => p.name));
  const raw = [];
  for (const p of meta.packages) {
    for (const d of p.dependencies) {
      if (d.source !== null && d.source !== undefined) continue; // registry/git
      if (!members.has(d.name)) continue;
      raw.push({
        from: p.name,
        to: d.name,
        optional: Boolean(d.optional),
        kinds: new Set(d.kind === null ? ['normal'] : [d.kind]),
      });
    }
  }
  // Same pair declared in different manifest sections merges with unioned
  // kinds; `optional` fails OPEN (any non-optional declaration ships it).
  const agg = new Map();
  for (const e of raw) {
    const k = `${e.from}=>${e.to}`;
    if (!agg.has(k)) agg.set(k, { from: e.from, to: e.to, kinds: new Set(), optional: e.optional });
    for (const kind of e.kinds) agg.get(k).kinds.add(kind);
    if (!e.optional) agg.get(k).optional = false;
  }
  return [...agg.values()];
}

/** Deterministic human-readable edge label. */
export function edgeLabel(e) {
  const kinds = [...e.kinds].sort().join('+');
  return `${e.from} -> ${e.to} [${kinds}${e.optional ? ', optional' : ''}]`;
}

// ── R1/R2: the edge judge (exported for --self-test) ─────────────────────────

/**
 * @param {Array<{from,to,kinds:Set<string>,optional:boolean}>} edges
 * @param {(name: string) => string} laneByName name → lane
 * @returns {string[]} violations, exact-edge named; empty = pass
 */
export function judgeEdges(edges, laneByName) {
  const problems = [];
  for (const e of edges) {
    const a = laneByName(e.from);
    const b = laneByName(e.to);
    const label = edgeLabel(e);
    const shipsByDefault = !e.optional || e.kinds.has('normal') || e.kinds.has('build');
    const isDev = e.kinds.has('dev');

    // R1 — blitzkrieg-core's outbound edges.
    if (a === LANE.CORE) {
      if (b === LANE.PLUGIN) {
        if (!e.optional || !CORE_PLUGIN_ASSEMBLY.has(e.to)) {
          problems.push(`violation: core -> concrete plugin is forbidden (feature-optional builtin assembly is the only carve): ${label}`);
        }
      } else if (b === LANE.UI && !CORE_DEV_SEAM.has(e.to)) {
        problems.push(`violation: core -> concrete UI implementation is forbidden: ${label}`);
      } else if (b === LANE.UI && CORE_DEV_SEAM.has(e.to) && shipments(shipsByDefault, isDev, e)) {
        problems.push(
          `violation: core -> ${e.to} is dev-seam-only (#236); a normal/build edge is forbidden: ${label}`,
        );
      } else if (b !== LANE.CONTRACT && !CORE_ALLOWED.has(e.to) && !CORE_DEV_SEAM.has(e.to)) {
        problems.push(`violation: core -> ${b} implementation not carve-listed: ${label}`);
      }
    }

    // R2 — downstream must not climb back up, and no plugin plugins together.
    if (b === LANE.CORE && (a === LANE.PLUGIN || a === LANE.UI || a === LANE.STRATEGY || a === LANE.TOOLS)) {
      problems.push(`violation: ${a} -> core is forbidden: ${label}`);
    }
    if (a === LANE.PLUGIN && b === LANE.PLUGIN) {
      problems.push(`violation: plugin -> another concrete plugin is forbidden: ${label}`);
    }
    if (a === LANE.UI && b === LANE.PLUGIN) {
      problems.push(`violation: ui -> plugin is forbidden: ${label}`);
    }
    if (a === LANE.STRATEGY && b === LANE.PLUGIN) {
      problems.push(`violation: strategy -> plugin is forbidden: ${label}`);
    }
  }
  return problems;
}

/** The dev-seam grandfather holds only while the edge is dev-ONLY. */
function shipments(ships, isDev, e) {
  return ships && ![...e.kinds].every((k) => k === 'dev');
}

// ── R4: cycle detection ──────────────────────────────────────────────────────

/** Elementary cycles over the edge list, each reported once (rotation-dedup),
 * bounded depth so pathological graphs cannot wedge the gate. */
export function findCycles(edges) {
  const directed = new Map();
  for (const e of edges) {
    if (!directed.has(e.from)) directed.set(e.from, new Set());
    directed.get(e.from).add(e.to);
  }
  const seen = new Set();
  const out = [];
  const roots = [...directed.keys()].sort();
  for (const root of roots) {
    const stack = [[root, [root]]];
    while (stack.length) {
      const [node, path] = stack.pop();
      for (const next of [...(directed.get(node) || [])].sort()) {
        if (next === root) {
          const key = [...path].sort().join('|');
          if (!seen.has(key)) {
            seen.add(key);
            out.push([...path, root]);
          }
          continue;
        }
        if (path.includes(next) || path.length > 16) continue;
        stack.push([next, [...path, next]]);
      }
    }
  }
  return out;
}

// ── R3: the module-tree-faithful Rust scanner ────────────────────────────────

const MOD_DECL = /^(?!\s*\/\/)(\s*)(?:pub(?:\([^)]*\))?\s+)?mod\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*(;|\{)/;
const DERIVE = /^\s*#\[\s*derive\s*\(([^)]*)\)\]/;
const ITEM = /^\s*pub\s+(struct|enum)\s+([A-Za-z_][A-Za-z0-9_]*)/;

/** Was the last `#[derive(...)]` seen a serde wire derivation? */
function serdeKindOf(lastDerive) {
  if (!lastDerive) return null;
  const m = lastDerive.match(DERIVE);
  if (!m) return null;
  const has = (t) => new RegExp(`\\b${t}\\b`).test(m[1]);
  const s = has('Serialize');
  const d = has('Deserialize');
  if (s && d) return 'both';
  if (s) return 'ser';
  if (d) return 'de';
  return null;
}

/** Offset of line `i` (0-based) within `lines` joined by '\n'. */
function offsetOf(lines, i) {
  let n = 0;
  for (let j = 0; j < i; j++) n += lines[j].length + 1;
  return n;
}

/**
 * Scan a crate's src/ from lib.rs/main.rs and return, for every `.rs` file
 * REACHED through the module tree, its public serde-derived type names:
 *   Map<typeName, filePath>.
 * Module path rules mirrored from the Rust reference:
 *   - file module `foo.rs` at dir D  ⇒ its children resolve in `D/foo/`
 *   - directory module `foo/mod.rs` at D ⇒ children resolve in `D/foo/`
 *     (its own directory — stem 'mod' specializes to the file's own dir)
 *   - inline `mod x { … }` in a file whose children dir is C ⇒ the inline
 *     module's children resolve in `C/x/`
 *   - roots: `lib.rs`/`main.rs` (children dir = src/)
 * Dead files (`mod` never declares) never enter the scan.
 */
export function compiledSerdeTypes(srcDir) {
  const types = new Map();
  const files = new Set();

  function scanFile(path) {
    if (files.has(path)) return;
    files.add(path);
    let text;
    try {
      text = readFileSync(path, 'utf8');
    } catch {
      return;
    }
    const base = dirname(path);
    const stem = path.split('/').pop().replace(/\.rs$/, '');
    // A file hosts module `stem`; its child modules resolve under stem/ —
    // except lib.rs / main.rs / mod.rs, whose children resolve in `base`.
    const childrenDir = stem === 'lib' || stem === 'main' || stem === 'mod' ? base : join(base, stem);

    let lastDerive = null;
    const lines = text.split('\n');
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];

      if (DERIVE.test(line)) {
        lastDerive = line;
        continue;
      }

      const mDecl = line.match(MOD_DECL);
      if (mDecl && mDecl[1].length < 2) {
        // top-level-ish module declaration (indent < 2 keeps us out of the
        // inside of fn bodies' conditional mods, and of `mod tests` blocks —
        // those carry no wire types by repo convention)
        const name = mDecl[2];
        const brace = mDecl[3];
        if (brace === ';') {
          for (const cand of [join(childrenDir, `${name}.rs`), join(childrenDir, name, 'mod.rs')]) {
            if (existsSync(cand) && !files.has(cand)) scanFile(cand);
          }
        } else {
          // inline block: mod-decls inside resolve under childrenDir/name/
          const textOffset = offsetOf(lines, i);
          const openIdx = text.indexOf('{', textOffset);
          if (openIdx >= 0) {
            let depth = 0;
            let j = openIdx;
            while (j < text.length) {
              if (text[j] === '{') depth++;
              else if (text[j] === '}') {
                depth--;
                if (depth === 0) break;
              }
              j++;
            }
            const block = text.slice(openIdx, j + 1);
            const inlineDir = join(childrenDir, name);
            for (const m2 of block.split('\n')) {
              const m2d = m2.match(MOD_DECL);
              if (m2d && m2d[3] === ';') {
                const nm = m2d[2];
                for (const cand of [join(inlineDir, `${nm}.rs`), join(inlineDir, nm, 'mod.rs')]) {
                  if (existsSync(cand) && !files.has(cand)) scanFile(cand);
                }
              }
            }
          }
        }
        lastDerive = null;
        continue;
      }

      const mItem = line.match(ITEM);
      if (mItem) {
        if (serdeKindOf(lastDerive)) types.set(mItem[2], path);
        lastDerive = null;
        continue;
      }

      // Other attributes (`#[serde(rename_all = …)]`) and comments sit
      // BETWEEN the derive and the item — they must PRESERVE the link,
      // not reset it. Only plain code resets the association.
      if (/^\s*#/.test(line) || /^\s*\/\//.test(line) || !line.trim()) continue;
      lastDerive = null;
    }
  }

  for (const rootFile of ['lib.rs', 'main.rs']) {
    const p = join(srcDir, rootFile);
    if (existsSync(p)) scanFile(p);
  }
  return { types, files };
}

/**
 * R3 verdict: contract lane ↔ everything else. Returns violation strings.
 * @param {string} contractSrcDir  bk_contract's src/
 * @param {Array<[string, string]>} otherCrates  [crateName, crateSrcDir, lane]
 */
export function judgeDtoOwnership(contractSrcDir, otherCrates) {
  const contract = compiledSerdeTypes(contractSrcDir);
  const contractNames = new Set(contract.types.keys());
  const problems = [];
  for (const [name, srcDir, lane, rootDir] of otherCrates) {
    if (lane === LANE.CONTRACT || lane === LANE.OTHER) continue;
    const scan = compiledSerdeTypes(srcDir);
    const collisions = [...scan.types.keys()].filter((t) => contractNames.has(t));
    for (const t of collisions) {
      const at = relative(rootDir || ROOT, scan.types.get(t)).split('\\').join('/');
      problems.push(
        `violation: wire DTO '${t}' redefined outside bk_contract: ${name} has its own ` +
          `serde Serialize/Deserialize type named ${t} — own it in bk_contract or rename one side (${at})`,
      );
    }
  }
  return problems;
}

// ── --self-test: teeth on in-memory fixtures, then the live tree ─────────────

function selfTest() {
  const checks = [];
  const expect = (name, ok, detail = '') => checks.push({ name, ok, detail });

  // fixup for a stale helper name
  const laneByName = (name) => {
    const known = {
      'blitzkrieg-core': LANE.CORE,
      'blitzkrieg-bk-contract': LANE.CONTRACT,
      'blitzkrieg-market-api': LANE.CONTRACT,
      'blitzkrieg-build-info': LANE.OTHER,
      'blitzkrieg-build-support': LANE.OTHER,
      'blitzkrieg-data-model': LANE.CONTRACT,
      'blitzkrieg-strategy-api': LANE.STRATEGY,
      'blitzkrieg-lua-runtime': LANE.STRATEGY,
      'strategy-logic': LANE.STRATEGY,
      'parity-logic': LANE.STRATEGY,
      'blitzkrieg-onchain-pull': LANE.TOOLS,
      'blitzkrieg-ui-kit': LANE.UI,
      'blitzkrieg-ui-panel': LANE.UI,
      'polymarket-extension': LANE.PLUGIN,
      'blitzkrieg-kalshi': LANE.PLUGIN,
      'blitzkrieg-predictfun': LANE.PLUGIN,
      'blitzkrieg-polysimulator': LANE.PLUGIN,
    };
    return known[name] ?? LANE.OTHER;
  };
  // (lane map above is the fixture mirror of laneOf; kept local so the teeth
  // run without cargo)

  // ── R1 teeth ──
  const ok1 = 'core -> market-api danced [normal]';
  {
    const edges = [{ from: 'blitzkrieg-core', to: 'blitzkrieg-market-api', kinds: new Set(['normal']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect('R1: core -> contract lane passes', p.length === 0, p.join('; '));
  }
  {
    const edges = [{ from: 'blitzkrieg-core', to: 'polymarket-extension', kinds: new Set(['normal']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect(
      'R1 tooth: core -> plugin (non-optional) reds with the exact edge',
      p.length === 1 && p[0].includes('core -> polymarket-extension'),
      p.join('; '),
    );
  }
  {
    const edges = [{ from: 'blitzkrieg-core', to: 'blitzkrieg-kalshi', kinds: new Set(['normal']), optional: true }];
    const p = judgeEdges(edges, laneByName);
    expect('R1: core -> builtin extension as feature-optional assembly is the carved seam', p.length === 0, p.join('; '));
  }
  {
    const edges = [{ from: 'blitzkrieg-core', to: 'fifth-extension', kinds: new Set(['normal']), optional: true }];
    const p5 = judgeEdges(edges, (n) => (n === 'blitzkrieg-core' ? LANE.CORE : n === 'fifth-extension' ? LANE.PLUGIN : LANE.CONTRACT));
    expect(
      'R1 tooth: core -> plugin (optional, NOT carve-listed) reds',
      p5.length === 1 && p5[0].includes('feature-optional builtin assembly is the only carve'),
      p5.join('; '),
    );
  }
  {
    const edges = [{ from: 'blitzkrieg-core', to: 'blitzkrieg-ui-panel', kinds: new Set(['normal']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect(
      'R1 tooth: core -> concrete UI reds on a normal edge',
      p.length === 1 && p[0].includes('concrete UI implementation'),
      p.join('; '),
    );
  }
  {
    const edges = [{ from: 'blitzkrieg-core', to: 'blitzkrieg-ui-kit', kinds: new Set(['dev']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect('R1: core -> ui_kit as dev-dependency is the grandfathered seam', p.length === 0, p.join('; '));
  }
  {
    const edges = [{ from: 'blitzkrieg-core', to: 'blitzkrieg-ui-kit', kinds: new Set(['dev', 'normal']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect(
      'R1 tooth: ui_kit dev-seam rot reds when it ships by default',
      p.length === 1 && p[0].includes('dev-seam-only'),
      p.join('; '),
    );
  }

  // ── R2 teeth ──
  {
    const edges = [{ from: 'polymarket-extension', to: 'blitzkrieg-core', kinds: new Set(['normal']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect(
      'R2 tooth: plugin -> core reds',
      p.length === 1 && p[0].includes('plugin -> core is forbidden'),
      p.join('; '),
    );
  }
  {
    const edges = [{ from: 'blitzkrieg-kalshi', to: 'blitzkrieg-market-api', kinds: new Set(['normal']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect('R2: plugin -> contract lane passes', p.length === 0, p.join('; '));
  }
  {
    const edges = [{ from: 'polymarket-extension', to: 'blitzkrieg-kalshi', kinds: new Set(['normal']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect(
      'R2 tooth: plugin -> plugin reds',
      p.length === 1 && p[0].includes('plugin -> another concrete plugin'),
      p.join('; '),
    );
  }
  {
    const edges = [{ from: 'blitzkrieg-ui-panel', to: 'blitzkrieg-core', kinds: new Set(['normal']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect('R2 tooth: ui -> core reds', p.length === 1 && p[0].includes('ui -> core is forbidden:'), p.join('; '));
  }
  {
    const edges = [{ from: 'strategy-logic', to: 'polymarket-extension', kinds: new Set(['normal']), optional: false }];
    const p = judgeEdges(edges, laneByName);
    expect('R2 tooth: strategy -> plugin reds', p.length === 1 && p[0].includes('strategy -> plugin'), p.join('; '));
  }

  // ── R4 teeth ──
  {
    const e = (f, t) => ({ from: f, to: t, kinds: new Set(['normal']), optional: false });
    const cycles = findCycles([e('a', 'b'), e('b', 'c'), e('c', 'a')]);
    expect(
      'R4 tooth: three-node cycle names the exact path',
      cycles.length === 1 && cycles[0].join(' -> ') === 'a -> b -> c -> a',
      JSON.stringify(cycles),
    );
  }
  {
    const e = (f, t) => ({ from: f, to: t, kinds: new Set(['normal']), optional: false });
    const cycles = findCycles([e('a', 'b'), e('b', 'a'), e('x', 'y'), e('c', 'd')]);
    expect('R4: two-node cycle caught, acyclic pairs not flagged', cycles.length === 1, JSON.stringify(cycles));
  }

  // ── R3 teeth: in-memory crate dirs under os.tmpdir() ──
  {
    const { mkdtempSync, writeFileSync, rmSync } = await_import_fs();
    const tmp = mkdtempSync();
    try {
      // contract crate owns BookUpdate; a plugin crate redefines it ⇒ red;
      // an UNREACHED dead file with the same name ⇒ silent (not compiled).
      mkcrate(join(tmp, 'contract'), {
        'lib.rs': `
pub mod types;
`,
        'types.rs': `
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookUpdate {
    pub token_id: String,
    pub bids: Vec<(String, String)>,
}
`,
      });
      mkcrate(join(tmp, 'plugin'), {
        'lib.rs': `
pub mod host;
`,
        'host.rs': `
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookUpdate {
    pub token_id: String,
}
`,
      });
      mkcrate(join(tmp, 'plugin_clean'), {
        'lib.rs': `
pub mod host;
`,
        'host.rs': `
// dead file: never declared by lib.rs — Rust would not compile it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookUpdate2 {
    pub bids: String,
}
#[derive(Serialize)]
pub struct OwnShape {
    pub legs: Vec<String>,
}
`,
      });
      const problems = judgeDtoOwnership(
        join(tmp, 'contract', 'src'),
        [
          ['fake-plugin', join(tmp, 'plugin', 'src'), LANE.PLUGIN, tmp],
          ['plugin-clean', join(tmp, 'plugin_clean', 'src'), LANE.PLUGIN, tmp],
        ],
      );
      expect(
        'R3 tooth: serde DTO name collision outside bk_contract reds with the file named',
        problems.length === 1 && problems[0].includes("wire DTO 'BookUpdate'") && problems[0].includes('plugin/src/host.rs'),
        problems.join('; '),
      );
    } finally {
      rmSync(tmp, { recursive: true, force: true });
    }
  }

  const failed = checks.filter((c) => !c.ok);
  for (const c of checks) console.log(`${c.ok ? 'ok  ' : 'RED '} self-test: ${c.name}`);
  if (failed.length) {
    for (const c of failed) if (c.detail) console.error(`  ${c.detail}`);
    console.error(`\ndep-direction-check self-test: ${failed.length} RED`);
    process.exit(1);
  }
  console.log(`\ndep-direction-check self-test: all green (${checks.length} checks)`);
  process.exit(0);
}

// tiny helper: top-level await is fine in .mjs, so this can just be imports —
// keep the self-test body synchronous-friendly instead.
import { mkdtempSync, rmSync, writeFileSync as wf } from 'fs';
import { tmpdir } from 'os';
function await_import_fs() {
  return {
    mkdtempSync: () => mkdtempSync(join(tmpdir(), 'dep-direction-')),
    rmSync,
  };
}
function mkcrate(base, files) {
  const src = join(base, 'src');
  const { mkdirSync } = await_import_fs_mkdir();
  mkdirSync(src, { recursive: true });
  wf(src + '/_rc', '');
  for (const [fn, body] of Object.entries(files)) wf(join(src, fn), body);
}
import { mkdirSync as mkd } from 'fs';
function await_import_fs_mkdir() {
  return { mkdirSync: mkd };
}

// ── main ─────────────────────────────────────────────────────────────────────

if (process.argv.includes('--self-test')) {
  selfTest(); // exits 0/1 itself; never falls through to the live verdict
}

// --fixture-edges '<json>' — judge ONLY mode for scripts/dep-direction-check-teeth.sh:
// runs judgeEdges + findCycles over the given edge list with the repo-shaped
// lane map (fixture names outside it fall back to their Cargo-tree lane by
// prefix: plugin* extensions polymarket), then exits 1 with the exact
// violations. NOT used by the real verdict — that always reads cargo metadata.
if (process.argv.includes('--fixture-edges')) {
  const i = process.argv.indexOf('--fixture-edges');
  const raw = process.argv[i + 1];
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (err) {
    console.error(`--fixture-edges: invalid JSON: ${err.message}`);
    process.exit(2);
  }
  const edges = parsed.map((e) => ({ ...e, kinds: new Set(e.kinds) }));
  const laneByName = (name) => {
    const map = {
      'blitzkrieg-core': LANE.CORE,
      'blitzkrieg-bk-contract': LANE.CONTRACT,
      'blitzkrieg-market-api': LANE.CONTRACT,
      'blitzkrieg-data-model': LANE.CONTRACT,
    };
    if (map[name]) return map[name];
    if (/plugin|extension|kalshi|predictfun|polysimulator|polymarket/.test(name)) return LANE.PLUGIN;
    if (/^ui[-_]|ui[-_]kit/.test(name)) return LANE.UI;
    if (/strateg|parity/.test(name)) return LANE.STRATEGY;
    return LANE.TOOLS;
  };
  const problems = [...judgeEdges(edges, laneByName)];
  for (const c of findCycles(edges)) problems.push(`violation: dependency cycle: ${c.join(' -> ')}`);
  if (problems.length) {
    for (const p of problems) console.error(`  ${p}`);
    console.error(`\ndep-direction-check fixture: ${problems.length} violation(s)`);
    process.exit(1);
  }
  console.log('dep-direction-check fixture: PASS');
  process.exit(0);
}

// --fixture-dto <contractSrc> 'name:srcDir:lane' [more...] — R3 judge ONLY mode
// for the teeth script: real module-tree walk + ownership judge over scratch
// crate trees, no cargo involved.
if (process.argv.includes('--fixture-dto')) {
  const i = process.argv.indexOf('--fixture-dto');
  const contractSrc = process.argv[i + 1];
  const others = process.argv.slice(i + 2).map((tok) => {
    // 'name:srcDir:lane' — paths may contain no ':' here, so split on the last
    // colon for lane and the first colon for name.
    const laneIdx = tok.lastIndexOf(':');
    const lane = tok.slice(laneIdx + 1);
    const rest = tok.slice(0, laneIdx);
    const nameIdx = rest.indexOf(':');
    return [rest.slice(0, nameIdx), rest.slice(nameIdx + 1), lane];
  });
  const problems = judgeDtoOwnership(contractSrc, others.map((o) => [...o, dirname(contractSrc)]));
  if (problems.length) {
    for (const p of problems) console.error(`  ${p}`);
    console.error(`\ndep-direction-check fixture-dto: ${problems.length} violation(s)`);
    process.exit(1);
  }
  console.log('dep-direction-check fixture-dto: PASS');
  process.exit(0);
}

const meta = workspaceMetadata();
const paths = new Map(meta.packages.map((p) => [p.name, p.manifest_path]));
const edges = extractGraph(meta); // pre-aggregated inside
const edgeList = edges;
const laneByName = (name) => {
  const mp = paths.get(name);
  return mp ? laneOf(name, mp) : LANE.OTHER;
};

const problems = [...judgeEdges(edgeList, laneByName)];

const contractSrc = join(dirname(meta.packages.find((p) => p.name === 'blitzkrieg-bk-contract').manifest_path), 'src');
const otherCrates = meta.packages
  .filter((p) => p.name !== 'blitzkrieg-bk-contract')
  .map((p) => [p.name, join(dirname(p.manifest_path), 'src'), laneByName(p.name), ROOT])
  .filter(([, srcPath]) => existsSync(srcPath));
problems.push(
  ...judgeDtoOwnership(contractSrc, otherCrates),
);

const cycles = findCycles(edgeList);
for (const c of cycles) problems.push(`violation: dependency cycle: ${c.join(' -> ')}`);

// fail-closed: a real workspace ALWAYS has internal edges. Empty extraction
// means the metadata output defeated the parser — say so.
if (edgeList.length === 0) {
  console.error('dep-direction-check: FAIL-CLOSED — zero internal edges extracted; cargo metadata output defeated the parser.');
  process.exit(1);
}

if (problems.length) {
  console.error(`dep-direction-check: ${problems.length} boundary violation(s) —\n`);
  for (const p of problems) console.error(`  ${p}`);
  console.error(
    '\nThe 0.4.2 boundary is machine-enforced: core depends on nothing but the contract lane ' +
      'plus the carve list above; plugins/UI/strategies depend on nothing but the contract lane. ' +
      'Move the offending edge into bk_contract/market_api or remove it.',
  );
  process.exit(1);
}
console.log(
  `dep-direction-check: PASS — ${meta.packages.length} members, ${edgeList.length} internal edges: ` +
    `R1 core-carve ok, R2 no reverse edges, R3 DTO ownership single-owner, R4 no cycles`,
);
