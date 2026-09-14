#!/usr/bin/env bash
# soak-health.sh — deterministic, ZERO-TOKEN health check for the dry-run soak.
#
# This replaces the LLM-driven 2-hourly health check for the *routine* case:
# every step is a plain shell command, so running it costs no API tokens. It
# prints one summary line and exits 0 (ok) or 1 (anomaly). Wire it to the OS
# scheduler (launchd/cron) so monitoring runs 24/7 for free; only escalate to an
# agent when it exits non-zero.
#
# Usage:  scripts/soak-health.sh [--quiet]
# Exit:   0 = healthy, 1 = anomaly (details on stdout), 2 = not in repo root
#
# Read-only: it never restarts or kills anything.

set -uo pipefail

QUIET=0
[ "${1:-}" = "--quiet" ] && QUIET=1

# Resolve repo root from this script's location (works from any cwd).
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2
[ -f package.json ] || { echo "ANOMALY: not in BlitzkriegBot root ($ROOT)"; exit 2; }

problems=()
note() { [ "$QUIET" -eq 1 ] || echo "$@"; }

# ── 1. processes ────────────────────────────────────────────────────────────
core_n=$(pgrep -f "target/release/blitzkrieg-core" 2>/dev/null | wc -l | tr -d ' ')
node_n=$(pgrep -f "node dist/index.js" 2>/dev/null | wc -l | tr -d ' ')
soak_n=$(pgrep -f "soak-monitor" 2>/dev/null | wc -l | tr -d ' ')

[ "$node_n" -ge 1 ] || problems+=("node shell not running")
[ "$soak_n" -ge 1 ] || problems+=("soak-monitor not running")
if [ "$core_n" -eq 0 ]; then
  problems+=("blitzkrieg-core not running")
elif [ "$core_n" -gt 1 ]; then
  problems+=("$core_n blitzkrieg-core instances (expected 1)")
fi

# round-sec must stay 900 (300 = 5m regression = alarm)
core_pid=$(pgrep -f "target/release/blitzkrieg-core" 2>/dev/null | head -1)
if [ -n "$core_pid" ]; then
  round_sec=$(ps -o command= -p "$core_pid" 2>/dev/null | tr ' ' '\n' | grep -A1 '^--round-sec$' | tail -1)
  [ "$round_sec" = "900" ] || problems+=("round-sec=${round_sec:-?} (expected 900)")
fi

# ── 2. health endpoint ──────────────────────────────────────────────────────
health=$(curl -s -m 5 http://127.0.0.1:18789/health 2>/dev/null)
case "$health" in
  *'"status":"healthy"'*|*'"status": "healthy"'*) : ;;
  *) problems+=("health not healthy (${health:0:60})") ;;
esac

# ── 3. trade ledger (bounded: never cat the whole file) ─────────────────────
TRADES=data/trades/trades.jsonl
if [ -f "$TRADES" ]; then
  trades_n=$(wc -l < "$TRADES" | tr -d ' ')
  # holdTimeSec=0 paired with exitReason=force_exit == the old "秒平" bug.
  bad_pair=$(grep -c '"holdTimeSec":0' "$TRADES" 2>/dev/null || true)
  force0=$(python3 - "$TRADES" <<'PY' 2>/dev/null || echo 0
import json,sys
n=0
for l in open(sys.argv[1]):
    l=l.strip()
    if not l: continue
    try: r=json.loads(l)
    except: continue
    if r.get("holdTimeSec")==0 and r.get("exitReason")=="force_exit": n+=1
print(n)
PY
)
  [ "${force0:-0}" -gt 0 ] && problems+=("holdTimeSec=0 & force_exit = $force0 (seconds-flatten bug)")
else
  trades_n=0
  problems+=("no trade ledger at $TRADES")
fi

# ── 4. crash / anomaly scan in the log tail (BOUNDED) ───────────────────────
# Only the last ~200KB, so a 70MB+ run.log never enters a context window.
if [ -f run.log ]; then
  scan=$(tail -c 200000 run.log 2>/dev/null | grep -icE "panic|crash|not connected|orphan|startup sweep cancelled" || true)
  [ "${scan:-0}" -gt 0 ] && problems+=("run.log tail has $scan crash/orphan hits")
fi

# ── 5. report ───────────────────────────────────────────────────────────────
round_disp=${round_sec:-?}
if [ ${#problems[@]} -eq 0 ]; then
  echo "OK  core=${core_n} node=${node_n} soak=${soak_n} round=${round_disp} trades=${trades_n} health=healthy"
  exit 0
fi
echo "ANOMALY  core=${core_n} node=${node_n} soak=${soak_n} round=${round_disp} trades=${trades_n}"
for p in "${problems[@]}"; do echo "  - $p"; done
exit 1
