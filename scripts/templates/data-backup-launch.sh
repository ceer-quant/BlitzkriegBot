#!/bin/sh
# data-backup-launch.sh — the internal-disk launcher for the KI-24 backup schedule.
#
# THIS FILE IS A TEMPLATE (issue #217, requirement 6). `scripts/data-backup-install.sh`
# renders it to ~/Library/Application Support/blitzkrieg/data-backup-launch.sh and the
# LaunchAgents call that copy. scripts/templates/README.md documents the manual route.
#
# WHY IT SITS ON THE INTERNAL DISK. macOS TCC denies a launchd-spawned process access
# to the volume this repository lives on, so
#   /bin/sh /Volumes/…/scripts/data-backup-cli.sh
# dies with "Operation not permitted" before one line of our own code runs — and that
# is what the daily agent did for months: a 94-byte log line, an exit code launchd
# reported as 0, and a system that looked backed up and was not. A launcher that both
# launchd and the human can read is what makes the refusal STATable.
#
# ⚠ MOVING THIS FILE IS NECESSARY BUT NOT SUFFICIENT. The launcher does not grant
# anything: it must still reach the repository to run a backup, and the guards below
# are real reads precisely because a permission bit is not the question TCC answers.
# Without Full Disk Access for /bin/sh (decision D-33) the first read fails, the run
# is refused, and NOTHING IS BACKED UP — what changes is that the refusal is loud, is
# written into the status file `blitzkrieg backup --status` reads, and exits non-zero.
# Granting that access is a user action; see scripts/templates/README.md (§ "Manual
# install" and § "What this route cannot do").
#
# Contract:
#   * repository readable      → exec the real CLI, which owns the attempt record
#   * repository NOT readable  → record a FAILED attempt (status file, internal disk),
#                                say why with a date and the fix, EXIT NON-ZERO
#
# Placeholders, substituted by scripts/data-backup-install.sh:
#   __REPO_ROOT__    the checkout being backed up
#   __BACKUP_DIR__   the backup destination root
# Each is overridable at runtime by the environment variable on the same line
# (BK_REPO_ROOT / BLITZKRIEG_BACKUP_DIR) — the plists set both, and that override is
# also what lets scripts/data-backup-check.mjs drive this template directly in a
# fixture without rendering it.
#
# Invoked by launchd as: /bin/sh <rendered copy> <light|full>
set -u

TIER="${1:-light}"
REPO="${BK_REPO_ROOT:-__REPO_ROOT__}"
BACKUP_ROOT="${BLITZKRIEG_BACKUP_DIR:-__BACKUP_DIR__}"
LOGFILE="${BK_BACKUP_LOG_DIR:-$HOME/Library/Logs}/blitzkrieg-data-backup-$TIER.log"
CLI="$REPO/scripts/data-backup-cli.sh"
STATUS_DIR="${BK_BACKUP_STATUS_DIR:-$HOME/Library/Logs}"
RECORD="$STATUS_DIR/$TIER.status"

