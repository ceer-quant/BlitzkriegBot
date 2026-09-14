#!/usr/bin/env bash
# offpeak.sh — DeepSeek off-peak gate.
#
# Off-peak rates are HALF the peak rates. Peak = Mon-Fri 09:00-12:00 and
# 14:00-18:00 Beijing time (UTC+8); every other hour, plus all weekend, is
# off-peak. This script tells you which window NOW falls in, so batch jobs and
# monitors can be gated to the cheap window.
#
# Usage:
#   scripts/offpeak.sh                 # print window + exit 0 (off-peak) / 1 (peak)
#   scripts/offpeak.sh --quiet         # exit code only
#   scripts/offpeak.sh --run <cmd...>  # run <cmd> only when off-peak
#
# Note: "off-peak" means cheap, not "closed" — anything that costs no API
# tokens (like scripts/soak-health.sh) is safe to run at any hour.

set -uo pipefail

# Beijing time = UTC+8, independent of the machine's local zone.
now_hm=$(TZ=Asia/Shanghai date +%H%M)
now_dow=$(TZ=Asia/Shanghai date +%u)   # 1=Mon .. 7=Sun
now=$(TZ=Asia/Shanghai date '+%Y-%m-%d %H:%M %a (Beijing)')

is_peak() {
  [ "$now_dow" -le 5 ] || return 1              # weekend → off-peak
  local h=$((10#$now_hm / 100))
  # Peak hours: 9,10,11 and 14,15,16,17
  case "$h" in
    9|10|11|14|15|16|17) return 0 ;;
    *) return 1 ;;
  esac
}

if [ "${1:-}" = "--run" ]; then
  shift
  if is_peak; then
    echo "offpeak: PEAK at $now — skipping: $*" >&2
    exit 0
  fi
  exec "$@"
fi

if is_peak; then
  [ "${1:-}" = "--quiet" ] || echo "PEAK     $now — off-peak rates start after 12:00 / 18:00"
  exit 1
fi
[ "${1:-}" = "--quiet" ] || echo "OFF-PEAK $now — half-price window"
exit 0
