#!/usr/bin/env bash
# Launch the 100 USDC live profile only after soak-72h-check accepts the
# dry-run trade log.
#
# macOS bash 3.2 / zsh, BSD userland. A failing check exits before the kernel
# starts. .env.example is not modified and keeps DRY_RUN=true. DRY_RUN=false
# is applied only to the child. On Darwin the live process is wrapped in
# caffeinate -ims, same as the 72h dry soak.
set -eu
if [ -n "${BASH_VERSION:-}" ]; then
  set -o pipefail
elif [ -n "${ZSH_VERSION:-}" ]; then
  setopt pipefail
fi

script_file() {
  if [ -n "${ZSH_VERSION:-}" ]; then
    eval 'printf %s "${(%):-%x}"'
  elif [ -n "${BASH_VERSION:-}" ]; then
    printf %s "${BASH_SOURCE[0]}"
  else
    printf %s "$0"
  fi
}

ROOT="$(cd "$(dirname "$(script_file)")/.." && pwd -P)"
cd "$ROOT"

TRADES="${1:-$ROOT/data/trades/trades.jsonl}"
if [ ! -f "$TRADES" ]; then
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
if [ ! -x "$BIN" ]; then
  echo "go-live-100u: check passed but $BIN is missing. Not starting." >&2
  exit 2
fi

# Read caps without sourcing the live env into this shell.
val() {
  key="$1"
  line="$(grep -E "^${key}=" "$ROOT/user_layer/configs/production-live-100u.env" | head -n 1 || true)"
  printf '%s' "${line#*=}"
}

echo "go-live-100u: starting live with the 100 USDC profile and postonly_pair"
if [ "$(uname -s)" = "Darwin" ]; then
  echo "  caffeinate -ims is on. Plug into AC power."
fi

set -- env \
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

if [ "$(uname -s)" = "Darwin" ] && [ -x /usr/bin/caffeinate ]; then
  exec /usr/bin/caffeinate -ims "$@"
fi
exec "$@"
