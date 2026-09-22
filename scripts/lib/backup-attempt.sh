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
#   artifact=<the backup directory this attempt produced, on success>
#   bytes=<bytes this attempt captured (source + archive), on success>
#
#   suffix: _at / _epoch / _result / _detail hold the LAST SUCCESSFUL attempt.
#   They are carried forward from the previous record on failure, so "last
#   success 3 days ago, last attempt failed 2 minutes ago" is expressible in one
#   file — which is precisely the distinction that was missing.
#   success_artifact / success_bytes are carried forward the same way: on a
#   failure, "how big was the newest good copy and where is it" is the question
#   that gets asked next, and making the reader go find the directory again is
#   how a status file turns back into a scavenger hunt.
#
# WHY artifact/bytes AND NOT JUST dest (issue #217)
#   `dest` is the tier ROOT (`…/light`), which is the same string for every run
#   ever made there. A status file whose only "where" is a directory that never
#   changes cannot answer "what is the newest good copy, and is it plausibly
#   the right size?" — the two questions an operator asks when a scheduled job
#   has been quiet. Both new fields are reported BY the run itself and read
#   straight out of the produced BACKUP.json, so they cost no extra IO (a `du`
#   of an 8 GB full backup on every nightly run is real cost for a number the
#   backup already computed).
#
# Usage:  . "$(dirname "$0")/lib/backup-attempt.sh"
#         bk_attempt_write <tier> <source> <result> <detail> [dest] [artifact] [bytes]
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
  BK_PREV_SUCCESS_ARTIFACT=""
  BK_PREV_SUCCESS_BYTES=""
  _bkf="$(bk_attempt_file "$1")"
  if [ -f "$_bkf" ]; then
    BK_PREV_SUCCESS_AT="$(sed -n 's/^success_at=//p' "$_bkf" 2>/dev/null | head -1)"
    BK_PREV_SUCCESS_EPOCH="$(sed -n 's/^success_epoch=//p' "$_bkf" 2>/dev/null | head -1)"
    BK_PREV_SUCCESS_ARTIFACT="$(sed -n 's/^success_artifact=//p' "$_bkf" 2>/dev/null | head -1)"
    BK_PREV_SUCCESS_BYTES="$(sed -n 's/^success_bytes=//p' "$_bkf" 2>/dev/null | head -1)"
  fi
}

# Captured bytes for a produced backup directory, read from the BACKUP.json the
# backup wrote itself (sourceBytes + archiveBytes). Empty when it cannot be read:
# a size is a nice-to-have and must never turn into a wrong number. The two `sed`
# extractions are on the generator's own fixed shape (data-backup.sh writes it
# with printf, one field per line), not on arbitrary JSON — a general parser here
# would be a dependency this library deliberately does not have.
bk_attempt_bytes() {
  _b=""
  [ -n "${1:-}" ] && [ -f "$1/BACKUP.json" ] || { printf '%s' ""; return 0; }
  _src_b="$(sed -n 's/^[[:space:]]*"sourceBytes":[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$1/BACKUP.json" 2>/dev/null | head -1)"
  _arc_b="$(sed -n 's/^[[:space:]]*"archiveBytes":[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$1/BACKUP.json" 2>/dev/null | head -1)"
  case "${_src_b:-0}" in ''|*[!0-9]*) _src_b=0 ;; esac
  case "${_arc_b:-0}" in ''|*[!0-9]*) _arc_b=0 ;; esac
  _b=$((_src_b + _arc_b))
  [ "$_b" -gt 0 ] && printf '%s' "$_b"
  return 0
}

bk_attempt_write() {
  _tier="$1"; _src="$2"; _result="$3"; _detail="$(bk_attempt_oneline "${4:-}")"; _dest="${5:-}"
  _artifact="$(bk_attempt_oneline "${6:-}")"
  _bytes="${7:-}"
  case "$_bytes" in ''|*[!0-9]*) _bytes="" ;; esac
  # A failure did not produce anything, so the current-attempt fields must not
  # describe the previous run's artifact. (Defensive: callers already pass these
  # only on success, but a record that can lie about what it produced is worse
  # than a record that omits it.)
  [ "$_result" = "ok" ] || _artifact=""
  [ -z "$_artifact" ] && _bytes=""
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
  _s_artifact="$BK_PREV_SUCCESS_ARTIFACT"
  _s_bytes="$BK_PREV_SUCCESS_BYTES"
  if [ "$_result" = "ok" ]; then
    _s_at="$_at"; _s_epoch="$_epoch"
    # Only overwrite the remembered artifact when this attempt actually names
    # one: a success from a runner that does not report its output must not erase
    # the path the previous record already knows. Written as an `if` (not `[ … ] &&`)
    # so the branch cannot end on a non-zero status — this library is sourced by
    # callers running `set -e`.
    if [ -n "$_artifact" ]; then
      _s_artifact="$_artifact"; _s_bytes="$_bytes"
    fi
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
    printf 'artifact=%s\n' "$_artifact"
    printf 'bytes=%s\n'   "$_bytes"
    printf 'success_at=%s\n'    "$_s_at"
    printf 'success_epoch=%s\n' "$_s_epoch"
    printf 'success_artifact=%s\n' "$_s_artifact"
    printf 'success_bytes=%s\n'    "$_s_bytes"
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
