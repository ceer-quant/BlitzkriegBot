#!/usr/bin/env bash
# data-backup-loop.sh — run the daily/weekly backup from the operator's session.
#
# ── WHY THIS EXISTS (and what it is NOT) ────────────────────────────────────
# The LaunchAgent route is `data-backup-install.sh` + Full Disk Access, and it is
# still the only route that survives a reboot. But on 2026-09-21 the installed
# agent was measured failing with
#
#     /bin/sh: /Volumes/Hard Disk/BlitzkriegBot/scripts/data-backup-cli.sh: Operation not permitted
#
# — macOS TCC refuses a launchd-spawned process BOTH read and exec access to this
# external volume (reproduced with a probe agent: `ls` of the repository and
# `head` of a file inside it both return EPERM, so moving the launcher to the
# internal disk is necessary but NOT sufficient). Granting Full Disk Access is a
# security-posture change and is the user's call (D-33), so until that decision is
# made there must be a route that works with no posture change at all.
#
# This is that route, and it is the same one the repo already uses for the soak
# pair (`scripts/soak-resident.sh`): a process started detached from the
# operator's own session INHERITS that session's TCC access, so it can read the
# repository while launchd cannot. `nohup` + a pidfile, exactly like the soak
# loop that this repo has been running for weeks.
#
# Its honest limitation, stated up front because pretending otherwise is the bug
# this whole change is about: **it does not survive a reboot or a logout.** After
# a reboot, run `data-backup-loop.sh start` again (or switch to the LaunchAgent
# route). It is not a substitute for the reboot-persistent path — it is the
# working half while the reboot-persistent half waits on a user decision. The
# backup-freshness check (scripts/soak-health.sh, `blitzkrieg backup --status`)
# is what makes a forgotten restart loud instead of silent.
#
# ── SCHEDULE ────────────────────────────────────────────────────────────────
# Mirrors the D-27 plan the LaunchAgents implement, so switching between the two
# routes does not change what gets backed up:
#   light : daily, default 04:00 local  → <backup dir>/light  (keep 7, no archive)
#   full  : Sunday, default 04:30 local → <backup dir>/full   (keep 4)
# Each run goes through scripts/data-backup-cli.sh with `--attempt-source loop`,
# so its outcome lands in the same attempt record and appears in
# `blitzkrieg backup --status` as scheduler evidence.
#
# Usage:
#   scripts/data-backup-loop.sh start [--light-at HH:MM] [--full-at HH:MM]
#   scripts/data-backup-loop.sh start --once     # one light run now, then exit
#   scripts/data-backup-loop.sh stop
#   scripts/data-backup-loop.sh status
#
# Env: BK_BACKUP_LOG_DIR   where the pidfile, this loop's own output, and the
#                          per-tier run logs live (default $HOME/Library/Logs).
#                          The per-tier logs are the SAME files a LaunchAgent
#                          would write, so `--status` reads either route.
#      BK_BACKUP_DIR       destination root (default from data-backup-cli.sh).
#      BK_REPO_ROOT        repository root (default: derived from this script).
#
# Detect it:    ps -p "$(cat ~/Library/Logs/blitzkrieg-data-backup-loop.pid)"
# Stop it:      scripts/data-backup-loop.sh stop   (or pkill -f data-backup-loop)

set -uo pipefail

ROOT="${BK_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
LOG_DIR="${BK_BACKUP_LOG_DIR:-${HOME:-/tmp}/Library/Logs}"
CLI="$ROOT/scripts/data-backup-cli.sh"
PIDFILE="$LOG_DIR/blitzkrieg-data-backup-loop.pid"
SELF_OUT="$LOG_DIR/blitzkrieg-data-backup-loop.out"

LIGHT_AT="04:00"
FULL_AT="04:30"
ONCE=0

usage() { awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "$0"; }

cmd="${1:-status}"
shift 2>/dev/null || true
while [ $# -gt 0 ]; do
  case "$1" in
    --light-at) LIGHT_AT="${2:-}"; shift 2 ;;
    --full-at)  FULL_AT="${2:-}";  shift 2 ;;
    --once)     ONCE=1; shift ;;
    -h|--help)  usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# HH:MM with real ranges. The shape glob alone is not enough: it accepts `25:99`
# (each class matches its own digit), `seconds_until` then hands 25 to python,
# which raises, the `|| echo 86400` fallback swallows it, and the tier silently
# never runs — a typo in the schedule would disable backups instead of failing the
# start. Ranges are therefore checked, not just the shape.
valid_hhmm() {
  case "$1" in [0-9][0-9]:[0-9][0-9]) ;; *) return 1 ;; esac
  local hh="${1%%:*}" mm="${1##*:}"
  [ "$hh" -le 23 ] && [ "$mm" -le 59 ]
}

