#!/usr/bin/env bash
# soak-health.sh — deterministic, ZERO-TOKEN health check for the dry-run soak.
#
# This replaces the LLM-driven 2-hourly health check for the *routine* case:
# every step is a plain shell command, so running it costs no API tokens. It
# prints one summary line and exits 0 (ok) or 1 (anomaly). Wire it to the OS
# scheduler (launchd/cron) so monitoring runs 24/7 for free; only escalate to an
# agent when it exits non-zero.
#
# The whole value of this script is that its exit code can be trusted, so every
# check below is written to be *able to fail*. KI-30: five of them could not —
# a probe of a route that never existed (permanently red), scans of a file that
# no longer exists (silently skipped, forever green), and a process check against
# a sampler with a bounded lifetime. An alarm that is always on and a check that
# can never fire are the same defect: no signal. The gate
# `scripts/soak-health-check.mjs` injects each failure instead of only walking
# the happy path.
#
# Usage:  scripts/soak-health.sh [--quiet]
# Exit:   0 = healthy, 1 = anomaly (details on stdout), 2 = not in repo root
#
# Read-only: it never restarts or kills anything, and never writes anything.
#
# Overridable seams. Every one exists so the gate can drive the failure branches
# with fixtures; each defaults to the real deployment.
#   BK_CORE_PGREP       pgrep pattern for the core process
#                       (default: target/release/blitzkrieg-core)
#   BK_SOCKET           core UDS path; derived from the running core's own argv
#                       when unset (the argv is authoritative — no second copy of
#                       the socket-naming contract lives here)
#   BK_PANEL_URL        panel base URL (default: http://127.0.0.1:51888)
#   BK_SOAK_DIR         soak sample directory (default: data/soak)
#   BK_SOAK_STALE_SEC   sampling staleness bound (default: 1800 = 3x the
#                       monitor's 600s default interval)
#   BK_ARCH_DIR         market-data archive dir (default: data/archive)
#   BK_TRADES           trade ledger (default: data/trades/trades.jsonl)
#   BK_TRADES_LOOKBACK_SEC
#                       only trade records newer than this many seconds are
#                       counted (default 86400 = 24h). The ledger is append-only
#                       and never rewritten, so counting it from the beginning of
#                       time makes every figure monotonically non-decreasing: a
#                       single old record pins HEALTH_ALERT on forever and the
#                       loop can never report recovery. Bounding by age is what
#                       makes the alarm able to CLEAR — while still lighting up
#                       again the moment a new occurrence lands. (KI-30 family:
#                       a guard that can only ever fire is as useless as one that
#                       can never fire.)
#                       Trade-off: an operator who wants a whole-soak view can
#                       raise this above the soak length, but must then accept the
#                       alarm staying lit for the rest of the soak. The default
#                       trades that away so that a healthy system reports healthy.
#   BK_RUN_LOG          log to scan for panics / archive-stop. UNSET means "not
#                       configured" and is reported as such: the path is a
#                       deployment choice, because the core's stdout is
#                       /dev/null and its stderr is inherited, so only the
#                       operator's redirect knows where the log is. Scanning a
#                       hardcoded path that nothing writes is a check that can
#                       never fire.

set -uo pipefail

QUIET=0
[ "${1:-}" = "--quiet" ] && QUIET=1

# Resolve repo root from this script's location (works from any cwd).
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2
# Markers that survive the Node-layer removal: `package.json` was deleted with
# it, so guarding on that (as this line once did) refuses to run in its own tree.
# The git marker is a DIRECTORY in a normal clone but a FILE in a linked worktree
# (`gitdir: …`), so both are accepted: guarding on `[ -d .git ]` alone made every
# worktree — every agent's build tree, every `git worktree add` — report "not in
# BlitzkriegBot root" and exit 2, which is why the gate that drives this script
# was 45 assertions of noise outside CI (issue #213).
[ -f Cargo.toml ] && { [ -d .git ] || [ -f .git ]; } || { echo "ANOMALY: not in BlitzkriegBot root ($ROOT)"; exit 2; }

