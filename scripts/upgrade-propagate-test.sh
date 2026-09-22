#!/bin/sh
# upgrade-propagate-test.sh — exercise scripts/lib/upgrade-artifacts.sh (#252).
#
# The propagation half of an upgrade is the half that used to be missing, and it
# is also the half nobody can test by running `blitzkrieg upgrade` (that needs a
# network, a release build, and a live stack). The helpers in the library work on
# directories the caller names, so this test drives the WHOLE install/verify/
# rollback path against two throwaway trees — no cargo, no npm, no network.
#
# What it asserts:
#   1. staging copies binaries, every strategy cdylib, the panel bundle and the
#      config files, and refuses to stage an empty cdylib set;
#   2. the drift report names what differs (`~`) and what does not (`=`);
#   3. installing replaces the running copies byte-for-byte;
#   4. a tampered install is caught by ua_verify before the stack is restarted;
#   5. rollback restores the previous binaries, cdylibs, bundle AND configs —
#      including a config the operator had edited locally;
#   6. the rollback pruner keeps the three newest sets and nothing else.
#
# Run it from anywhere: `sh scripts/upgrade-propagate-test.sh` (exit 0 = pass).

set -eu

here=$(cd "$(dirname "$0")" && pwd)
lib="$here/lib/upgrade-artifacts.sh"
[ -f "$lib" ] || { echo "missing $lib" >&2; exit 2; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bk-upgrade-propagate-XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

pass=0
fail=0
ok() { printf '  ok   %s\n' "$1"; pass=$((pass + 1)); }
no() {
  printf '  FAIL %s\n         expected: %s\n         actual:   %s\n' "$1" "$2" "$3" >&2
  fail=$((fail + 1))
}
check() { if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "$2" "$3"; fi; }
check_same() { # check_same <name> <expected file> <actual file>
  if cmp -s "$2" "$3"; then ok "$1"; else no "$1" "same bytes as $2" "differs from $2"; fi
}

ua_die() { printf 'ERROR: %s\n' "$1" >&2; exit 1; }

# ── two trees: what is running, and what the build produced ──────────────────
REPO_ROOT="$tmp/repo"
BUILD_WT="$tmp/build"
ARTIFACTS="blitzkrieg blitzkrieg-core libpolymarket_extension.dylib ui_kit_panel"
DYLIB_DIRS="user_layer/strategies/target/release user_layer/parity_strategy/target/release"
CONFIG_DIR="user_layer/configs"
DIST_DIR="ui/webapp/webui/dist"
STAGE="$tmp/stage"
ROLLBACK="$tmp/rollback"
export REPO_ROOT BUILD_WT ARTIFACTS DYLIB_DIRS CONFIG_DIR DIST_DIR STAGE ROLLBACK

mkdir -p "$REPO_ROOT/target/release" "$REPO_ROOT/$CONFIG_DIR" \
  "$REPO_ROOT/ui/webapp/webui/dist" "$BUILD_WT/target/release" \
  "$BUILD_WT/$CONFIG_DIR" "$BUILD_WT/ui/webapp/webui/dist"

for f in $ARTIFACTS; do
  printf 'OLD %s\n' "$f" > "$REPO_ROOT/target/release/$f"
  printf 'NEW %s\n' "$f" > "$BUILD_WT/target/release/$f"
done
# Two cdylibs exist on both sides; one is new in the build and must ship too.
for d in $DYLIB_DIRS; do
  mkdir -p "$REPO_ROOT/$d" "$BUILD_WT/$d"
done
printf 'OLD spread\n' > "$REPO_ROOT/user_layer/strategies/target/release/libspread_arb_strategy.dylib"
printf 'NEW spread\n' > "$BUILD_WT/user_layer/strategies/target/release/libspread_arb_strategy.dylib"
printf 'OLD parity\n' > "$REPO_ROOT/user_layer/parity_strategy/target/release/libparity_strategy.dylib"
printf 'NEW parity\n' > "$BUILD_WT/user_layer/parity_strategy/target/release/libparity_strategy.dylib"
printf 'NEW trend\n' > "$BUILD_WT/user_layer/strategies/target/release/libtrend_follow_strategy.dylib"

printf '<html>OLD</html>\n' > "$REPO_ROOT/$DIST_DIR/index.html"
printf '<html>NEW</html>\n' > "$BUILD_WT/$DIST_DIR/index.html"
printf 'assets\n' > "$BUILD_WT/$DIST_DIR/bundle.js"

printf 'round_sec = 900\n' > "$BUILD_WT/$CONFIG_DIR/default.toml"
printf 'round_sec = 900\n' > "$REPO_ROOT/$CONFIG_DIR/default.toml"
# A config the operator edited by hand: it must be backed up, not silently lost.
printf 'auto_evolve = true\n' > "$BUILD_WT/$CONFIG_DIR/shadow_evolution.toml"
printf 'auto_evolve = false  # hand-edited locally\n' > "$REPO_ROOT/$CONFIG_DIR/shadow_evolution.toml"
cp "$REPO_ROOT/$CONFIG_DIR/shadow_evolution.toml" "$tmp/operator-copy.toml"
cp "$REPO_ROOT/target/release/blitzkrieg" "$tmp/old-blitzkrieg"

. "$lib"

# ── 1. staging ───────────────────────────────────────────────────────────────
printf '\nstaging\n'
ua_stage
check "binaries staged" "4" "$(ls "$STAGE/bin" | wc -l | tr -d ' ')"
check "cdylibs staged (2 shipped + 1 new)" "3" "$(wc -l < "$STAGE/dylibs.list" | tr -d ' ')"
check "configs staged" "2" "$(ls "$STAGE/configs" | wc -l | tr -d ' ')"
check_same "bundle staged" "$BUILD_WT/$DIST_DIR/index.html" "$STAGE/dist/index.html"
check_same "nested bundle file staged" "$BUILD_WT/$DIST_DIR/bundle.js" "$STAGE/dist/bundle.js"
# 4 binaries + 3 cdylibs + dylibs.list + 2 configs + 2 bundle files.
check "checksums written" "12" "$(wc -l < "$STAGE/SHA256SUMS" | tr -d ' ')"
if ( BUILD_WT="$tmp/empty-build" ua_stage ) >/dev/null 2>&1; then
  no "an empty build is refused" "non-zero exit" "exit 0"
else
  ok "an empty build is refused"
fi

# ── 2. drift report ──────────────────────────────────────────────────────────
printf '\ndrift report\n'
ua_drift > "$tmp/drift.txt"
check "drift: unchanged config marked =" "1" "$(grep -c '= default.toml' "$tmp/drift.txt" | tr -d ' ')"
check "drift: edited config marked ~" "1" "$(grep -c '~ shadow_evolution.toml' "$tmp/drift.txt" | tr -d ' ')"
check "drift: rebuilt cdylib marked ~" "2" "$(grep -c '~ lib.*_strategy.dylib' "$tmp/drift.txt" | tr -d ' ')"
check "drift: new cdylib marked +" "1" "$(grep -c '+ libtrend_follow_strategy.dylib' "$tmp/drift.txt" | tr -d ' ')"

# ── 3. install + verify ──────────────────────────────────────────────────────
printf '\ninstall\n'
ua_install
ua_verify
check_same "binary replaced" "$STAGE/bin/blitzkrieg-core" "$REPO_ROOT/target/release/blitzkrieg-core"
check_same "cdylib replaced" "$STAGE/dylibs/user_layer/strategies/target/release/libspread_arb_strategy.dylib" \
  "$REPO_ROOT/user_layer/strategies/target/release/libspread_arb_strategy.dylib"
check_same "new cdylib installed" "$STAGE/dylibs/user_layer/strategies/target/release/libtrend_follow_strategy.dylib" \
  "$REPO_ROOT/user_layer/strategies/target/release/libtrend_follow_strategy.dylib"
check_same "bundle replaced" "$STAGE/dist/index.html" "$REPO_ROOT/$DIST_DIR/index.html"
check "bundle holds exactly the staged set" "2" "$(ls "$REPO_ROOT/$DIST_DIR" | wc -l | tr -d ' ')"
check_same "config replaced by the shipped value" "$STAGE/configs/shadow_evolution.toml" \
  "$REPO_ROOT/$CONFIG_DIR/shadow_evolution.toml"
check_same "the operator's edited config is kept for rollback" "$tmp/operator-copy.toml" \
  "$ROLLBACK/configs/shadow_evolution.toml"
check_same "the previous binary is kept for rollback" "$tmp/old-blitzkrieg" "$ROLLBACK/bin/blitzkrieg"

# ── 4. a tampered install must not reach the restart ─────────────────────────
printf '\nverification\n'
printf 'truncated\n' > "$REPO_ROOT/target/release/blitzkrieg-core"
if ( ua_verify ) >/dev/null 2>&1; then
  no "a half-written binary is caught" "non-zero exit" "exit 0"
else
  ok "a half-written binary is caught"
fi
cp "$STAGE/bin/blitzkrieg-core" "$REPO_ROOT/target/release/blitzkrieg-core"
ua_verify

# ── 5. rollback restores everything, including the operator's config ─────────
printf '\nrollback\n'
ua_rollback
check_same "binary restored" "$tmp/old-blitzkrieg" "$REPO_ROOT/target/release/blitzkrieg"
check "cdylib restored" "OLD spread" "$(cat "$REPO_ROOT/user_layer/strategies/target/release/libspread_arb_strategy.dylib")"
check "the cdylib the install ADDED is taken away again" "0" \
  "$(ls "$REPO_ROOT/user_layer/strategies/target/release" | grep -c trend_follow || true)"
check "bundle index restored" "<html>OLD</html>" "$(cat "$REPO_ROOT/$DIST_DIR/index.html")"
check_same "the operator's edited config is restored" "$tmp/operator-copy.toml" \
  "$REPO_ROOT/$CONFIG_DIR/shadow_evolution.toml"

# ── 6. the rollback pruner keeps the three newest ────────────────────────────
printf '\nrollback pruner\n'
for n in 1 2 3 4 5; do
  mkdir -p "$REPO_ROOT/target/rollback-2026010${n}T000000"
  sleep 0.05
done
mkdir -p "$REPO_ROOT/target/keepme-20260101T000000"
ua_prune_rollbacks
check "three newest rollback sets survive" "3" \
  "$(ls -1d "$REPO_ROOT"/target/rollback-* | wc -l | tr -d ' ')"
check "the survivors are the three newest" "rollback-20260103T000000 rollback-20260104T000000 rollback-20260105T000000" \
  "$(ls -1d "$REPO_ROOT"/target/rollback-* | xargs -n1 basename | sort | tr '\n' ' ' | sed 's/ $//')"
check "an unrelated target/ dir is untouched" "1" \
  "$(ls -1d "$REPO_ROOT"/target/keepme-* | wc -l | tr -d ' ')"

printf '\n%s\n' "upgrade-propagate: $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
