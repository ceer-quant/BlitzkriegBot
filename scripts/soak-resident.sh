#!/usr/bin/env bash
# soak-resident.sh — start/stop/status for the DRY-RUN soak monitor pair (D-30).
#
# D-30 asked for two things the individual scripts did not provide:
#   1. the sampler runs RESIDENTLY (a 12h window made "is sampling happening?"
#      unanswerable across a long deployment), and
#   2. there is an EXPLICIT STOP SWITCH.
# This wrapper is that stop switch, and it is the single place that knows how the
# pair is meant to be run together.
#
# Why not launchd: this repo lives on an EXTERNAL volume (/Volumes/Hard Disk), and
# macOS TCC refuses a launchd-spawned process both read and exec access to it.
# Measured, not assumed — a launchd agent running `cat Cargo.toml` there reported
# `read-volume: DENIED`, and executing a script on the volume failed with
# `Operation not permitted` (exit 126). So the pair is started detached from the
# operator's own session (`nohup`), which does have access. The consequence is
# honest and worth stating: this survives until logout/reboot, NOT across a
# reboot. Boot persistence would need the user to grant Full Disk Access to a
# binary, which is a security-posture change — recorded as a decision, not
# taken unilaterally. See DECISIONS_PENDING.md (D-30/D-33).
#
# Usage:
#   scripts/soak-resident.sh start [--interval-sec N] [--sample-sec N] [--run-log PATH]
#   scripts/soak-resident.sh stop
#   scripts/soak-resident.sh status
#
# Env: BK_SOAK_DIR (default data/soak), BK_RUN_LOG (core log for the health scan),
#      BK_LOG_MAX_BYTES (per-log bound, default 20 MiB).
#
# BK_RUN_LOG is read from the environment or from $BK_SOAK_DIR/resident.env (one
# `KEY=value` per line), so the deployment-specific core log path is configured in
# one file instead of being hardcoded here: the core's stdout is /dev/null and its
# stderr is inherited, so only whoever launched it knows where the log landed —
# that is genuinely per-deployment. On this machine it is
# /private/tmp/blitzkrieg-panel.log (found via `lsof -p <core> -a -d 2`), and
# without it the crash scan reports `log=off` / `log_not_scanned`: an honest "not
# configured", never a silent zero (KI-30).
#
# `start` is idempotent-by-refusal: if a component is already running it says so
# and starts nothing, rather than quietly stacking a second sampler on the same
# socket (two samplers would double every figure in soak.jsonl).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

SOAK_DIR="${BK_SOAK_DIR:-data/soak}"
mkdir -p "$SOAK_DIR"
PID_LOOP="$SOAK_DIR/resident-loop.pid"
PID_SAMPLE="$SOAK_DIR/resident-sampler.pid"
OUT_LOOP="$SOAK_DIR/resident-loop.out"
OUT_SAMPLE="$SOAK_DIR/resident-sampler.out"
ENV_FILE="$SOAK_DIR/resident.env"

# Deployment config, sourced before any default so an explicit environment value
# still wins (the file is the fallback, not the authority — a one-off
# `BK_RUN_LOG=… soak-resident.sh start` must not be silently overridden).
if [ -f "$ENV_FILE" ]; then
  RUN_LOG_CFG=$(sed -n 's/^BK_RUN_LOG=//p' "$ENV_FILE" | head -1)
else
  RUN_LOG_CFG=""
fi
export BK_RUN_LOG="${BK_RUN_LOG:-$RUN_LOG_CFG}"

HEALTH_INTERVAL=1800     # how often soak-health.sh runs
SAMPLE_INTERVAL=600      # how often the sampler records a sample

# Is a pidfile's process alive AND actually ours? A stale pidfile is common (a
# kill -9, a reboot), and `kill -0` on a recycled pid would report a stranger as
# running. So match the command string too.
alive() {
  local pf=$1 pat=$2 pid st
  [ -f "$pf" ] || return 1
  pid=$(cat "$pf" 2>/dev/null) || return 1
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  # `kill -0` also succeeds for a ZOMBIE: the process table entry survives until
  # the parent collects it, so a process that has ALREADY exited reads as running
  # for the few milliseconds before it is reaped. That window is why `status`
  # could report an exited loop as RUNNING and made the resident gate
  # non-deterministic — the same run passed and then failed on an identical tree
  # (issue #212). A zombie is not running; state `Z` is not "ours, still alive".
  st=$(ps -o state= -p "$pid" 2>/dev/null | tr -d ' ')
  case "$st" in Z*) return 1 ;; esac
  ps -o command= -p "$pid" 2>/dev/null | grep -q "$pat" || return 1
  return 0
}