# pidfile + liveness, with the two refinements soak-resident.sh documents: a
# zombie is NOT running (`kill -0` succeeds for one), and a recycled pid belongs
# to a stranger, so the command line is matched too. Reusing those semantics
# rather than inventing a second notion of "alive" — two notions eventually give
# two answers to the same question.
alive() {
  local pf=$1 pat=$2 pid st
  [ -f "$pf" ] || return 1
  pid=$(cat "$pf" 2>/dev/null) || return 1
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  st=$(ps -o state= -p "$pid" 2>/dev/null | tr -d ' ')
  case "$st" in Z*) return 1 ;; esac
  ps -o command= -p "$pid" 2>/dev/null | grep -q "$pat" || return 1
  return 0
}

# Local-time seconds until the next occurrence of HH:MM, 0 when inside the
# 120-second grace window just after it. python3 rather than `date -j -f`/`date -d`:
# the two forms disagree across BSD and GNU, and this repo's gates run on both.
# weekday: "" for daily, "6" (Sunday in python's Mon=0 convention) for weekly.
seconds_until() {
  python3 -c '
import datetime, sys, time
hh, mm = (int(x) for x in sys.argv[1].split(":"))
wd = sys.argv[2]
now = datetime.datetime.now()
target = now.replace(hour=hh, minute=mm, second=0, microsecond=0)
if wd != "":
    target += datetime.timedelta(days=(int(wd) - now.weekday()) % 7)
delta = (target - now).total_seconds()
if 0 <= -delta <= 120:      # inside the grace window: due now
    print(0)
elif delta < 0:             # today already passed -> tomorrow / next weekday
    delta += 86400 if wd == "" else 604800
    print(int(delta))
else:
    print(int(delta))
' "$1" "$2" 2>/dev/null || echo 86400
}

run_tier() {
  # `log` is assigned on its own line, not in the same `local` statement: word
  # expansion happens before `local` runs, so `local tier="$1" log="…$tier…"`
  # expands `$tier` in the CALLER's scope — under `set -u` that is a hard "unbound
  # variable" crash on the very first scheduled run.
  local tier="$1" log rc=0 ts
  log="$LOG_DIR/blitzkrieg-data-backup-$tier-loop.log"
  ts="$(date '+%Y-%m-%d %H:%M:%S')"
  mkdir -p "$LOG_DIR" 2>/dev/null
  {
    echo "[$ts] loop: starting $tier backup (pid $$)"
    if [ "$tier" = "light" ]; then
      sh "$CLI" --light --attempt-source loop
    else
      sh "$CLI" --attempt-source loop
    fi
    rc=$?
  } >>"$log" 2>&1
  ts="$(date '+%Y-%m-%d %H:%M:%S')"
  # The LAST line of this file is what `data-backup.sh --status` classifies, and
  # the failure line must match its failure signature (`FAIL:`), so a run that
  # failed can never be read back as healthy just because the shell that wrapped
  # it exited 0. stdout marker first, then the run's own exit code.
  if [ "$rc" -eq 0 ]; then
    echo "[$ts] $tier backup ok rc=0" >>"$log"
  else
    echo "[$ts] FAIL: $tier backup rc=$rc (full output above; see $log)" >>"$log"
  fi
  return "$rc"
}

