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
#
# The schedule itself lives in the LaunchAgents registered by
# scripts/data-backup-install.sh: daily 04:00 light, Sunday 04:30 full.
#
# Destination can be overridden with $BLITZKRIEG_BACKUP_DIR; the default lives
# outside the repository on the same volume that already holds the incident
# backups, so a workspace-level accident (the 2026-09-17 shape) cannot take it
# out with the repo.

set -eu

repo_root=$(cd "$(dirname "$0")/.." && pwd)
backup_script="$repo_root/scripts/data-backup.sh"
BACKUP_DIR="${BLITZKRIEG_BACKUP_DIR:-/Volumes/Hard Disk/BlitzkriegBotBackup}"

[ -f "$backup_script" ] || { echo "error: $backup_script missing" >&2; exit 1; }

mode="full"
case "${1:-}" in
  --light) mode="light" ;;
  --list)  mode="list" ;;
  --verify) mode="verify" ;;
  "") ;;
  -h|--help) echo "usage: blitzkrieg backup [--light | --list | --verify [dir]]" >&2; exit 0 ;;
  *) echo "error: unknown option: $1 (use --light, --list or --verify)" >&2; exit 2 ;;
esac

case "$mode" in
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
        exit 2
      }
    fi
    echo "==> blitzkrieg backup ($mode) → $dest"
    exec sh "$backup_script" --dest "$dest" --keep "$keep" $extra
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
