#!/usr/bin/env bash
# soak-health-loop.sh — run the zero-token health check on a timer, forever.
#
# This is the FREE replacement for an LLM-driven periodic health check: it costs
# no API tokens, runs 24/7, and needs no OS scheduler (launchd/cron cannot access
# this repo's external volume). Run it detached:
#
#   nohup scripts/soak-health-loop.sh --interval-sec 1800 \
#       >> data/soak/health-loop.out 2>&1 &
#
# Behaviour each tick:
#   * bound data/soak/health.log (and $BK_RUN_LOG when set) so neither can grow
#     into the hundreds of MB — a huge log is a token hazard, not just disk
#   * run scripts/soak-health.sh --quiet
#   * append a timestamped result line to data/soak/health.log
#   * on ANOMALY: write data/soak/HEALTH_ALERT (small, human-readable); on
#     recovery: remove it. So an agent/you can check one file cheaply:
#         test -f data/soak/HEALTH_ALERT && cat data/soak/HEALTH_ALERT
#
# Env: BK_RUN_LOG (core log to scan/bound; unset = not configured),
#      BK_LOG_MAX_BYTES (per-log bound, default 20 MiB).
#
# Stop:  pkill -f soak-health-loop

set -uo pipefail
# BK_REPO_ROOT exists for the same reason as in soak-health.sh and
# stack-watchdog.sh: the supported way to run this under launchd is a COPY on the
# internal disk pointing back here (macOS TCC denies a launchd-spawned process
# access to the external volume this repo lives on).
ROOT="${BK_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT" || exit 2

INTERVAL=1800
while [ $# -gt 0 ]; do
  case "$1" in
    --interval-sec) INTERVAL="${2:-1800}"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[ "$INTERVAL" -ge 30 ] 2>/dev/null || INTERVAL=30

# Same seam the health check reads, so a fixture (or an operator with a
# non-default deployment) can move the whole trio together.
SOAK_DIR="${BK_SOAK_DIR:-data/soak}"
LOG="$SOAK_DIR/health.log"
ALERT="$SOAK_DIR/HEALTH_ALERT"
mkdir -p "$SOAK_DIR"

# Bound a log so it can never grow into the hundreds of MB (a huge log is a token
# hazard: any full read re-sent on every later turn). KI-30: this used to shell
# out to `scripts/rotate-run-log.sh`, which the Node-layer purge had deleted, so
# every tick appended a "No such file or directory" line into the very health log
# used to detect anomalies. The behaviour is inlined, and now also covers this
# loop's own log, which is the file that actually grows.
#
# Safe on a live file: `>>` opens with O_APPEND, so truncating in place keeps the
# same inode and the next write lands at offset 0 — no sparse hole, no lost fd.
# If gzip fails the log is left untouched rather than destroyed.
LOG_MAX_BYTES="${BK_LOG_MAX_BYTES:-20971520}"
bound_log() {
  local log=$1 max=${2:-$LOG_MAX_BYTES} size
  [ -f "$log" ] || return 0
  size=$(python3 -c 'import os,sys;print(os.path.getsize(sys.argv[1]))' "$log" 2>/dev/null) || return 0
  [ "$size" -ge "$max" ] 2>/dev/null || return 0
  if gzip -c "$log" > "$log.1.gz" 2>/dev/null; then
    : > "$log"
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] bounded $log ($((size / 1024 / 1024))MiB -> $log.1.gz)"
  else
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] WARN: gzip of $log failed; left untouched" >&2
  fi
}

running=1
trap 'running=0' TERM INT
echo "soak-health-loop: interval=${INTERVAL}s pid=$$ root=$ROOT"

while [ "$running" -eq 1 ]; do
  # The deployment's core log, when one is configured. Unset means it is not
  # ours to bound (BK_RUN_LOG is a deployment choice — see soak-health.sh).
  [ -n "${BK_RUN_LOG:-}" ] && bound_log "$BK_RUN_LOG"
  bound_log "$LOG"

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