CORE_PGREP=${BK_CORE_PGREP:-target/release/blitzkrieg-core}
PANEL_URL=${BK_PANEL_URL:-http://127.0.0.1:51888}
SOAK_DIR=${BK_SOAK_DIR:-data/soak}
SOAK_STALE_SEC=${BK_SOAK_STALE_SEC:-1800}
TRADES_LOOKBACK_SEC=${BK_TRADES_LOOKBACK_SEC:-86400}
RUN_LOG=${BK_RUN_LOG:-}

problems=()
note() { [ "$QUIET" -eq 1 ] || echo "$@"; }

# ── 1. processes ────────────────────────────────────────────────────────────
core_pids=$(pgrep -f "$CORE_PGREP" 2>/dev/null | tr '\n' ' ')
core_pids=${core_pids% }
if [ -z "$core_pids" ]; then
  core_n=0
elif [ "${core_pids#* }" = "$core_pids" ]; then
  core_n=1
else
  core_n=$(printf '%s\n' $core_pids | wc -l | tr -d ' ')
fi
core_pid=${core_pids%% *}

[ "$core_n" -ge 1 ] || problems+=("core not running (pgrep '$CORE_PGREP')")
[ "$core_n" -le 1 ] || problems+=("$core_n core instances (expected 1)")

# round-sec must stay 900 (300 = 5m regression = alarm)
core_sock=""
round_sec=""
if [ -n "$core_pid" ]; then
  core_argv=$(ps -o command= -p "$core_pid" 2>/dev/null)
  round_sec=$(printf '%s\n' "$core_argv" | tr ' ' '\n' | grep -A1 '^--round-sec$' | tail -1)
  [ "$round_sec" = "900" ] || problems+=("round-sec=${round_sec:-?} (expected 900)")
  core_sock=${BK_SOCKET:-$(printf '%s\n' "$core_argv" | tr ' ' '\n' | grep -A1 '^--socket$' | tail -1)}
fi

# ── 2. panel liveness ───────────────────────────────────────────────────────
# The panel's own unauthenticated probe. `/health` was never a route: an
# unmatched path falls through to the HTML panel with a **200**, so a substring
# probe against it could never succeed — which is why this check was
# permanently red from the day it was written. Requiring the panel's JSON shape
# also rejects that catch-all page explicitly, rather than trusting a substring
# that a 200-but-HTML body could accidentally carry.
panel_note="bad-body"
panel_code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$PANEL_URL/api/ping" 2>/dev/null || echo 000)
panel_body=$(curl -s -m 5 "$PANEL_URL/api/ping" 2>/dev/null)
case "$panel_body" in
  *'"service"'*'"blitzkrieg-panel"'*|*'"blitzkrieg-panel"'*'"service"'*)
    case "$panel_body" in
      *'"ok":true'*|*'"ok": true'*) panel_note="ok" ;;
      *) panel_note="not-ok"; problems+=("panel /api/ping reports not ok (${panel_body:0:80})") ;;
    esac ;;
  '<'*)
    panel_note="html-fallback"
    problems+=("panel served its HTML fallback for /api/ping (HTTP $panel_code): the panel JSON route is gone") ;;
  *)
    panel_note="bad-body"
    problems+=("panel /api/ping did not return the panel's JSON (HTTP $panel_code): ${panel_body:0:80}") ;;
esac

# ── 3. core liveness over UDS (unauthenticated) ─────────────────────────────
# `core.ping` answers whether or not the panel has auth enabled, and unlike the
# process check it also catches a core that is alive but wedged.
core_ping_note="skip"
if [ "$core_n" -ge 1 ]; then
  core_ping_note="skip(no-socket)"
