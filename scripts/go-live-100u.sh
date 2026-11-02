#!/usr/bin/env bash
# Launch the 100 USDC live profile only after the dry-run trade log passes
# scripts/soak-72h-check.mjs.
#
# A failing check exits before the kernel is started. .env.example is not
# modified and keeps DRY_RUN=true. DRY_RUN=false is applied only to the
# child process, together with --mode live.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TRADES="${1:-$ROOT/data/trades/trades.jsonl}"
if [[ ! -f "$TRADES" ]]; then
  echo "go-live-100u: no trade log at $TRADES" >&2
  echo "  run scripts/run-72h-dry.sh first, then pass its trades jsonl" >&2
  exit 1
fi

echo "go-live-100u: scoring $TRADES"
if ! node "$ROOT/scripts/soak-72h-check.mjs" --trades "$TRADES"; then
  echo "go-live-100u: soak check failed. Not starting." >&2
  exit 1
fi

BIN="${BK_CORE_BIN:-$ROOT/target/release/blitzkrieg-core}"
if [[ ! -x "$BIN" ]]; then
  echo "go-live-100u: check passed but $BIN is missing. Not starting." >&2
  exit 2
fi

# Read the live profile's numeric caps without sourcing it into this shell,
# so a refusal above never leaves DRY_RUN=false in the parent environment.
val() {
  local key="$1"
  local line
  line="$(grep -E "^${key}=" "$ROOT/user_layer/configs/production-live-100u.env" | head -1 || true)"
  printf '%s' "${line#*=}"
}

echo "go-live-100u: starting live with the 100 USDC profile and postonly_pair"
exec env \
  DRY_RUN=false \
  BK_CONFIG=user_layer/configs/production-live-100u.toml \
  BK_MAX_DAILY_LOSS="$(val BK_MAX_DAILY_LOSS)" \
  BK_MAX_DAILY_LOSS_PCT="$(val BK_MAX_DAILY_LOSS_PCT)" \
  BK_SIZE_PCT="$(val BK_SIZE_PCT)" \
  BK_MAX_ORDER_NOTIONAL_PCT="$(val BK_MAX_ORDER_NOTIONAL_PCT)" \
  BK_MAX_ORDERBOOK_STALE_MS="$(val BK_MAX_ORDERBOOK_STALE_MS)" \
  "$BIN" \
  --mode live \
  --engine \
  --no-strategy-state \
  --enable-strategy postonly_pair \
  --config "$ROOT/user_layer/configs/production-live-100u.toml" \
  --event-archive "$ROOT/data/archive/events.jsonl" \
  --trade-log "$ROOT/data/trades/trades-live.jsonl" \
  --min-shares "$(val BK_LIVE_MIN_SHARES)" \
  --max-shares "$(val BK_LIVE_MAX_SHARES)" \
  --max-order-notional "$(val BK_LIVE_MAX_ORDER_NOTIONAL)" \
  --max-order-notional-pct "$(val BK_MAX_ORDER_NOTIONAL_PCT)" \
  --size-pct "$(val BK_SIZE_PCT)"
