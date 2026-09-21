#!/bin/sh
# backup-attempt.sh — the ONE attempt-record format for the backup schedulers.
#
# WHY THIS IS A SHARED LIBRARY AND NOT THREE INLINE COPIES
#   The 2026-09-21 failure of `com.blitzkrieg.databackup.light` was invisible
#   because nothing recorded that an attempt had happened: the launchd log held
#   one line, `launchctl list` said exit code 0, and no file anywhere said "the
#   daily backup has not succeeded once". Fixing that means every actor that can
#   start a backup leaves the same record — the launchd launcher, the resident
#   loop, and a hand run through `blitzkrieg backup`. Three copies of the record
#   format would drift, and a drifted record is read as "no attempt" by
#   `data-backup.sh --status`, i.e. back to silence. So the format lives here.
#
# WHERE RECORDS LIVE: $BK_BACKUP_STATUS_DIR, default $HOME/Library/Logs
#   Deliberately on the INTERNAL disk. This is the whole point: a scheduled job
#   that has been denied access to the external volume can still write here, so
#   "I ran and could not reach the repo (Operation not permitted)" survives even
#   though nothing on the repo could be touched. A record path inside the repo
#   would be unwritable in exactly the case it exists to report.
#
# Format (key=value, one per line — readable with `sed`, no parser dependency):
#   version=1
#   tier=light
#   source=launchd|loop|cli
#   at=2026-09-21T04:00:12Z        # human-readable UTC
#   attempt_epoch=1758427212       # authoritative for ordering and age
#   result=ok|fail
#   detail=<one line, newlines collapsed>
#   dest=<destination directory, when known>
#
#   suffix: _at / _epoch / _result / _detail hold the LAST SUCCESSFUL attempt.
#   They are carried forward from the previous record on failure, so "last
#   success 3 days ago, last attempt failed 2 minutes ago" is expressible in one
#   file — which is precisely the distinction that was missing.
#
# Usage:  . "$(dirname "$0")/lib/backup-attempt.sh"
#         bk_attempt_write <tier> <source> <result> <detail> [dest]
# Exit:   bk_attempt_write is best-effort and never fails its caller (a scheduler
#         must not be turned into a failure by its own bookkeeping), but it warns
#         on stderr when it cannot write, because a silent bookkeeping failure
#         would recreate the original defect one level up.

BK_ATTEMPT_STATUS_DIR_DEFAULT="${HOME:-/tmp}/Library/Logs"

bk_attempt_file() {
  printf '%s/%s.status\n' "${BK_BACKUP_STATUS_DIR:-$BK_ATTEMPT_STATUS_DIR_DEFAULT}" "$1"
}

# Collapse a multi-line reason into one line and bound it. A record read back by
# `sed -n 's/^detail=//p'` would otherwise lose everything after the first line,
# and an unbounded detail turns a status file into a log file.
bk_attempt_oneline() {
  printf '%s' "$1" | tr '\n\r\t' '   ' | sed 's/  */ /g; s/^ //; s/ $//' | cut -c1-300
}

# parse "k=v" lines from an existing record into BK_PREV_<K>; absent keys are
# left unset, and `set -u` callers read them through ${VAR:-}.
bk_attempt_load() {
  BK_PREV_SUCCESS_AT=""
  BK_PREV_SUCCESS_EPOCH=""
  _bkf="$(bk_attempt_file "$1")"
  if [ -f "$_bkf" ]; then
    BK_PREV_SUCCESS_AT="$(sed -n 's/^success_at=//p' "$_bkf" 2>/dev/null | head -1)"
    BK_PREV_SUCCESS_EPOCH="$(sed -n 's/^success_epoch=//p' "$_bkf" 2>/dev/null | head -1)"
  fi
}

bk_attempt_write() {
  _tier="$1"; _src="$2"; _result="$3"; _detail="$(bk_attempt_oneline "${4:-}")"; _dest="${5:-}"
  _dir="${BK_BACKUP_STATUS_DIR:-$BK_ATTEMPT_STATUS_DIR_DEFAULT}"
  _file="$(bk_attempt_file "$_tier")"
  _epoch="$(date +%s)"
  _at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"

  mkdir -p "$_dir" 2>/dev/null || {
    echo "warning: cannot create $_dir; the backup attempt record will not be written" >&2
    return 0
  }

  bk_attempt_load "$_tier"
  _s_at="$BK_PREV_SUCCESS_AT"
  _s_epoch="$BK_PREV_SUCCESS_EPOCH"
  if [ "$_result" = "ok" ]; then
    _s_at="$_at"; _s_epoch="$_epoch"
  fi

  # Atomic: a reader must never see a half-written record (it would read as
  # "no attempt" and drop back to silence). Same-directory mktemp + mv.
  _tmp="$(mktemp "$_dir/.bk-attempt.XXXXXX" 2>/dev/null)" || {
    echo "warning: cannot create a temp record in $_dir; attempt record not written" >&2
    return 0
  }
  {
    printf 'version=1\n'
    printf 'tier=%s\n'    "$_tier"
    printf 'source=%s\n'  "$_src"
    printf 'at=%s\n'      "$_at"
    printf 'attempt_epoch=%s\n' "$_epoch"
    printf 'result=%s\n'  "$_result"
    printf 'detail=%s\n'  "$_detail"
    printf 'dest=%s\n'    "$_dest"
    printf 'success_at=%s\n'    "$_s_at"
    printf 'success_epoch=%s\n' "$_s_epoch"
  } > "$_tmp" || {
    rm -f "$_tmp"
    echo "warning: cannot write attempt record for $_tier" >&2
    return 0
  }
  mv -f "$_tmp" "$_file" 2>/dev/null || {
    rm -f "$_tmp"
    echo "warning: cannot install attempt record at $_file" >&2
    return 0
  }
  return 0
}
