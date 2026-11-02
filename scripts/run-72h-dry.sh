#!/usr/bin/env bash
# Start a dry-run soak of postonly_pair with market-data archiving.
#
# DRY_RUN stays true for this process even if the parent shell exported
# DRY_RUN=false: the child is given DRY_RUN=true and --mode dry. This script
# does not edit .env.example. Leave it running for the 72-hour window, then
# score data/trades/trades.jsonl with scripts/soak-72h-check.mjs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN="${BK_CORE_BIN:-$ROOT/target/release/blitzkrieg-core}"
if [[ ! -x "$BIN" ]]; then
  echo "run-72h-dry: missing $BIN" >&2
  echo "  build it with: cargo build --release -p blitzkrieg-core" >&2
  exit 2
fi

mkdir -p "$ROOT/data/archive" "$ROOT/data/trades"

echo "run-72h-dry: DRY_RUN=true, strategy=postonly_pair, archive=data/archive/events.jsonl"
echo "  trade log: data/trades/trades.jsonl"
echo "  stop with Ctrl-C. Score later with: node scripts/soak-72h-check.mjs --trades data/trades/trades.jsonl"

exec env DRY_RUN=true \
  "$BIN" \
  --mode dry \
  --engine \
  --no-strategy-state \
  --enable-strategy postonly_pair \
  --config "$ROOT/user_layer/configs/production-72h.toml" \
  --event-archive "$ROOT/data/archive/events.jsonl" \
  --trade-log "$ROOT/data/trades/trades.jsonl" \
  --seed-balance 100 \
  --min-shares 1 \
  --max-shares 2 \
  --max-order-notional 2 \
  --max-order-notional-pct 2 \
  --size-pct 1
