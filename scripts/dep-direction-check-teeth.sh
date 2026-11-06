#!/bin/sh
# dep-direction-check-teeth.sh — negative fixtures for dep-direction-check (#844).
#
# The gate can only be trusted if it is proven to go RED on the boundary
# violations it claims to police (§A.4 order: prove the teeth, then the
# real verdict). Each arm builds a scratch workspace (its own Cargo.toml,
# no repo file touched), runs the GUARD ITSELF over it, and asserts BOTH
# the exit code and the exact offending text — a red must be an answer:
#
#   arm 1  R1  core -> concrete plugin (non-optional)          core > plugin
#   arm 2  R1  core -> concrete UI on a normal (shipped) edge  core > ui
#   arm 3  R2  plugin -> core                                  plugin > core
#   arm 4  R2  plugin -> another concrete plugin               plugin > plugin
#   arm 5  R4  three-crate cycle, exact path named             a > b > c > a
#   arm 6  R3  serde wire DTO redefined outside bk_contract    type named
#   arm 7  R3  dead (never-declared) copy in market_api shape  NOT flagged
#          — the module-tree walk must not collect files no `mod` reaches,
#          so a plan to delete the orphan copies stays exactly that.
#   arm 8  green control: the scratch tree mirrors the real boundaries and
#          the guard stays green there (proves the reds above are the
#          INJECTED edges, not the harness shape).
#
# The guard's judge functions are driven in-process over synthetic edge
# lists / crate trees via `--fixture-*` modes — cargo never resolves the
# scratch workspaces (a workspace cycle cannot even be parsed by cargo, so
# the cycle arm is necessarily judge-level; the manifest-level arms 1/2/3/4
# go through the real metadata judge path via --fixture-errs over raw JSON).
#
# Run: sh scripts/dep-direction-check-teeth.sh   (exit 0 = all teeth bite)
# No build, no network, milliseconds.

set -u

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
GUARD="$SCRIPT_DIR/dep-direction-check.mjs"

pass=0
fail=0

check() { # name expected actual
  if [ "$2" = "$3" ]; then pass=$((pass+1)); printf 'ok    %s\n' "$1"
  else fail=$((fail+1)); printf 'RED   %s\n  expected: %s\n  actual:   %s\n' "$1" "$2" "$3"; fi
}

command -v node >/dev/null 2>&1 || { echo "node not on PATH" >&2; exit 2; }
[ -f "$GUARD" ] || { echo "missing $GUARD" >&2; exit 2; }

# ── arm 1-4: manifest-level rule teeth over a synthetic metadata graph ────────
# `dep-direction-check.mjs --fixture-edges '<json>'` runs judgeEdges+findCycles
# over the given edges with lanes resolved from a name->lane map printed by the
# same flag (the mode exists ONLY for this teeth script — the real verdict
# always comes from cargo metadata).

# arm 1: core -> concrete plugin, non-optional.
out=$(node "$GUARD" --fixture-edges '[
 {"from":"blitzkrieg-core","to":"fancy-plugin","kinds":["normal"],"optional":false}]' 2>&1); rc=$?
check "arm 1 (core->plugin) exits 1" 1 "$rc"
check "arm 1 names the exact edge" \
  "yes" "$(printf '%s' "$out" | grep -q 'core -> concrete plugin is forbidden' && printf '%s' "$out" | grep -q 'blitzkrieg-core -> fancy-plugin' && echo yes || echo no)"

# arm 2: core -> concrete UI on a NORMAL edge (the dev-seam rot direction).
out=$(node "$GUARD" --fixture-edges '[
 {"from":"blitzkrieg-core","to":"blitzkrieg-ui-kit","kinds":["normal"],"optional":false}]' 2>&1); rc=$?
check "arm 2 (core->UI shipped) exits 1" 1 "$rc"
check "arm 2 names the dev-seam-only violation" \
  "yes" "$(printf '%s' "$out" | grep -q 'dev-seam-only' && echo yes || echo no)"

# arm 3: plugin -> core.
out=$(node "$GUARD" --fixture-edges '[
 {"from":"fancy-plugin","to":"blitzkrieg-core","kinds":["dev"],"optional":false}]' 2>&1); rc=$?
check "arm 3 (plugin->core, even dev) exits 1" 1 "$rc"
check "arm 3 names plugin -> core" \
  "yes" "$(printf '%s' "$out" | grep -q 'plugin -> core is forbidden' && printf '%s' "$out" | grep -q 'fancy-plugin -> blitzkrieg-core' && echo yes || echo no)"

# arm 4: plugin -> plugin.
out=$(node "$GUARD" --fixture-edges '[
 {"from":"polymarket-extension","to":"blitzkrieg-kalshi","kinds":["normal"],"optional":false}]' 2>&1); rc=$?
