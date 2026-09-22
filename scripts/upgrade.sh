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
#   5. stage the whole release (binaries, cdylibs, panel bundle, config files) and
#      report what differs from what is running, before anything is stopped
#   6. `blitzkrieg stop` (idempotent) and wait for the core to actually exit
#   7. install it, keeping the previous set as the rollback point, and verify
#      byte-for-byte that it landed
#   8. `nohup blitzkrieg run`, then verify IDENTITY (the new build answers) with a
#      deadline; on failure, restore the previous set and start it again
#
# `--check` runs steps 0–5 only: build + gates + stage, no live-stack restart.
#
# WHAT A RELEASE CARRIES (#252, #244). The build happens in target/bk-main-build
# but the kernel RUNS from the main checkout, and it reads more there than the
# binaries — so each of these used to stay at the previous version after a
# successful upgrade, which is how a green deploy kept behaving like the old build:
#   * user_layer/configs/*.toml — the factory values (e.g. shadow_evolution's
#     enabled / auto_evolve). These are shipped defaults, not the operator's
#     surface: the runtime switches persist in data/evolution/state.json and
#     outrank the file. A locally edited copy is backed up under the rollback
#     directory and named in the log before it is replaced.
#   * user_layer/*/target/release/*.dylib — the strategy code the kernel
#     `dlopen`s out of the running checkout. A gate that drives the previous
#     build of a strategy is not evidence (scripts/lib/strategy-dylib-freshness.mjs).
#   * ui/webapp/webui/dist — the panel bundle the launcher serves from the
#     exe-relative path; a Rust-only upgrade kept serving the previous JS.
# scripts/lib/upgrade-artifacts.sh owns all three (and is what
# scripts/upgrade-propagate-test.sh exercises without a build).
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

# The launcher, the kernel, the venue extension the kernel dlopens, and the
# terminal panel binary this checkout ships. All four are built by the workspace.
ARTIFACTS="blitzkrieg blitzkrieg-core libpolymarket_extension.dylib ui_kit_panel"
# Checkout-relative trees that ship with a release but are not built here (#252).
DYLIB_DIRS="user_layer/strategies/target/release user_layer/parity_strategy/target/release"
CONFIG_DIR="user_layer/configs"
DIST_DIR="ui/webapp/webui/dist"

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

# The propagation helpers read this contract from the environment; keeping it in
# variables here is what makes them testable on their own.
REPO_ROOT="$repo_root"
BUILD_WT="$build_wt"
export REPO_ROOT BUILD_WT ARTIFACTS DYLIB_DIRS CONFIG_DIR DIST_DIR
lib="$repo_root/scripts/lib/upgrade-artifacts.sh"
[ -f "$lib" ] || die "$lib is missing — this checkout is incomplete"
. "$lib"

step "0. data/ light backup (rollback point)"
sh scripts/data-backup-cli.sh --light

step "1. fetch ceer, resolve source"
git fetch ceer --quiet
SRC_SHA=$(git rev-parse "$SOURCE_REF")
# --short=12 because that is what the build stamps into the binary: build.rs runs
# `git rev-parse --short=12 HEAD`. A plain `--short` (7) can never equal the
# `g<sha>` in `.core-lock`, which would turn the identity check in step 8 into a
# guaranteed failure — every upgrade would build, install, then roll itself back.
SRC_SHORT=$(git rev-parse --short=12 "$SRC_SHA")
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
  # The panel serves whatever dist/ the build writes, and step 5 installs it into
  # the running checkout — a Rust-only upgrade shipped a stale panel (#244).
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

step "5. stage the release and report what it changes"
STAGE="$repo_root/target/upgrade-stage-$(date +%Y%m%dT%H%M%S)"
export STAGE
ua_stage
ua_drift
echo "staged $(wc -l < "$STAGE/SHA256SUMS" | tr -d ' ') files at $STAGE"

if [ "$mode" = "check" ]; then
  printf '\nUPGRADE --check OK (built %s, gates green, release staged; live stack untouched)\n' \
    "$SRC_SHORT"
  exit 0
fi

