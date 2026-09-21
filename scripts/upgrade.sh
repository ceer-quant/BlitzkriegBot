#!/bin/sh
# upgrade.sh — the 傻白甜 one-shot production upgrade (shim verb: `blitzkrieg upgrade`).
#
# Full pipeline, aborts on the first failure — never deploys a red tree, and
# never leaves the stack stopped:
#   0. data/ light backup (a few MB, seconds — the upgrade leaves a rollback point)
#   1. fetch ceer and resolve the source (default: ceer/feat/trading-safety-selfcheck,
#      override with BLITZKRIEG_UPGRADE_SOURCE)
#   2. update the production build worktree target/bk-main-build to the source
#      (detached checkout — the branch refs and the main checkout stay untouched)
#   3. build: release workspace + strategy cdylibs + the panel's webui bundle
#   4. LOCAL GATES: core lib tests AND the panel's check:all — both must be green
#   5. STAGE every artifact and checksum it, before anything is stopped
#   6. `blitzkrieg stop` (idempotent) and wait for the core to actually exit
#   7. swap the binaries in, keeping the previous set as the rollback point
#   8. `nohup blitzkrieg run`, then verify IDENTITY (the new build answers) with a
#      deadline; on failure, restore the previous set and start it again
#
# `--check` runs steps 0–4 only: build + gates, no live-stack restart.
#
# Why the shape is what it is (#244): a `cargo test … | tail -1` pipeline without
# pipefail reports tail's status, so a red suite shipped; the swap ran unlocked
# after the stop, so a failed `cp` aborted the script with the stack already down;
# a fixed `sleep 8` both raced a slow start and passed a dead one; `self-check
# balance: ok` is emitted by the LIVE venue bridge only, so in dry mode it never
# appears; and the pipeline never built the webui bundle, so the panel kept
# serving the previous JS.
#
# The build worktree target/bk-main-build is the production build's home (do
# NOT delete it); its target/ cache is what makes repeated upgrades fast.

set -eu

repo_root=$(cd "$(dirname "$0")/.." && pwd)
build_wt="$repo_root/target/bk-main-build"
release="$repo_root/target/release"
SOURCE_REF="${BLITZKRIEG_UPGRADE_SOURCE:-ceer/feat/trading-safety-selfcheck}"
RUN_LOG="/tmp/blitzkrieg-run.log"
READY_DEADLINE="${BLITZKRIEG_UPGRADE_DEADLINE:-120}"   # seconds to reach identity
# The launcher + the kernel + the venue extension the kernel dlopens. `ui_kit_panel`
# is built by the workspace but is not part of the running stack.
ARTIFACTS="blitzkrieg blitzkrieg-core libpolymarket_extension.dylib"
step() { printf '\n==> %s\n' "$1"; }
die() { printf 'ERROR: %s\n' "$1" >&2; exit 1; }

mode="ship"
case "${1:-}" in
  "") ;;
  --check) mode="check" ;;
  -h|--help) echo "usage: blitzkrieg upgrade [--check]" >&2; exit 0 ;;
  *) echo "error: unknown option: $1" >&2; exit 2 ;;
esac

cd "$repo_root"

step "0. data/ light backup (rollback point)"
sh scripts/data-backup-cli.sh --light

step "1. fetch ceer, resolve source"
git fetch ceer --quiet
SRC_SHA=$(git rev-parse "$SOURCE_REF")
SRC_SHORT=$(git rev-parse --short "$SRC_SHA")
echo "source: $SOURCE_REF @ $SRC_SHORT"

step "2. production build worktree → source"
git -C "$build_wt" fetch ceer --quiet
git -C "$build_wt" checkout --detach "$SRC_SHA"

step "3. build (release workspace + strategy cdylibs + panel bin + webui bundle)"
( cd "$build_wt" && cargo build --release --workspace --locked )
( cd "$build_wt/user_layer/strategies" && cargo build --release --locked )
( cd "$build_wt/user_layer/parity_strategy" && cargo build --release --locked )
(
  cd "$build_wt/ui/webapp/webui"
  # The panel binary serves whatever dist/ it finds next to its build tree, so a
  # Rust-only upgrade that skips this step ships a stale panel (#244).
  [ -d node_modules ] || npm ci
  npm run build
)

step "4a. local gate: blitzkrieg-core lib tests"
# NO pipeline: the status must be cargo's own. `| tail -1` reports tail's status,
# which is how a red suite shipped (#244).
if ! gate_out=$(cd "$build_wt" && CARGO_TARGET_DIR="$build_wt/target" \
    cargo test -p blitzkrieg-core --lib 2>&1); then
  printf '%s\n' "$gate_out" | tail -40 >&2
  die "blitzkrieg-core lib tests failed — nothing was stopped or shipped"
fi
printf '%s\n' "$gate_out" | grep -E '^test result' | tail -3