# WHY a pidfile was rejected, for the log. `alive` collapses three different
# situations into one false — no pidfile, a dead pid, and a pid the OS handed to
# an unrelated process — and `start` used to replace all three in silence. The
# third is the one that costs an afternoon: refusing to adopt a recycled pid is
# correct behaviour, but with nothing printed it reads as "start is broken",
# which is what turned a gate's false red into a hunt through an unrelated PR
# (issue #212). Empty output means there is nothing to explain.
stale_reason() {
  local pf=$1 pat=$2 pid
  [ -f "$pf" ] || return 0
  pid=$(cat "$pf" 2>/dev/null)
  [ -n "$pid" ] || { echo "pidfile $(basename "$pf") is empty"; return 0; }
  kill -0 "$pid" 2>/dev/null || { echo "stale pidfile: pid $pid is not running"; return 0; }
  echo "stale pidfile: pid $pid is not $pat (the pid was recycled by another process)"
}

sample_age_sec() {
  python3 - "$SOAK_DIR/soak.jsonl" <<'PY' 2>/dev/null || echo ""
import json, sys, time
last = None
try:
    for l in open(sys.argv[1], errors="replace"):
        l = l.strip()
        if not l:
            continue
        try:
            r = json.loads(l)
        except Exception:
            continue
        if isinstance(r.get("ts"), (int, float)):
            last = r["ts"]
except FileNotFoundError:
    pass
print(int(time.time() - last / 1000.0) if last else "")
PY
}

cmd=${1:-status}
shift 2>/dev/null || true

while [ $# -gt 0 ]; do
  case "$1" in
    --interval-sec) HEALTH_INTERVAL="${2:-$HEALTH_INTERVAL}"; shift 2 ;;
    --sample-sec)   SAMPLE_INTERVAL="${2:-$SAMPLE_INTERVAL}"; shift 2 ;;
    --run-log)      export BK_RUN_LOG="${2:-}"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

case "$cmd" in
  start)
    started=0
    if alive "$PID_LOOP" "soak-health-loop.sh"; then
      echo "already running: health loop pid $(cat "$PID_LOOP")"
    else
      reason=$(stale_reason "$PID_LOOP" "soak-health-loop.sh")
      if [ -n "$reason" ]; then echo "note: $reason — starting a fresh loop"; fi
      nohup ./scripts/soak-health-loop.sh --interval-sec "$HEALTH_INTERVAL" \
        >> "$OUT_LOOP" 2>&1 &
      echo $! > "$PID_LOOP"
      # `disown` so the loop is not taken down with this shell's job table.
      disown 2>/dev/null || true
      echo "started: health loop pid $(cat "$PID_LOOP") (every ${HEALTH_INTERVAL}s)"
      started=1
    fi
    if alive "$PID_SAMPLE" "soak-monitor.mjs"; then
      echo "already running: sampler pid $(cat "$PID_SAMPLE")"
    else
      reason=$(stale_reason "$PID_SAMPLE" "soak-monitor.mjs")
      if [ -n "$reason" ]; then echo "note: $reason — starting a fresh sampler"; fi
      nohup node ./scripts/soak-monitor.mjs --forever --interval-sec "$SAMPLE_INTERVAL" \
        </dev/null >> "$OUT_SAMPLE" 2>&1 &
      echo $! > "$PID_SAMPLE"
      disown 2>/dev/null || true
      echo "started: sampler pid $(cat "$PID_SAMPLE") (every ${SAMPLE_INTERVAL}s, --forever)"
      started=1
    fi
    [ "$started" -eq 1 ] && echo "note: these run until 'soak-resident.sh stop', logout, or reboot. See the TCC note in the header."
    ;;

  stop)
    for pair in "$PID_LOOP:health loop" "$PID_SAMPLE:sampler"; do
      pf=${pair%%:*}; label=${pair#*:}
      if [ -f "$pf" ]; then
        pid=$(cat "$pf" 2>/dev/null)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
          kill -TERM "$pid" 2>/dev/null
          # The loops sleep in 1s slices precisely so this is prompt; 10s is
          # generous, and SIGKILL after that would lose the closing log line.
          for _ in $(seq 1 20); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.5
          done
          if kill -0 "$pid" 2>/dev/null; then
            echo "WARN: $label pid $pid did not exit within 10s (left running; inspect it)"
          else
            echo "stopped: $label pid $pid"
          fi
        else
          echo "not running: $label (stale pidfile removed)"
        fi
        rm -f "$pf"
      else
        echo "not running: $label (no pidfile)"
      fi
    done
    ;;

  status)
    if alive "$PID_LOOP" "soak-health-loop.sh"; then
      echo "health loop: RUNNING pid $(cat "$PID_LOOP")"
    else
      echo "health loop: not running"
    fi
    if alive "$PID_SAMPLE" "soak-monitor.mjs"; then
      echo "sampler:     RUNNING pid $(cat "$PID_SAMPLE")"
    else
      echo "sampler:     not running"
    fi
    age=$(sample_age_sec)
    if [ -z "$age" ]; then
      echo "newest sample: none recorded"
    else
      echo "newest sample: ${age}s ago"
    fi
    if [ -f "$SOAK_DIR/HEALTH_ALERT" ]; then
      echo "alert:       PRESENT ($SOAK_DIR/HEALTH_ALERT)"
    else
      echo "alert:       none"
    fi
    ;;

  *)
    echo "usage: $0 {start|stop|status} [--interval-sec N] [--sample-sec N]" >&2
    exit 2
    ;;
esac
