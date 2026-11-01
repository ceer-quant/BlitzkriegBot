#!/usr/bin/env node
/**
 * lua-decimal-sync — the #439 candidate-2 sync tool.
 *
 * `user_layer/strategies_lua/common/decimal.lua` is the ONE source of truth
 * for the exact-decimal helper block. The Lua sandbox has no require/dofile
 * (poisoned in user_layer/lua_runtime/src/sandbox.rs) and the §6.4 loader
 * contract vouches for the WHOLE entry via manifest.sha256, so every package
 * embeds the block verbatim between
 *
 *   -- BEGIN common/decimal.lua
 *   ...byte-for-byte the source file...
 *   -- END common/decimal.lua
 *
 * This tool rewrites each package's marked region from the source and
 * re-signs the package manifest's sha256 (the loader refuses a stale one).
 * `--check` (the default) verifies without writing — that IS the CI gate's
 * verdict pass; `lua-decimal-check.mjs` is the gate with the reverse
 * acceptance (--teeth).
 *
 * Usage:
 *   node scripts/lua-decimal-sync.mjs            # check only
 *   node scripts/lua-decimal-sync.mjs --apply    # rewrite + re-sign
 *
 * Exit: 0 clean / 1 divergence.
 */

import { readFileSync, writeFileSync, readdirSync, existsSync } from 'fs';
import { createHash } from 'crypto';
import { join, resolve, dirname } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
// BK_LUA_DIR: check an ALTERNATE strategies_lua tree (the --teeth harness
// points this at a throwaway sandbox; the default stays the working tree).
const LUA_DIR = process.env.BK_LUA_DIR
  ? resolve(process.env.BK_LUA_DIR)
  : join(ROOT, 'user_layer', 'strategies_lua');
// The source lives INSIDE the checked tree: a sandbox that drops common/
// must fail here, not silently verify against the working tree's copy.
const SOURCE = join(LUA_DIR, 'common', 'decimal.lua');
const BEGIN = '-- BEGIN common/decimal.lua';
const END = '-- END common/decimal.lua';

const APPLY = process.argv.includes('--apply');
const source = readFileSync(SOURCE, 'utf8');

// Packages that carry NO decimal block at all — currently none reference
// POW10/dec_* (the no-stop-loss scanner and this gate both verify). A new
// package that starts using exact decimals must either embed the block or be
// listed here with a written reason.
const EMBED_EXEMPT = new Set(['lua_momentum']);

// common/ itself is a SOURCE dir, not a package: no manifest.json, and the
// loader's discover_lua_packages only registers dirs carrying manifest.json.
// Dot-dirs (`.mimosa` hook state) and anything without a strategy.lua entry
// are not packages either.
const packages = readdirSync(LUA_DIR, { withFileTypes: true })
  .filter((e) => e.isDirectory() && !e.name.startsWith('.') && e.name !== 'common')
  .filter((e) => existsSync(join(LUA_DIR, e.name, 'strategy.lua')))
  .map((e) => e.name)
  .sort();

let red = false;
const stamp = (msg) => { console.error(msg); red = true; };

for (const pkg of packages) {
  const entryPath = join(LUA_DIR, pkg, 'strategy.lua');
  const manifestPath = join(LUA_DIR, pkg, 'manifest.json');
  const text = readFileSync(entryPath, 'utf8');

  if (EMBED_EXEMPT.has(pkg)) {
    if (/\bdec_(parse|fmt|rescale|cmp|add|sub|mul|floor2|round2|param)\b|\bPOW10\b/.test(text)) {
      stamp(`${pkg}: listed as decimal-exempt but uses decimal helpers — remove the exemption or embed the block`);
    } else {
      console.log(`${pkg}: decimal-exempt (no decimal code), skipped`);
    }
    continue;
  }

  // Line-anchored matching: the source's own header comment MENTIONS the
  // markers, so a bare indexOf(BEGIN/END) would find the prose, not the
  // markers. Only a line consisting of exactly the marker counts.
  const beginIdx = text.indexOf(BEGIN);
  const endIdx = text.search(new RegExp(`^${END.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`, 'm'));
  if (beginIdx === -1 || endIdx === -1 || endIdx < beginIdx) {
    stamp(`${pkg}: no ${BEGIN}/${END} markers — the package predates the block or dropped them`);
    continue;
  }

  // The embedded region is markers + source, joined by exactly one blank line.
  const want = `${BEGIN}\n${source}\n${END}`;
  const have = text.slice(beginIdx, endIdx + END.length);
  if (have !== want) {
    if (APPLY) {
      const next = text.slice(0, beginIdx) + want + text.slice(endIdx + END.length);
      const sha = createHash('sha256').update(next).digest('hex');
      const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
      manifest.sha256 = sha;
      writeFileSync(entryPath, next);
      writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
      console.log(`${pkg}: re-embedded + re-signed ${sha.slice(0, 12)}`);
    } else {
      stamp(`${pkg}: embedded block diverges from common/decimal.lua — run: node scripts/lua-decimal-sync.mjs --apply`);
    }
  }

  // Re-signed OR verified: the manifest must match the entry ON DISK. In
  // --apply mode a byte-identical block under a stale sha (an entry edit
  // outside the block) still gets the re-sign — this is the only fixer.
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  const actual = createHash('sha256').update(readFileSync(entryPath)).digest('hex');
  if (manifest.sha256 !== actual) {
    if (APPLY) {
      manifest.sha256 = actual;
      writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + '\n');
      console.log(`${pkg}: re-signed ${actual.slice(0, 12)}`);
    } else {
      stamp(`${pkg}: manifest sha256 is stale for the entry on disk (loader would refuse it)`);
    }
  }
}

if (red) {
  console.error('\nlua-decimal-sync: RED — see above; fix with --apply, never by editing a copy');
  process.exit(1);
}
console.log(`lua-decimal-sync: ${packages.length} packages, embedded blocks byte-identical to common/decimal.lua`);
