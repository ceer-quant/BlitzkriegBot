#!/usr/bin/env bash
# Opt-in 100 USDC live launcher (issue #870).
#
# Default and --readonly: run the read-only preflight and do not start a
# kernel. --arm-orders starts a live kernel only after that preflight exits
# 0, with the empty allowlist, --no-strategy-state, and the $2 order cap.
# .env.example is never rewritten. DRY_RUN=false is applied only to the
# armed child.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
ENV_FILE="$ROOT/user_layer/configs/production-live-100u.env"
PREFLIGHT=(node "$ROOT/scripts/live-100u-preflight.mjs")

val() {
  local key="$1"
  local line
  line="$(grep -E "^${key}=" "$ENV_FILE" | head -1 || true)"
  printf '%s' "${line#*=}"
}

NOTIONAL="$(val BK_LIVE_MAX_ORDER_NOTIONAL)"
MIN_SHARES="$(val BK_LIVE_MIN_SHARES)"
MAX_SHARES="$(val BK_LIVE_MAX_SHARES)"
PCT="$(val BK_MAX_ORDER_NOTIONAL_PCT)"

case "${1:-}" in
  ""|--readonly)
    "${PREFLIGHT[@]}"
    echo "live-100u: no kernel was started. Orders were not armed."
    ;;
  --config)
    "${PREFLIGHT[@]}" --config
    ;;
  --arm-orders)
    "${PREFLIGHT[@]}"
    BIN="$ROOT/target/release/blitzkrieg-core"
    if [[ ! -x "$BIN" ]]; then
      echo "live-100u: preflight passed but ${BIN} is missing; not starting." >&2
      exit 2
    fi
    echo "live-100u: arming a live process with no strategy and a \$${NOTIONAL} order cap."
    exec env \
      DRY_RUN=false \
      BK_CONFIG=user_layer/configs/production-live-100u.toml \
      BK_MAX_DAILY_LOSS="$(val BK_MAX_DAILY_LOSS)" \
      BK_MAX_DAILY_LOSS_PCT="$(val BK_MAX_DAILY_LOSS_PCT)" \
      BK_SIZE_PCT="$(val BK_SIZE_PCT)" \
      BK_MAX_ORDER_NOTIONAL_PCT="$PCT" \
      BK_MAX_ORDERBOOK_STALE_MS="$(val BK_MAX_ORDERBOOK_STALE_MS)" \
      "$BIN" \
      --mode live \
      --no-strategy-state \
      --config "$ROOT/user_layer/configs/production-live-100u.toml" \
      --max-order-notional "$NOTIONAL" \
      --max-order-notional-pct "$PCT" \
      --min-shares "$MIN_SHARES" \
      --max-shares "$MAX_SHARES"
    ;;
  *)
    echo "live-100u: unknown argument ${1}" >&2
    exit 2
    ;;
esac
