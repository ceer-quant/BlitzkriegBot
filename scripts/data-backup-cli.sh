#!/bin/sh
# data-backup-cli.sh — the 傻白甜 front door to `scripts/data-backup.sh` (KI-24).
#
# data-backup.sh keeps its tested, strict contract: `--dest` is mandatory and
# refused paths fail loudly. This wrapper exists so the operator never has to
# remember the contract — it supplies the project's standard destination and
# retention, and adds the two verbs the launcher's other commands already have
# in spirit: `list` and `verify`.
#
# Usage (all through the shim as `blitzkrieg backup …`):
#   blitzkrieg backup               full backup of data/ (archive included;
#                                   keep 4 — per D-27: one full per week)
#   blitzkrieg backup --light       light backup (data/archive skipped; the
#                                   daily tier per D-27 — trades/orders/
#                                   positions/soak/strategy-state are a few MB)
#   blitzkrieg backup --list        show the backups that exist, newest first
#   blitzkrieg backup --verify      verify the newest full backup (or --verify <dir>)
#   blitzkrieg backup --status      ONE line: how fresh each tier's newest backup
#                                   is, and what the last automatic attempt did.
#                                   Exit 1 when a tier is stale or its scheduler
#                                   is failing (issue #217). `--status --quiet`
#                                   is the machine-readable form.
#
# The schedule itself lives in the LaunchAgents registered by
# scripts/data-backup-install.sh: daily 04:00 light, Sunday 04:30 full.
#
# EVERY OUTCOME IS RECORDED (issue #217). This script is the front door, so it is
# where an attempt becomes observable: each run writes an attempt record through
# scripts/lib/backup-attempt.sh (on the INTERNAL disk, so it works even when the
# external volume is unreachable). Without it, a failed scheduled run left one
# 94-byte log line and `launchctl list` still reported exit code 0 — the failure
# was indistinguishable from "the job has not run yet", which is how a completely
# un-backed-up system came to look healthy on the ops board.
#
# Destination can be overridden with $BLITZKRIEG_BACKUP_DIR; the default lives
# outside the repository on the same volume that already holds the incident
# backups, so a workspace-level accident (the 2026-09-17 shape) cannot take it
# out with the repo.
#
# Env: BK_REPO_ROOT          checkout to back up (default: this script's repo)
#      BLITZKRIEG_BACKUP_DIR destination root (default: the standard external one)
#      BK_BACKUP_STATUS_DIR  attempt records (default $HOME/Library/Logs)

set -eu

# BK_REPO_ROOT overrides the checkout being backed up, and that seam is not
# academic (same convention as soak-health.sh / stack-watchdog.sh): the documented
# way to run under launchd from this external volume is a COPY of the scripts on
# the internal disk pointing back here — and a copied CLI that derived its root
# from its own location would cheerfully back up the wrong tree (or none).
repo_root="${BK_REPO_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
backup_script="$repo_root/scripts/data-backup.sh"
BACKUP_DIR="${BLITZKRIEG_BACKUP_DIR:-/Volumes/Hard Disk/BlitzkriegBotBackup}"

# Shared record format. Best-effort: a missing lib must not stop a backup, so the
# record is skipped (with a warning) rather than made fatal — losing bookkeeping
# is bad, losing the backup because of bookkeeping is worse.
BK_LIB="$repo_root/scripts/lib/backup-attempt.sh"
if [ -f "$BK_LIB" ]; then
  . "$BK_LIB"
else
  echo "warning: $BK_LIB missing; backup attempts will not be recorded" >&2
  bk_attempt_write() { :; }
fi

[ -f "$backup_script" ] || { echo "error: $backup_script missing" >&2; exit 1; }

# data-backup.sh is bash (`set -o pipefail`, `[[ =~ ]]`). This wrapper is POSIX sh
# and may well BE dash (Linux); `sh "$backup_script"` would abort inside it with
# "Illegal option -o pipefail" — a backup that fails because of its own
# interpreter, reported as a backup failure. Prefer bash; fall back to the
# script's own shebang (it is committed executable) if bash is absent.
if command -v bash >/dev/null 2>&1; then
  BK_INTERP="$(command -v bash)"
else
  BK_INTERP="$backup_script"
fi

# Recording source: the plists and the resident loop pass `--attempt-source` so a
# record can say who ran it. Defaults to "cli" (a hand run) — and that distinction
# is load-bearing: a successful MANUAL run must never mark the SCHEDULER healthy.
attempt_source="cli"
mode="full"
case "${1:-}" in
  --light)  mode="light" ;;
  --list)   mode="list" ;;
  --verify) mode="verify" ;;
  --status) mode="status" ;;
  "") ;;
  -h|--help) echo "usage: blitzkrieg backup [--light | --list | --verify [dir] | --status [--quiet]]" >&2; exit 0 ;;
  *) echo "error: unknown option: $1 (use --light, --list, --verify or --status)" >&2; exit 2 ;;