step "4b. local gate: panel check:all"
if ! panel_out=$(cd "$build_wt/ui/webapp/webui" && npm run check:all 2>&1); then
  printf '%s\n' "$panel_out" | grep -E 'RESULT: FAIL|error|✗' | head -20 >&2
  printf '%s\n' "$panel_out" | tail -20 >&2
  die "panel gates failed — nothing was stopped or shipped"
fi
printf '%s\n' "$panel_out" | grep -c 'RESULT: PASS' | sed 's/^/panel gates passed: /'

if [ "$mode" = "check" ]; then
  printf '\nUPGRADE --check OK (built %s, gates green; live stack untouched)\n' "$SRC_SHORT"
  exit 0
fi

step "5. stage the artifacts (nothing is stopped until every one exists)"
stage="$repo_root/target/upgrade-stage-$(date +%Y%m%dT%H%M%S)"
mkdir -p "$stage"
for f in $ARTIFACTS; do
  src="$build_wt/target/release/$f"
  [ -f "$src" ] || die "the build produced no $f at $src"
  cp "$src" "$stage/$f"
done
( cd "$stage" && shasum -a 256 $ARTIFACTS > SHA256SUMS && cat SHA256SUMS )

step "6. stop the running stack (idempotent) and wait for the core to exit"
"$release/blitzkrieg" stop || true
lock="$repo_root/data/trades/.core-lock"
i=0
while [ "$i" -lt 30 ]; do
  pid=$(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$lock" 2>/dev/null || true)
  [ -z "$pid" ] && break
  kill -0 "$pid" 2>/dev/null || break
  sleep 1; i=$((i + 1))
done
if [ "$i" -ge 30 ]; then
  die "the previous core is still alive after 30s (pid $pid) — refusing to swap under a running stack"
fi
echo "stack stopped"

step "7. swap the binaries in, previous set kept as the rollback point"
rollback="$repo_root/target/rollback-$(date +%Y%m%dT%H%M%S)"
mkdir -p "$rollback"
for f in $ARTIFACTS; do
  if [ -f "$release/$f" ]; then cp -a "$release/$f" "$rollback/$f"; fi
done
printf '%s\n' "$rollback" > "$repo_root/target/.last-rollback"

# Restore the previous set and bring it back up. Used by every failure from here
# on: the stack must never be left stopped by a failed upgrade (#244).
rollback_and_die() {
  printf 'ERROR: %s\n' "$1" >&2
  tail -30 "$RUN_LOG" >&2
  "$release/blitzkrieg" stop || true
  sleep 2
  for f in $ARTIFACTS; do
    if [ -f "$rollback/$f" ]; then cp -a "$rollback/$f" "$release/$f"; fi
  done
  : > "$RUN_LOG"
  nohup "$release/blitzkrieg" run >> "$RUN_LOG" 2>&1 &
  sleep 10
  old=$(sed -n 's/.*"version":"\([^"]*\)".*/\1/p' "$lock" 2>/dev/null || true)
  die "$2; the running core is now ${old:-unknown} (rollback: $rollback)"
}

for f in $ARTIFACTS; do
  cp "$stage/$f" "$release/$f.new"
  mv "$release/$f.new" "$release/$f"
done
hash=shasum
for f in $ARTIFACTS; do
  want=$($hash -a 256 "$stage/$f" | awk '{print $1}')
  got=$($hash -a 256 "$release/$f" | awk '{print $1}')
  [ "$want" = "$got" ] || rollback_and_die \
    "$f did not land intact in $release" "the swap failed and was undone"
done
echo "swapped: $(echo $ARTIFACTS | tr ' ' ',')  (rollback: $rollback)"

step "8. start the stack and verify identity within ${READY_DEADLINE}s"
: > "$RUN_LOG"
nohup "$release/blitzkrieg" run >> "$RUN_LOG" 2>&1 &
ready=""
i=0
while [ "$i" -lt "$READY_DEADLINE" ]; do
  # Identity, not liveness: the freshly written .core-lock names the build that is
  # running, so this is what proves the SWAP took effect rather than that some core
  # is up. `self-check` is deliberately not used: the balance probe is emitted by the
  # live venue bridge, so a dry stack never logs it (#244).
  if grep -q "\"version\":\"[^\"]*g$SRC_SHORT\"" "$lock" 2>/dev/null; then
    ready="$SRC_SHORT"; break
  fi
  sleep 2; i=$((i + 2))
done
code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:51888/panel/ 2>/dev/null || echo 000)

if [ -z "$ready" ]; then
  rollback_and_die \
    "the new build ($SRC_SHORT) is not the running core after ${READY_DEADLINE}s" \
    "the upgrade was rolled back"
fi

grep -E '\[core\]' "$RUN_LOG" | tail -3 || true
printf 'core: %s (identity verified via %s)\n' "$ready" "$lock"
printf 'panel: HTTP %s on 127.0.0.1:51888/panel/\n' "$code"
case "$code" in
  200|302|401) ;;
  *) echo "note: the panel probe answered $code — check the auth/port config before trading" >&2 ;;
esac

printf '\nUPGRADE OK — production now runs %s (rollback: %s)\n' "$SRC_SHORT" "$rollback"
