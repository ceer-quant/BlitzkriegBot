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
# Markers that survive the Node-layer removal: `package.json` was deleted with
# it, so guarding on that (as this line once did) refuses to run in its own tree.
[ -f Cargo.toml ] && [ -d .git ] || { echo "ANOMALY: not in BlitzkriegBot root ($ROOT)"; exit 2; }

problems=()
note() { [ "$QUIET" -eq 1 ] || echo "$@"; }

# ── 1. processes ────────────────────────────────────────────────────────────
core_n=$(pgrep -f "target/release/blitzkrieg-core" 2>/dev/null | wc -l | tr -d ' ')
soak_n=$(pgrep -f "soak-monitor" 2>/dev/null | wc -l | tr -d ' ')

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
health=$(curl -s -m 5 http://127.0.0.1:51888/health 2>/dev/null)
case "$health" in
  *'"status":"healthy"'*|*'"status": "healthy"'*) : ;;
  *) problems+=("health not healthy (${health:0:60})") ;;
esac

# ── 3. trade ledger (bounded: never cat the whole file) ─────────────────────
TRADES=data/trades/trades.jsonl
if [ -f "$TRADES" ]; then
  trades_n=$(wc -l < "$TRADES" | tr -d ' ')
  # A zero-hold force_exit has TWO distinct causes, and they need different fixes:
  #   (a) the old "秒平" bug — the position was force-exited the instant it opened
  #       even though the round had plenty of time left;
  #   (b) a timing-EXEMPT strategy (dog_strategy declares `gate_exemptions =
  #       ["timing"]`, E2-b/D-16) entering inside the force-exit window, where the
  #       exit policy fires on the very next tick by design. The entry is legal but
  #       the round has too little life left to ever reach its target.
  # Reporting (b) as (a) sends the reader chasing the wrong bug, so classify by the
  # round clock: timeLeft <= force_exit_sec means the entry itself was late.
  zero_hold=$(python3 - "$TRADES" <<'PY' 2>/dev/null || echo "0 0 0"
import json,sys
ROUND,FORCE=900,120
late=sudden=0
for l in open(sys.argv[1]):
    l=l.strip()
    if not l: continue
    try: r=json.loads(l)
    except: continue
    if r.get("holdTimeSec")!=0 or r.get("exitReason")!="force_exit": continue
    et=r.get("entryTime")
    if not et: continue
    tl=ROUND-(et/1000.0-(int(et/1000.0)//ROUND)*ROUND)
    if tl<=FORCE: late+=1
    else: sudden+=1
print(late+sudden, late, sudden)
PY
)
  zero_n=${zero_hold%% *}
  rest=${zero_hold#* }
  late_n=${rest%% *}
  sudden_n=${rest##* }
  if [ "${sudden_n:-0}" -gt 0 ]; then
    problems+=("holdTimeSec=0 & force_exit = $sudden_n with time left (seconds-flatten bug)")
  fi
  if [ "${late_n:-0}" -gt 0 ]; then
    problems+=("$late_n entry(ies) opened inside the force-exit window (timing-exempt strategy; legal but the round was already over)")
  fi
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

# ── 5. market-data archive freshness (P-1.3) ────────────────────────────────
# Capture is on by default and only useful if it is actually recording: a stopped
# archive (cap/disk) or a stale one means a future "why did this trade lose?" is
# unanswerable. Cheap check: how long since the newest segment was written.
ARCH_DIR=data/archive
arch_note="off"
if [ -d "$ARCH_DIR" ]; then
  newest=$(ls -t "$ARCH_DIR"/*.jsonl 2>/dev/null | head -1)
  if [ -z "$newest" ]; then
    arch_note="no-segments"
    problems+=("market-data archive dir exists but holds no segments")
  else
    now_s=$(date +%s)
    mt_s=$(stat -f%m "$newest" 2>/dev/null || stat -c%Y "$newest" 2>/dev/null || echo "$now_s")
    age=$((now_s - mt_s))
    arch_note="ok(${age}s)"
    # The core flushes the BufWriter once a second while events arrive, and a live
    # round trades continuously, so 5 minutes of silence means capture is down.
    if [ "$age" -gt 300 ]; then
      arch_note="stale(${age}s)"
      problems+=("market-data archive stale (${age}s since last write: $newest)")
    fi
  fi
  tail -c 200000 run.log 2>/dev/null | grep -q "event archive stopped recording" \
    && problems+=("market-data archive stopped recording (see run.log)")
fi

# ── 6. report ───────────────────────────────────────────────────────────────
round_disp=${round_sec:-?}
arch_disp=${arch_note:-?}
if [ ${#problems[@]} -eq 0 ]; then
  echo "OK  core=${core_n} soak=${soak_n} round=${round_disp} trades=${trades_n} archive=${arch_disp} health=healthy"
  exit 0
fi
echo "ANOMALY  core=${core_n} soak=${soak_n} round=${round_disp} trades=${trades_n} archive=${arch_disp}"
for p in "${problems[@]}"; do echo "  - $p"; done
exit 1