esac
# Internal (not documented in the shim): lets an automatic runner label itself.
if [ "${2:-}" = "--attempt-source" ]; then attempt_source="${3:-cli}"; shift 2 || true; fi

case "$mode" in
  status)
    # --status and everything after it pass straight through to the verdict
    # engine, which is where the age/record logic lives — one implementation, read
    # by both this verb and scripts/soak-health.sh, so the two cannot disagree.
    shift
    exec "$BK_INTERP" "$backup_script" --status "$@"
    ;;

  full | light)
    # Standard destination layout: light/ and full/ subroots so the two tiers
    # prune independently (a daily light run must never prune a weekly full,
    # and vice versa — same name pattern, different root).
    sub="full"
    keep=4
    extra=""
    if [ "$mode" = "light" ]; then
      sub="light"
      keep=7
      extra="--exclude-archive"
    fi
    dest="$BACKUP_DIR/$sub"
    if [ ! -d "$dest" ]; then
      mkdir -p "$dest" 2>/dev/null || {
        echo "error: backup destination $dest missing and cannot be created." >&2
        echo "       Is the volume mounted? Override with BLITZKRIEG_BACKUP_DIR." >&2
        # The volume being gone is a FAILED ATTEMPT, not a no-op: it is the
        # unmounted-external-disk case, and it must reach --status.
        bk_attempt_write "$mode" "$attempt_source" fail \
          "destination missing and cannot be created (volume not mounted?): $dest" "$dest"
        exit 2
      }
    fi
    echo "==> blitzkrieg backup ($mode) → $dest"

    # Run the strict script, streaming its output to the operator while keeping a
    # transcript so a failure can name its own reason. The exit code is carried out
    # of the pipeline through a file: POSIX `sh` has no PIPESTATUS, and `$?` after a
    # pipeline is tee's status — which is 0 for a failed backup, the exact
    # silent-success shape this whole change exists to remove.
    rc_file="$(mktemp "${TMPDIR:-/tmp}/bk-backup-rc.XXXXXX")"
    out_file="$(mktemp "${TMPDIR:-/tmp}/bk-backup-out.XXXXXX")"
    trap 'rm -f "$rc_file" "$out_file"' EXIT
    ( "$BK_INTERP" "$backup_script" --dest "$dest" --keep "$keep" $extra 2>&1; echo $? >"$rc_file" ) \
      | tee "$out_file"
    rc="$(cat "$rc_file" 2>/dev/null || echo 1)"
    case "$rc" in ''|*[!0-9]*) rc=1 ;; esac

    if [ "$rc" -eq 0 ]; then
      bk_attempt_write "$mode" "$attempt_source" ok "" "$dest"
    else
      detail="$(grep -v '^[[:space:]]*$' "$out_file" 2>/dev/null | tail -1)"
      [ -n "$detail" ] || detail="exit $rc with no output"
      bk_attempt_write "$mode" "$attempt_source" fail "exit $rc: $detail" "$dest"
    fi
    exit "$rc"
    ;;


  list)
    for sub in light full; do
      dir="$BACKUP_DIR/$sub"
      echo "== $dir =="
      if [ ! -d "$dir" ]; then echo "  (missing — no backups yet)"; continue; fi
      found=0
      for d in "$dir"/blitzkrieg-data-*; do
        [ -d "$d" ] || continue
        found=$((found + 1))
        sz=$(du -h "$d" | awk '{print $1}')
        echo "  $(basename "$d")  $sz"
      done
      [ "$found" -eq 0 ] && echo "  (empty)"
    done
    # The two pre-mechanism snapshots (data-reset + incident era) live at the root.
    if [ -d "$BACKUP_DIR" ]; then
      others=$(find "$BACKUP_DIR" -maxdepth 1 -mindepth 1 -type d \
        \( -name 'blitzkrieg-data-*' -o -name 'data-history-*' \) 2>/dev/null | wc -l | tr -d ' ')
      echo "(plus $others manual snapshot(s) directly under $BACKUP_DIR)"
    fi
    ;;

  verify)
    target="${2:-}"
    if [ -z "$target" ]; then
      # Default: the newest full backup.
      target=$(ls -d "$BACKUP_DIR/full"/blitzkrieg-data-* 2>/dev/null | LC_ALL=C sort | tail -1)
      [ -n "$target" ] || { echo "error: no full backup found in $BACKUP_DIR/full" >&2; exit 2; }
      echo "verifying newest: $target"
    fi
    exec sh "$backup_script" --verify "$target"
    ;;
esac
