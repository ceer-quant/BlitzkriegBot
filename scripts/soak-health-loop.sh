#!/usr/bin/env bash
# soak-health-loop.sh — run the zero-token health check on a timer, forever.
#
# This is the FREE replacement for the LLM-driven periodic health check: it costs
# no API tokens, runs 24/7, and needs no OS scheduler (launchd/cron cannot access
# this repo's external volume — see docs/COST_OPTIMIZATION.md). Run it detached:
#
#   nohup scripts/soak-health-loop.sh --interval-sec 1800 \
#       >> data/soak/health-loop.out 2>&1 &
#
# Behaviour each tick:
#   * run scripts/soak-health.sh --quiet
#   * append a timestamped result line to data/soak/health.log
#   * on ANOMALY: write data/soak/HEALTH_ALERT (small, human-readable); on
#     recovery: remove it. So an agent/you can check one file cheaply:
#         test -f data/soak/HEALTH_ALERT && cat data/soak/HEALTH_ALERT
#
# Stop:  pkill -f soak-health-loop

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

INTERVAL=1800
while [ $# -gt 0 ]; do
  case "$1" in
    --interval-sec) INTERVAL="${2:-1800}"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[ "$INTERVAL" -ge 30 ] 2>/dev/null || INTERVAL=30

LOG=data/soak/health.log
ALERT=data/soak/HEALTH_ALERT
mkdir -p data/soak

running=1
trap 'running=0' TERM INT
echo "soak-health-loop: interval=${INTERVAL}s pid=$$ root=$ROOT"

while [ "$running" -eq 1 ]; do
  # Bound run.log so it can never grow into the hundreds of MB (a huge log is a
  # token hazard: any full read is re-sent every later turn). No-op when small.
  ./scripts/rotate-run-log.sh >> "$LOG" 2>&1

  out=$(./scripts/soak-health.sh --quiet 2>&1)
  code=$?
  ts=$(date '+%Y-%m-%d %H:%M:%S')
  echo "[$ts] $out" >> "$LOG"
  if [ "$code" -ne 0 ]; then
    { echo "[$ts] ANOMALY detected by soak-health.sh"; echo "$out"; echo; } > "$ALERT"
  else
    [ -f "$ALERT" ] && rm -f "$ALERT"
  fi
  # sleep in 1s slices so a TERM is honoured promptly
  i=0
  while [ "$i" -lt "$INTERVAL" ] && [ "$running" -eq 1 ]; do
    sleep 1
    i=$((i + 1))
  done
done
echo "soak-health-loop: exiting (pid $$)"