case "$cmd" in
  start)
    valid_hhmm "$LIGHT_AT" || { echo "invalid --light-at: $LIGHT_AT (want HH:MM)" >&2; exit 2; }
    valid_hhmm "$FULL_AT"  || { echo "invalid --full-at: $FULL_AT (want HH:MM)" >&2; exit 2; }
    [ -f "$CLI" ] || { echo "error: $CLI missing (BK_REPO_ROOT=$ROOT?)" >&2; exit 1; }
    mkdir -p "$LOG_DIR" || { echo "error: cannot create $LOG_DIR" >&2; exit 1; }

    if [ "$ONCE" -eq 1 ]; then
      # The detached loop writes only to its log (a daemon must not hold a
      # terminal), but `--once` is a FOREGROUND command: run it, then say what
      # happened and where — a silent exit 0/1 is how the original defect stayed
      # unseen for a day.
      run_tier light
      rc=$?
      once_log="$LOG_DIR/blitzkrieg-data-backup-light-loop.log"
      if [ "$rc" -eq 0 ]; then
        echo "ok: light backup finished (rc=0)"
      else
        echo "FAILED: light backup rc=$rc" >&2
      fi
      echo "  log: $once_log"
      tail -n "${BK_ONCE_TAIL:-12}" "$once_log" 2>/dev/null | sed 's/^/  | /'
      exit "$rc"
    fi

    if alive "$PIDFILE" "data-backup-loop"; then
      echo "already running: pid $(cat "$PIDFILE") (refusing to stack a second loop — two loops would run two backups)"
      exit 0
    fi

    # `nohup` + `&` from the operator's session is the whole point: this process
    # inherits the session's TCC access to the external volume, which launchd
    # does not have. See the header.
    nohup "$0" _run --light-at "$LIGHT_AT" --full-at "$FULL_AT" >>"$SELF_OUT" 2>&1 &
    pid=$!
    echo "$pid" > "$PIDFILE"
    sleep 1
    if alive "$PIDFILE" "data-backup-loop"; then
      echo "started: pid $pid (light $LIGHT_AT daily, full Sunday $FULL_AT)"
      echo "  run log : $LOG_DIR/blitzkrieg-data-backup-{light,full}-loop.log"
      echo "  this log: $SELF_OUT"
      echo "note: runs until 'scripts/data-backup-loop.sh stop', logout, or reboot —"
      echo "      it does NOT survive a reboot (TCC; see the header). After a reboot"
      echo "      run 'start' again, or install the LaunchAgent route + Full Disk Access."
    else
      echo "error: the loop did not stay up; see $SELF_OUT" >&2
      exit 1
    fi
    ;;

  _run)
    # The detached child. Not a public verb (hence the underscore) — it is what
    # `start` execs, and it is reachable directly only for debugging.
    echo "$(date '+%Y-%m-%d %H:%M:%S') loop: pid $$ light=$LIGHT_AT full=$FULL_AT"
    running=1
    trap 'running=0' TERM INT
    last_light=""; last_full=""
    while [ "$running" -eq 1 ]; do
      today="$(date '+%Y-%m-%d')"
      wl="$(seconds_until "$LIGHT_AT" "")"
      wf="$(seconds_until "$FULL_AT" "6")"
      if [ "$wl" -eq 0 ] && [ "$last_light" != "$today" ]; then
        last_light="$today"
        run_tier light || true    # the failure is recorded and logged; the loop must keep running
      elif [ "$wf" -eq 0 ] && [ "$last_full" != "$today" ]; then
        last_full="$today"
        run_tier full || true
      fi
      # Poll in <=30s steps and sleep in 1s slices, so a TERM is honoured promptly
      # while a daily wait does not spin.
      wait_s=$(( wl < wf ? wl : wf ))
      [ "$wait_s" -gt 0 ] || wait_s=30
      [ "$wait_s" -gt 30 ] && wait_s=30
      i=0
      while [ "$i" -lt "$wait_s" ] && [ "$running" -eq 1 ]; do sleep 1; i=$((i + 1)); done
    done
    echo "$(date '+%Y-%m-%d %H:%M:%S') loop: exiting (pid $$)"
    ;;

  stop)
    if [ ! -f "$PIDFILE" ]; then
      echo "not running (no pidfile at $PIDFILE)"
      exit 0
    fi
    pid=$(cat "$PIDFILE" 2>/dev/null)
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null
      for _ in $(seq 1 20); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
      done
      kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
      echo "stopped: pid $pid"
    else
      echo "not running (stale pidfile: ${pid:-empty})"
    fi
    rm -f "$PIDFILE"
    ;;

  status)
    if alive "$PIDFILE" "data-backup-loop"; then
      echo "RUNNING  pid $(cat "$PIDFILE")"
      echo "  light  $LIGHT_AT daily, full Sunday $FULL_AT"
      for tier in light full; do
        log="$LOG_DIR/blitzkrieg-data-backup-$tier-loop.log"
        if [ -f "$log" ]; then
          echo "  $tier last: $(grep -v '^[[:space:]]*$' "$log" | tail -1)"
        else
          echo "  $tier last: (no run recorded yet: $log)"
        fi
      done
      exit 0
    fi
    echo "NOT RUNNING (no live loop at $PIDFILE)"
    echo "  → automatic backups are NOT happening right now. Start with:"
    echo "      scripts/data-backup-loop.sh start"
    echo "  → and verify the real question, not just this process:"
    echo "      scripts/data-backup.sh --status"
    exit 1
    ;;

  -h|--help|help) usage ;;

  *) usage >&2; echo >&2; echo "unknown command: $cmd (want start|stop|status)" >&2; exit 2 ;;
esac