# ── the attempt record, for the refusal path only ───────────────────────────────
# A launchd context cannot source scripts/lib/backup-attempt.sh: that file lives on
# the volume TCC denies — the very failure being recorded. So the refusal writes the
# same key=value format with a deliberately small second implementation, and the key
# sequence is kept honest by node scripts/data-backup-check.mjs (§12 compares it with
# the library's). The SUCCESS path records nothing here: it execs the CLI, which owns
# the real writer (and the artifact path/size only it knows).
record_fail() { # <detail>
  _detail="$(printf '%s' "$1" | tr '\n\r\t' '   ' | sed 's/  */ /g' | cut -c1-300)"
  _at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  # Carry the previous success forward even on a refusal: "last good copy 3 days ago,
  # refused 2 minutes ago" is the one fact this file exists to state, and a refusal
  # must not erase it.
  _s_at=""; _s_epoch=""; _s_artifact=""; _s_bytes=""
  if [ -f "$RECORD" ]; then
    _s_at="$(sed -n 's/^success_at=//p' "$RECORD" 2>/dev/null | head -1)"
    _s_epoch="$(sed -n 's/^success_epoch=//p' "$RECORD" 2>/dev/null | head -1)"
    _s_artifact="$(sed -n 's/^success_artifact=//p' "$RECORD" 2>/dev/null | head -1)"
    _s_bytes="$(sed -n 's/^success_bytes=//p' "$RECORD" 2>/dev/null | head -1)"
  fi
  mkdir -p "$STATUS_DIR" 2>/dev/null || return 0
  # Same-directory temp + mv: a reader (--status, the watchdog) must never find a
  # half-written record and read it as "no run has happened".
  {
    printf 'version=1\n'
    printf 'tier=%s\n' "$TIER"
    printf 'source=launchd\n'
    printf 'at=%s\n' "$_at"
    printf 'attempt_epoch=%s\n' "$(date +%s)"
    printf 'result=fail\n'
    printf 'detail=%s\n' "$_detail"
    printf 'dest=%s\n' "$BACKUP_ROOT/$TIER"
    printf 'artifact=\n'
    printf 'bytes=\n'
    printf 'success_at=%s\n' "$_s_at"
    printf 'success_epoch=%s\n' "$_s_epoch"
    printf 'success_artifact=%s\n' "$_s_artifact"
    printf 'success_bytes=%s\n' "$_s_bytes"
  } > "$RECORD.new" 2>/dev/null && mv -f "$RECORD.new" "$RECORD" 2>/dev/null
  return 0
}

# Emitted on both streams: launchd redirects StandardOutPath and StandardErrorPath at
# $LOGFILE, so these lines land in the scheduler log even if this process cannot write
# there itself. The LAST line is deliberately a bare `FAIL: …` — that is the shape
# data-backup.sh --status classifies as a failure, and it names the issue so the next
# person to read the log knows where the reasoning lives.
deny() {
  _reason="$1"
  # The record comes FIRST: if anything below is lost, the status file still says the
  # schedule is refusing to run.
  record_fail "launchd context cannot access the repository: $_reason"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] launchd context cannot access the repository: $_reason"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] macOS TCC denies a launchd-spawned process access to the volume this"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] repository lives on ($REPO). Grant Full Disk Access to /bin/sh to allow it:"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')]   System Settings > Privacy & Security > Full Disk Access > + > /bin/sh (D-33)"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] or run the no-FDA route: 'blitzkrieg backup' / scripts/data-backup-loop.sh start"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] refusal recorded in $RECORD — read it with: blitzkrieg backup --status"
  echo "FAIL: $TIER backup refused — $_reason. See https://github.com/ceer-quant/BlitzkriegBot/issues/217"
  exit 126
}

[ -n "$REPO" ] || deny "BK_REPO_ROOT is empty"
# Real reads, not `[ -r ]`: TCC enforces at open(2), and a permission bit is not the
# question being asked here. The reason text says which of the two worlds it is in —
# denied access is TCC (the phrase below is the one --status greps for and the one the
# runbook names), a missing directory is a moved checkout. A record that blamed TCC
# for a missing repo would send the reader after the wrong fix.
[ -d "$REPO" ] || deny "repository not found at $REPO"
head -c 1 "$CLI" >/dev/null 2>&1 || deny "cannot read $CLI (Operation not permitted)"
head -c 1 "$REPO/Cargo.toml" >/dev/null 2>&1 || deny "cannot read $REPO/Cargo.toml (Operation not permitted)"
if [ -e "$BACKUP_ROOT" ]; then
  ls "$BACKUP_ROOT" >/dev/null 2>&1 || deny "cannot read the backup root $BACKUP_ROOT (Operation not permitted)"
fi

# The repository is reachable: hand over to the real CLI, which records the attempt
# (with the artifact it produced and that artifact's size) and prunes. `--attempt-source
# launchd` is what makes the outcome visible to `blitzkrieg backup --status` as SCHEDULER
# evidence — a hand run must never count as proof that the schedule works.
if [ "$TIER" = "light" ]; then
  exec /bin/sh "$CLI" --light --attempt-source launchd
fi
exec /bin/sh "$CLI" --attempt-source launchd