step "6. stop the running stack (idempotent) and wait for the core to exit"
"$release/blitzkrieg" stop || true
lock="$repo_root/data/trades/.core-lock"
pid=""
i=0
while [ "$i" -lt 30 ]; do
  pid=$(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$lock" 2>/dev/null || true)
  if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then break; fi
  sleep 1
  i=$((i + 1))
done
[ "$i" -lt 30 ] || die "the previous core is still alive after 30s (pid $pid) — refusing to swap under a running stack"
echo "stack stopped"

step "7. install the release (previous versions kept as the rollback point)"
ROLLBACK="$repo_root/target/rollback-$(date +%Y%m%dT%H%M%S)"
export ROLLBACK
mkdir -p "$ROLLBACK"
printf '%s\n' "$ROLLBACK" > "$repo_root/target/.last-rollback"
ua_prune_rollbacks
ua_install
ua_verify
echo "installed: $(echo $ARTIFACTS | tr ' ' ',') + $(wc -l < "$STAGE/dylibs.list" | tr -d ' ') cdylibs + the panel bundle + $CONFIG_DIR"
echo "rollback point: $ROLLBACK"

# Restore the previous set and bring it back up. Used by every failure from here
# on: the stack must never be left stopped by a failed upgrade (#244).
rollback_and_die() {
  printf 'ERROR: %s\n' "$1" >&2
  tail -30 "$RUN_LOG" >&2 || true
  "$release/blitzkrieg" stop || true
  sleep 2
  ua_rollback
  : > "$RUN_LOG"
  nohup "$release/blitzkrieg" run >> "$RUN_LOG" 2>&1 &
  sleep 10
  old=$(sed -n 's/.*"version":"\([^"]*\)".*/\1/p' "$lock" 2>/dev/null || true)
  die "$2; the running core is now ${old:-unknown} (rollback: $ROLLBACK)"
}

step "8. start the stack and verify identity within ${READY_DEADLINE}s"
: > "$RUN_LOG"
nohup "$release/blitzkrieg" run >> "$RUN_LOG" 2>&1 &
ready=""
i=0
while [ "$i" -lt "$READY_DEADLINE" ]; do
  # Identity, not liveness: the freshly written .core-lock names the build that is
  # running, so this proves the SWAP took effect rather than that some core is up.
  # `self-check balance: ok` is deliberately not used — the balance probe is
  # emitted by the live venue bridge, so a dry stack never logs it (#244).
  # `[0-9a-f]*` after the token keeps this a prefix match on the sha rather than an
  # exact-length one: a future change to the stamp length must not be able to
  # silently turn this gate back into a guaranteed failure.
  if grep -q "\"version\":\"[^\"]*g$SRC_SHORT[0-9a-f]*\"" "$lock" 2>/dev/null; then
    ready="$SRC_SHORT"
    break
  fi
  sleep 2
  i=$((i + 2))
done
code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:51888/panel/ 2>/dev/null || echo 000)

if [ -z "$ready" ]; then
  rollback_and_die \
    "the new build ($SRC_SHORT) is not the running core after ${READY_DEADLINE}s" \
    "the upgrade was rolled back"
fi

grep -E '\[core\]' "$RUN_LOG" | tail -3 || true
printf 'core: %s (identity verified via %s)\n' "$ready" "$lock"
# The switches the operator cares about, as the kernel resolved them: a deploy that
# shipped a new config file must be able to show that the value moved.
grep -E 'config shadow_evolution\.(enabled|auto_evolve)=' "$RUN_LOG" | tail -4 || true
if [ -f "$repo_root/data/evolution/state.json" ]; then
  printf 'evolution switches (runtime state, outranks the file): %s\n' \
    "$(cat "$repo_root/data/evolution/state.json")"
fi
printf 'panel: HTTP %s on 127.0.0.1:51888/panel/\n' "$code"
case "$code" in
  200|302|401) ;;
  *) echo "note: the panel probe answered $code — check the auth/port config before trading" >&2 ;;
esac

printf '\nUPGRADE OK — production now runs %s (rollback: %s)\n' "$SRC_SHORT" "$ROLLBACK"
