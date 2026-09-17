#!/usr/bin/env bash
# tui-demo.sh — one command to see the ratatui panel against a dry core.
#
#   scripts/tui-demo.sh              # temporary scratch socket, DRY + live public feed
#   scripts/tui-demo.sh --manage     # panel gains start/stop lifecycle commands
#
# forcibly DRY: the core is always spawned with --mode dry, and manage-mode
# start commands are still dry-run sessions. Kills the spawned core on exit.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=./target/release/blitzkrieg-core
PANEL=./target/release/ui_kit_panel
[ -x "$BIN" ] || cargo build --release --workspace --locked
[ -x "$PANEL" ] || cargo build --release --manifest-path ui/ui_kit_panel/Cargo.toml
SOCK="${TMPDIR:-/tmp}/blitzkrieg-tui-demo-$$-$RANDOM.sock"
rm -f "$SOCK"
"$BIN" --socket "$SOCK" --mode dry --engine --feed-ws --tick-ms 100 \
  --seed-balance 1000 --max-order-notional 6 --no-trade-log --no-order-log \
  --no-position-log &
CORE_PID=$!
cleanup() { kill "$CORE_PID" 2>/dev/null || true; rm -f "$SOCK"; }
trap cleanup EXIT
for _ in $(seq 1 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { echo "core socket never appeared" >&2; exit 2; }
echo "dry core on $SOCK (pid $CORE_PID) — quitting the panel stops it."
"$PANEL" --socket "$SOCK" "$@"