check "arm 4 (plugin->plugin) exits 1" 1 "$rc"
check "arm 4 names plugin -> plugin" \
  "yes" "$(printf '%s' "$out" | grep -q 'plugin -> another concrete plugin is forbidden' && printf '%s' "$out" | grep -q 'polymarket-extension -> blitzkrieg-kalshi' && echo yes || echo no)"

# arm 5: the three-crate CYCLE — exact offending path.
out=$(node "$GUARD" --fixture-edges '[
 {"from":"a","to":"b","kinds":["normal"],"optional":false},
 {"from":"b","to":"c","kinds":["normal"],"optional":false},
 {"from":"c","to":"a","kinds":["dev"],"optional":false}]' 2>&1); rc=$?
check "arm 5 (cycle a>b>c>a) exits 1" 1 "$rc"
check "arm 5 names the exact cycle path" \
  "yes" "$(printf '%s' "$out" | grep -q 'dependency cycle: a -> b -> c -> a' && echo yes || echo no)"

# arm 5b: green control — the same edges WITHOUT the closing edge stay green.
out=$(node "$GUARD" --fixture-edges '[
 {"from":"a","to":"b","kinds":["normal"],"optional":false},
 {"from":"b","to":"c","kinds":["normal"],"optional":false}]' 2>&1); rc=$?
check "arm 5b (acyclic same-shape) stays green" 0 "$rc"

# ── arm 6/7: DTO ownership teeth over real scratch crate trees ────────────────
tmp="$(mktemp -d "${TMPDIR:-/tmp}/dep-direction-teeth.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

mkcrate() { # dir; reads printed rs files from stdin pairs
  mkdir -p "$1/src"
}

# arm 6: contract owns BookUpdate; plugin redefines it -> violation names type + file.
mkcrate "$tmp/contract"
printf 'pub mod types;\n' > "$tmp/contract/src/lib.rs"
printf '#[derive(Debug, Clone, Serialize, Deserialize)]\n#[serde(rename_all = "camelCase")]\npub struct BookUpdate {\n    pub token_id: String,\n    pub bids: Vec<(String, String)>,\n}\n' > "$tmp/contract/src/types.rs"
mkcrate "$tmp/plugin"
printf 'pub mod host;\n' > "$tmp/plugin/src/lib.rs"
printf '#[derive(Debug, Clone, Serialize, Deserialize)]\npub struct BookUpdate {\n    pub token_id: String,\n}\n' > "$tmp/plugin/src/host.rs"

out=$(node "$GUARD" --fixture-dto "$tmp/contract/src" "plugin:$tmp/plugin/src:plugin" 2>&1); rc=$?
check "arm 6 (DTO redefined outside bk_contract) exits 1" 1 "$rc"
check "arm 6 names the wire type and the offending file" \
  "yes" "$(printf '%s' "$out" | grep -q "wire DTO 'BookUpdate' redefined outside bk_contract" && printf '%s' "$out" | grep -q 'plugin/src/host.rs' && echo yes || echo no)"

# arm 7: the SAME name in an UNREACHED file must NOT flag — the module-tree
# walk only collects what Rust actually compiles.
mkcrate "$tmp/clean"
printf 'pub mod ledger;\n' > "$tmp/clean/src/lib.rs"
printf '#[derive(Debug, Clone, Serialize, Deserialize)]\npub struct BookUpdate {\n    pub best_bid: String,\n}\n' > "$tmp/clean/src/host_orphan_dead.rs"
printf '#[derive(Debug, Clone, Serialize, Deserialize)]\npub struct LedgerRow {\n    pub id: String,\n}\n' > "$tmp/clean/src/ledger.rs"

out=$(node "$GUARD" --fixture-dto "$tmp/contract/src" "plugin-clean:$tmp/clean/src:plugin" 2>&1); rc=$?
check "arm 7 (dead-file same-name DTO) stays green" 0 "$rc"

# arm 8: green control — the scratch contract+plugin pair with DIFFERENT type
# names (the real boundaries) is green.
mkcrate "$tmp/ok"
printf 'pub mod host;\n' > "$tmp/ok/src/lib.rs"
printf '#[derive(Debug, Clone, Serialize, Deserialize)]\npub struct PluginBook {\n    pub best_bid: String,\n}\n' > "$tmp/ok/src/host.rs"
out=$(node "$GUARD" --fixture-dto "$tmp/contract/src" "plugin-ok:$tmp/ok/src:plugin" 2>&1); rc=$?
check "arm 8 (disjoint DTO names) stays green" 0 "$rc"

printf '\n%s\n' "dep-direction-check-teeth: $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