fi
if [ -n "$core_sock" ]; then
  raw_ping=$(python3 - "$core_sock" <<'PY' 2>/dev/null || echo "fail"
import json, socket, sys
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(3)
    s.connect(sys.argv[1])
    s.sendall((json.dumps({"jsonrpc": "2.0", "id": 1, "method": "core.ping", "params": {}}) + "\n").encode())
    buf = b""
    while True:
        d = s.recv(65536)
        if not d:
            break
        buf += d
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            try:
                m = json.loads(line)
            except Exception:
                continue
            if m.get("id") == 1:
                print("ok" if (m.get("result") or {}).get("pong") else "not-ok")
                sys.exit(0)
    print("fail")
except Exception:
    print("fail")
PY
)
  if [ "$raw_ping" = "ok" ]; then
    core_ping_note="ok"
  else
    core_ping_note="fail"
    problems+=("core not answering core.ping over UDS (${core_sock}): $raw_ping")
  fi
fi

# ── 4. trade ledger (bounded: never cat the whole file) ─────────────────────
TRADES=${BK_TRADES:-data/trades/trades.jsonl}
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
  #
  # Both counts are bounded to the lookback window. The ledger is append-only, so
  # an unbounded count is monotonically non-decreasing: one record from days ago
  # keeps this anomaly (and therefore HEALTH_ALERT) lit forever, and the loop can
  # never report recovery no matter how healthy the system becomes. Age is the
  # right bound because the question each tick asks is "is anything wrong NOW",
  # not "was anything ever wrong".
  zero_hold=$(python3 - "$TRADES" "$TRADES_LOOKBACK_SEC" <<'PY' 2>/dev/null || echo "0 0 0"
import json,sys,time
ROUND,FORCE=900,120
cutoff=(time.time()-float(sys.argv[2]))*1000.0
late=sudden=0
for l in open(sys.argv[1]):
    l=l.strip()
    if not l: continue
    try: r=json.loads(l)
    except: continue
    if r.get("holdTimeSec")!=0 or r.get("exitReason")!="force_exit": continue
    et=r.get("entryTime")
    if not et: continue
    if et < cutoff: continue
    tl=ROUND-(et/1000.0-(int(et/1000.0)//ROUND)*ROUND)
    if tl<=FORCE: late+=1
    else: sudden+=1
print(late+sudden, late, sudden)
PY
)
  # Split "total late sudden" into fields. bash's suffix/prefix idioms are
  # asymmetric (`%% *` but `##* `) and a transposed space is not a syntax error:
  # it silently yields the whole string, `[ "$x" -gt 0 ]` then errors and takes
  # the false branch, and the anomaly it guards can never be reported. `read`
  # makes the split explicit and cannot be transposed.
  read -r zero_n late_n sudden_n <<<"$zero_hold"
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

# ── 5. soak sampling freshness ──────────────────────────────────────────────
# "Is the soak being sampled?" — not "is a process named soak-monitor alive".
# The sampler has a BOUNDED lifetime (`soak-monitor.mjs --hours 12`, then exit
# 0), so grepping for its name reports a designed, healthy end-of-run as an
# outage while missing the opposite failure (process alive, sampling wedged).
# The newest sample's own timestamp answers the real question, and can only be
# recent if sampling is actually happening.
SAMPLE="$SOAK_DIR/soak.jsonl"
soak_note="none"
if [ -f "$SAMPLE" ]; then
  sample_age=$(python3 - "$SAMPLE" <<'PY' 2>/dev/null || echo ""
import json, sys, time
last = None
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
print(int(time.time() - last / 1000.0) if last else "")
PY
)
  # Fall back to mtime when no record carries a usable ts, so a malformed sample
  # file is still judged on when it was last touched rather than passing as fresh.
  # python3 rather than `stat`: `stat -f%m` is BSD-only and GNU's `-f` means
  # "filesystem", so a two-form shell fallback is a portability landmine this
  # gate has to run on (Ubuntu CI). python3 is already a dependency above.
  if [ -z "$sample_age" ]; then
    mt_s=$(python3 -c 'import os,sys;print(int(os.path.getmtime(sys.argv[1])))' "$SAMPLE" 2>/dev/null || echo "$(date +%s)")
    sample_age=$(( $(date +%s) - mt_s ))
  fi
  soak_note="ok(${sample_age}s)"
  if [ "$sample_age" -gt "$SOAK_STALE_SEC" ]; then
    soak_note="stale(${sample_age}s)"
    problems+=("soak sampling stale (${sample_age}s > ${SOAK_STALE_SEC}s since last sample: $SAMPLE)")
  fi
else
  problems+=("no soak samples at $SAMPLE (nothing is sampling the soak)")
fi

# ── 6. crash / anomaly scan (BOUNDED; only when a log is configured) ────────
# The scan reads a *configured* path, never a hardcoded one. A hardcoded
# `run.log` was the Node layer's artefact: after the purge nothing wrote it, so
# this whole section silently vanished while still looking present. Unset is
# reported in the summary line (and as a note) instead of being skipped in
# silence, and "configured but missing" is an anomaly rather than a pass.
log_note="off"
if [ -n "$RUN_LOG" ]; then
  if [ -f "$RUN_LOG" ]; then
    log_note="ok"
    scan=$(tail -c 200000 "$RUN_LOG" 2>/dev/null | grep -icE "panic|crash|not connected|orphan|startup sweep cancelled" || true)
    [ "${scan:-0}" -gt 0 ] && problems+=("$RUN_LOG tail has $scan crash/orphan hits")
    tail -c 200000 "$RUN_LOG" 2>/dev/null | grep -q "event archive stopped recording" \
      && problems+=("market-data archive stopped recording (see $RUN_LOG)")
  else
    log_note="missing"
    problems+=("BK_RUN_LOG=$RUN_LOG does not exist (configured but not being written?)")
  fi
else
  note "note: BK_RUN_LOG unset — crash/archive-stop scan skipped (the log path is a deployment choice; see scripts/README.md)"
fi

# ── 7. market-data archive freshness (P-1.3) ────────────────────────────────
# Capture is on by default and only useful if it is actually recording: a stopped
# archive (cap/disk) or a stale one means a future "why did this trade lose?" is
# unanswerable. Cheap check: how long since the newest segment was written.
ARCH_DIR=${BK_ARCH_DIR:-data/archive}
arch_note="off"
if [ -d "$ARCH_DIR" ]; then
  newest=$(ls -t "$ARCH_DIR"/*.jsonl 2>/dev/null | head -1)
  if [ -z "$newest" ]; then
    arch_note="no-segments"
    problems+=("market-data archive dir exists but holds no segments")
  else
    now_s=$(date +%s)
    mt_s=$(python3 -c 'import os,sys;print(int(os.path.getmtime(sys.argv[1])))' "$newest" 2>/dev/null || echo "$now_s")
    age=$((now_s - mt_s))
    arch_note="ok(${age}s)"
    # The core flushes the BufWriter once a second while events arrive, and a live
    # round trades continuously, so 5 minutes of silence means capture is down.
    if [ "$age" -gt 300 ]; then
      arch_note="stale(${age}s)"
      problems+=("market-data archive stale (${age}s since last write: $newest)")
    fi
  fi
fi

# ── 8. report ───────────────────────────────────────────────────────────────
# Every sub-status goes in the summary line, because under --quiet (how the loop
# runs it) the summary and the problems are the ONLY things that reach
# data/soak/health.log. A sub-status that lives only in a `note` is invisible
# exactly where it is needed.
status="core=${core_n} round=${round_sec:-?} panel=${panel_note} core-ping=${core_ping_note} soak=${soak_note} trades=${trades_n} archive=${arch_note:-?} log=${log_note}"
if [ ${#problems[@]} -eq 0 ]; then
  echo "OK  $status"
  exit 0
fi
echo "ANOMALY  $status"
for p in "${problems[@]}"; do echo "  - $p"; done
exit 1
