#!/usr/bin/env bash
# data-backup.sh — verified, external backup for `data/`.
#
# WHY THIS EXISTS
#   On 2026-09-17 a destructive-path bug in `scripts/package-release.mjs` emptied
#   the working tree, taking `data/archive` (617 MB / 4.94M events) with it.
#   Nothing could be restored: `data/` is gitignored, Time Machine was not
#   configured, and no external copy existed. See KNOWN_ISSUES KI-24 and
#   `bk-recovery-20260917/INCIDENT_REPORT.md`.
#
#   This script is that missing mechanism. Its safety properties are the point:
#     * It only ever READS the source tree. Every write lands under `--dest`.
#     * It never removes anything it did not itself create: pruning matches a
#       strict `blitzkrieg-data-<UTCSTAMP>` name, refuses symlinks, and refuses
#       to touch anything that is not a real directory directly under `--dest`.
#     * `--dest` is mandatory and validated: a backup inside the repository is
#       refused, because a copy that dies with the repository is not a backup.
#     * Every backup carries a SHA-256 manifest of what was CAPTURED, so "the
#       backup is good" is a checkable claim rather than a feeling.
#     * The manifest is computed FROM the archive, not from the tree. The
#       capture core appends to `data/archive/events.jsonl` continuously; a
#       manifest hashed from the live tree before tar would describe a version
#       of the file that never existed, and every verify of a live system would
#       fail on exactly the file that matters most (hit for real on the first
#       scheduled-era backup, 2026-09-20). Coverage — nothing in the tree was
#       missed — is asserted at creation time instead.
#
# Usage:
#   scripts/data-backup.sh --dest <dir> [--keep N] [--exclude-archive] [--dry-run]
#   scripts/data-backup.sh --verify <backup-dir>
#   scripts/data-backup.sh --status [--stale-hours N] [--full-stale-hours N] [--quiet]
#
#   --dest <dir>        Destination root. Required. Must be outside the repo.
#   --keep N            Keep the N newest backups in dest (default 7; 0 = keep all).
#   --exclude-archive   Skip data/archive (it is 95%+ of the bytes; use for a
#                       fast, frequent light backup).
#   --dry-run           Print exactly what would happen, write nothing.
#   --verify <dir>      Re-check an existing backup directory against its manifest.
#   --status            Report in ONE line how fresh each tier's newest backup is
#                       and what the last scheduled attempt did. Exit 0 = fresh,
#                       1 = a tier is stale / never produced / its last attempt
#                       failed, 2 = usage error. This is what makes a silent
#                       scheduler failure visible (issue #217): it judges the
#                       ARTIFACT and the ATTEMPT RECORD, so "the job never ran"
#                       and "the job ran and failed" are both loud, instead of
#                       being a `0` in `launchctl list`'s exit-code column.
#   --quiet             With --status: the one-line verdict only. That is the line
#                       scripts/soak-health.sh folds into its summary.
#   --data <dir>        Source tree override (default: <repo>/data). For tests.
#
# Exit: 0 = ok, 1 = failure, 2 = usage error or refusal.
#
# Env seams for --status (each has a real default; they exist so the gates can
# drive stale / never / failed / unreachable with fixtures):
#   BK_BACKUP_DIR              destination root (default /Volumes/Hard Disk/BlitzkriegBotBackup;
#                              must agree with data-backup-cli.sh's default)
#   BK_BACKUP_STATUS_DIR       per-tier attempt records, written by every actor
#                              through scripts/lib/backup-attempt.sh
#                              (default $HOME/Library/Logs — deliberately on the
#                              INTERNAL disk: a scheduler that has been denied
#                              access to the external volume can still record
#                              "I ran and could not reach the repo" there, which
#                              is the one signal TCC cannot silence)
#   BK_BACKUP_LOG_DIR          where the launchd plists write per-tier logs
#                              (default $HOME/Library/Logs)
#   BK_BACKUP_STALE_HOURS      light tolerance (default 26 = daily 04:00 + 2h slack)
#   BK_BACKUP_FULL_STALE_HOURS full tolerance (default 192 = weekly + 1 day slack)
#   BK_PYTHON                  interpreter used to read file mtimes (default python3).
#                              A seam so the gates can inject its absence: without it
#                              --status REFUSES (exit 2) rather than mis-report ages.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# The repo root is identified by markers that actually exist post-Node-removal.
# (`package.json` was deleted with the Node layer, so a check for it would
# refuse to run in the very tree this script lives in.)
#
# `.git` is a DIRECTORY in a normal clone but a FILE in a linked worktree
# (`gitdir: …`), so both are accepted. Guarding on `[ -d .git ]` alone made every
# worktree exit 2 with "not a BlitzkriegBot checkout" — and to a scheduler exit 2
# is indistinguishable from "ran fine", so an entire class of backups silently
# did not happen (issue #231). soak-health.sh already carried this fix; this is
# the same guard with the same reasoning.
[ -f "$ROOT/Cargo.toml" ] && { [ -d "$ROOT/.git" ] || [ -f "$ROOT/.git" ]; } \
  || { echo "refusing: not a BlitzkriegBot checkout ($ROOT)" >&2; exit 2; }

DATA="$ROOT/data"
DEST=""
KEEP=7
EXCLUDE_ARCHIVE=0
DRY_RUN=0
VERIFY_DIR=""
STATUS_MODE=0
STATUS_QUIET=0
# --status defaults. The destination root default must agree with
# data-backup-cli.sh / data-backup-install.sh; it is repeated rather than shared
# so that `--status` works with nothing but this file (a checker that needs the
# component it is checking is not a checker).
BACKUP_ROOT="${BK_BACKUP_DIR:-/Volumes/Hard Disk/BlitzkriegBotBackup}"
STATUS_DIR="${BK_BACKUP_STATUS_DIR:-$HOME/Library/Logs}"
SCHED_LOG_DIR="${BK_BACKUP_LOG_DIR:-$HOME/Library/Logs}"
STALE_LIGHT_H="${BK_BACKUP_STALE_HOURS:-26}"
STALE_FULL_H="${BK_BACKUP_FULL_STALE_HOURS:-192}"

# Usage text is this file's own header comment, omitted line 1 — one place to keep
# true, and it cannot drift from the flags in the loop below (a fixed `sed 2,68p`
# silently truncated the help the moment the header grew).
usage() { awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "$0"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --dest)            DEST="${2:-}"; shift 2 ;;
    --keep)            KEEP="${2:-}"; shift 2 ;;
    --exclude-archive) EXCLUDE_ARCHIVE=1; shift ;;
    --dry-run)         DRY_RUN=1; shift ;;
    --verify)          VERIFY_DIR="${2:-}"; shift 2 ;;
    --status)          STATUS_MODE=1; shift ;;
    --quiet)           STATUS_QUIET=1; shift ;;
    --stale-hours)     STALE_LIGHT_H="${2:-}"; shift 2 ;;
    --full-stale-hours) STALE_FULL_H="${2:-}"; shift 2 ;;
    --data)            DATA="${2:-}"; shift 2 ;;
    -h|--help)         usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# ── helpers ─────────────────────────────────────────────────────────────────
die()  { echo "refusing: $*" >&2; exit 2; }
fail() { echo "FAIL: $*" >&2; exit 1; }

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    return 1
  fi
}

sha256_file /dev/null >/dev/null 2>&1 || die "no shasum/sha256sum available"

# Byte size of one file, portable across BSD (macOS) and GNU (CI runner).
# `stat -f%z` is BSD; on GNU `-f` means "filesystem" and the form is rejected
# outright, so the two forms must be tried rather than assumed. `wc -c` is the
# portable way to sum a whole tree (see the dry-run below).
file_bytes() {
  stat -f%z "$1" 2>/dev/null || stat -c%s "$1" 2>/dev/null || echo 0
}

# Strict backup-directory name. Pruning matches ONLY this; anything else in the
# destination (a user's own folder, a different tool's output) is never a prune
# candidate, so this script cannot delete a stranger's data.
#
# The optional `-N` suffix disambiguates two runs inside the same UTC second
# (a double invocation, or a test). Without it a same-second re-run would be
# refused as a name collision — a spurious failure indistinguishable from a
# real one. The suffix still sorts after the bare name, so "newest last" holds.
BACKUP_NAME_RE='^blitzkrieg-data-[0-9]{8}T[0-9]{6}Z(-[0-9]+)?$'

# Absolute, symlink-resolved path for a file or directory that may not exist yet
# (resolve the parent, then re-append the leaf). On macOS `/var` is a symlink to
# `/private/var`, so comparing a resolved path against an unresolved one silently
# misses — which is exactly how a nesting guard can fail open.
abs_path() {
  local p="$1" parent leaf
  if [ -e "$p" ]; then
    (cd "$p" && pwd -P)
    return
  fi
  parent="$(dirname "$p")"
  leaf="$(basename "$p")"
  if [ -d "$parent" ]; then
    printf '%s/%s\n' "$(cd "$parent" && pwd -P)" "$leaf"
  else
    printf '%s\n' "$p"
  fi
}

# Refuse a destination that is not provably disjoint from the source tree.
# `--dest .` (or any path inside the repo) is the exact shape of the 2026-09-17
# incident, so it is rejected up front rather than merely "not recommended".
validate_dest() {
  [ -n "$DEST" ] || die "--dest is required (a backup needs somewhere to live)"
  [ -d "$DEST" ] || die "--dest is not an existing directory: $DEST"
  [ ! -L "$DEST" ] || die "--dest is a symlink; give a real directory so pruning cannot escape it"

  DEST_ABS="$(abs_path "$DEST")"
  DATA_ABS="$(abs_path "$DATA")"
  ROOT_ABS="$(abs_path "$ROOT")"

  [ "$DEST_ABS" != "$ROOT_ABS" ] || die "--dest must not be the repository root"
  case "$DEST_ABS/" in
    "$ROOT_ABS"/*) die "--dest is inside the repository ($DEST_ABS); a copy that dies with the repo is not a backup" ;;
  esac
  [ "$DEST_ABS" != "$DATA_ABS" ] || die "--dest must not be the data directory itself"
  case "$DEST_ABS/" in
    "$DATA_ABS"/*) die "--dest is inside the source tree ($DEST_ABS)" ;;
  esac

  # The new backup must not be able to contain the source (recursive copy).
  case "$DATA_ABS/" in
    "$DEST_ABS"/*) die "source tree is inside --dest ($DATA_ABS); refusing a recursive backup" ;;
  esac
}

# ── verify mode ─────────────────────────────────────────────────────────────
if [ -n "$VERIFY_DIR" ]; then
  [ -d "$VERIFY_DIR" ] || die "--verify target is not a directory: $VERIFY_DIR"
  MAN="$VERIFY_DIR/MANIFEST.sha256"
  ARC="$VERIFY_DIR/data.tar.gz"
  [ -f "$MAN" ] || fail "no MANIFEST.sha256 in $VERIFY_DIR"
  [ -f "$ARC" ] || fail "no data.tar.gz in $VERIFY_DIR"

  echo "== verify $VERIFY_DIR =="
  WANT_ARC_LINE="$(grep '^# archive-sha256 ' "$MAN" | awk '{print $3}')"
  if [ -n "$WANT_ARC_LINE" ]; then
    GOT="$(sha256_file "$ARC")" || fail "cannot hash archive"
    [ "$GOT" = "$WANT_ARC_LINE" ] || fail "archive hash mismatch (recorded $WANT_ARC_LINE, got $GOT)"
    echo "  ok   archive sha256 matches the manifest"
  else
    echo "  warn manifest records no archive hash (older format)"
  fi

  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  tar -xzf "$ARC" -C "$TMP" 2>/dev/null || fail "archive does not extract"
  echo "  ok   archive extracts"

  BAD=0
  # Manifest lines are "<sha256>  <relpath>" under a leading "data/" or the
  # basename of the source; compare against the extracted copy.
  while IFS= read -r line; do
    case "$line" in \#*|"") continue ;; esac
    want="${line%% *}"
    rel="${line#*  }"
    [ -n "$want" ] && [ -n "$rel" ] || continue
    got="$(sha256_file "$TMP/$rel" 2>/dev/null || echo MISSING)"
    if [ "$got" != "$want" ]; then
      echo "  !! $rel (expected ${want:0:12}…, got ${got:0:12}…)" >&2
      BAD=$((BAD + 1))
    fi
  done < "$MAN"

  if [ "$BAD" -gt 0 ]; then fail "$BAD file(s) do not match the manifest"; fi
  n="$(grep -c '^[0-9a-f]' "$MAN")"
  echo "  ok   all $n file(s) match the manifest"
  echo "RESULT: backup verifies."
  exit 0
fi

# ── status mode — the "is a backup actually happening?" verdict (issue #217) ─
# WHY THIS MODE EXISTS
#   The scheduled backup ran for the first time on 2026-09-21 04:00 and failed
#   with `Operation not permitted` (macOS TCC refuses a launchd-spawned process
#   access to this external volume — read AND exec, both measured). `launchctl
#   list` showed exit code 0, the log file was 94 bytes, and NOTHING else
#   changed: from every operational vantage point the schedule looked installed
#   and healthy while zero scheduled backups had ever been produced. The defect
#   is not the TCC refusal (that is a deployment choice, D-33); it is that the
#   refusal was INVISIBLE.
#
#   A freshness check that only looked at artifact age would NOT have caught it:
#   a hand-run backup makes the artifact fresh again while the SCHEDULE stays
#   broken, which is exactly the "KI-24 closed, daily backup live" false comfort
#   the issue is about. So two questions are judged separately:
#
#     1. ARTIFACT: when was a backup actually produced? Judged on the directory
#        on disk (strict `blitzkrieg-data-*` name). This is the safety property
#        and it cannot be faked by a scheduler that never ran.
#
#     2. SCHEDULER: what did the most recent AUTOMATIC attempt do? Judged on the
#        NEWEST of the two evidences a scheduler can leave behind:
#          * the per-tier launchd log (what the installed plist writes), and
#          * the attempt record every actor writes through
#            scripts/lib/backup-attempt.sh (launcher / resident loop / CLI).
#        Whichever is newer decides, so the check follows whichever path is
#        actually deployed and two mechanisms cannot contradict each other.
#
#   Deliberate non-alarms (KI-30: a check that can never clear is as useless as
#   one that can never fire):
#     * Only the LATEST attempt counts. An old failure superseded by a success
#       must not keep the verdict red.
#     * "No scheduler evidence at all" is NOT an alarm on its own — running the
#       backup from a resident loop or by hand is a legitimate deployment, and
#       pinning it red forever is the same defect as never firing. The artifact
#       clause is what covers "nothing is producing backups".
#     * A missing attempt record is never the verdict, only a diagnostic.
record_get() { sed -n "s/^$2=//p" "$1" 2>/dev/null | head -1; }

# Newest backup directory for a tier, by the strict name pattern only, so a
# stranger's directory in the same root can never be mistaken for a backup.
# Names are UTC timestamps, so a lexical max is a chronological max.
newest_backup_dir() {
  local d base best="" bestbase=""
  for d in "$1"/*; do
    [ -d "$d" ] || continue
    base="${d##*/}"
    [[ "$base" =~ $BACKUP_NAME_RE ]] || continue
    if [ -z "$bestbase" ] || [[ "$base" > "$bestbase" ]]; then best="$d"; bestbase="$base"; fi
  done
  printf '%s' "$best"
}

# mtime as epoch seconds. python3 rather than `stat`: `stat -f%m` is BSD-only and
# GNU's `-f` means "filesystem", so a two-form fallback is a portability landmine
# (this script also runs on Ubuntu CI).
#
# The interpreter is a seam (`BK_PYTHON`) and its ABSENCE is fatal in --status
# (checked there): the failure mode of "no python3" is not a crash, it is an
# unreadable mtime falling back to `date +%s` — i.e. a week-old backup reported as
# 0h old, which is precisely the silent false-green this mode exists to remove.
# Better to refuse to answer than to answer wrongly.
path_epoch() {
  "${BK_PYTHON:-python3}" -c 'import os,sys;print(int(os.path.getmtime(sys.argv[1])))' "$1" 2>/dev/null \
    || printf '%s' "$(date +%s)"
}

# Failure signature for a scheduler log line. Kept tight on purpose: a family of
# broad patterns (`error`, `cannot`) would turn a successful run's own prose into
# an alarm. These are the shapes a failed scheduled run actually leaves:
#   /bin/sh: /Volumes/.../data-backup-cli.sh: Operation not permitted   (TCC)
#   error: backup destination ... missing and cannot be created         (CLI)
#   FAIL: ...                                                           (data-backup.sh)
#   refusing: ...                                                       (data-backup.sh)
LOG_FAIL_RE='Operation not permitted|Permission denied|No such file or directory|not a BlitzkriegBot checkout|^[[:space:]]*(FAIL|refusing|error):'

# The SAME failures, reduced to the portable signature, for the one-line verdict.
# `.. Operation not permitted` truncated at an arbitrary 80 characters reads as
# "Operation " — the incident's own signature cut off exactly where it stops being
# searchable. The signature is what an operator greps for and what the runbook
# names, so it is extracted rather than truncated.
LOG_SIG_RE='Operation not permitted|Permission denied|No such file or directory|not a BlitzkriegBot checkout'

# Newest AUTOMATIC scheduler evidence for a tier: result ok|fail|none + when +
# what + why. Sets S_EPOCH / S_RESULT / S_DETAIL / S_SOURCE / S_AGE_H.
#
# The two automatic deployment paths each leave their own log, and both are
# considered:
#   launchd  → $SCHED_LOG_DIR/blitzkrieg-data-backup-<tier>.log   (what the plist writes)
#   loop     → $SCHED_LOG_DIR/blitzkrieg-data-backup-<tier>-loop.log
# plus the attempt records those paths write (source=launchd|loop).
#
# A MANUAL run's record (source=cli) is deliberately NOT scheduler evidence: a
# successful hand run must never mark the schedule healthy, which is exactly the
# false comfort issue #217 is about ("KI-24 closed, daily backup live" while the
# scheduler had never once succeeded).
#
# Newest evidence wins, so the verdict follows whichever path is actually
# deployed and a stale log from an uninstalled agent cannot pin the check red.
scheduler_evidence() {
  # `rec` is assigned on its own line: word expansion runs before `local` does, so
  # `local tier="$1" rec="…$tier…"` would expand `$tier` from the CALLER's scope.
  # It happened to work only because the only caller has a `tier` of its own.
  local tier="$1" log line ep src rec
  rec="$STATUS_DIR/$tier.status"

  S_EPOCH=""; S_RESULT="none"; S_DETAIL=""; S_SOURCE="none"; S_AGE_H=-1

  # Keep the newest candidate. A tie is won by the later consideration, so the
  # record (which carries a machine-readable reason) beats a log line at the same
  # second.
  _consider() {
    [ -n "$1" ] || return 0
    case "$1" in *[!0-9]*) return 0 ;; esac
    if [ -z "$S_EPOCH" ] || [ "$1" -ge "$S_EPOCH" ]; then
      S_EPOCH="$1"; S_RESULT="$2"; S_SOURCE="$3"; S_DETAIL="$4"
    fi
  }

  for log in "$SCHED_LOG_DIR/blitzkrieg-data-backup-$tier-loop.log" \
             "$SCHED_LOG_DIR/blitzkrieg-data-backup-$tier.log"; do
    [ -f "$log" ] || continue
    ep="$(path_epoch "$log")"
    line="$(grep -v '^[[:space:]]*$' "$log" 2>/dev/null | tail -1)"
    if printf '%s' "$line" | grep -qE "$LOG_FAIL_RE"; then
      _consider "$ep" fail "log:${log##*/}" "$line"
    else
      _consider "$ep" ok "log:${log##*/}" "$line"
    fi
  done

  # Records come last: only automatic sources qualify, and they are compared
  # against both logs above.
  if [ -f "$rec" ]; then
    src="$(record_get "$rec" source)"
    case "$src" in
      launchd|loop)
        _consider "$(record_get "$rec" attempt_epoch)" \
                  "$(record_get "$rec" result)" "record:$src" "$(record_get "$rec" detail)" ;;
    esac
  fi

  if [ -n "$S_EPOCH" ]; then
    S_AGE_H=$(( ($(date +%s) - S_EPOCH) / 3600 ))
  fi
}

status_problems=()
status_line=""

# One tier → "<tier>=<artifact>(<age>h) sched=<sched>" appended to status_line,
# with any problem appended to status_problems.
# `$1` tier, `$2` destination dir, `$3` tolerance h.
tier_verdict() {
  local tier="$1" dir="$2" tol_h="$3"
  local newest age_h now state="" sched="" log="$SCHED_LOG_DIR/blitzkrieg-data-backup-$tier.log"
  now="$(date +%s)"
  newest="$(newest_backup_dir "$dir")"

  if [ -n "$newest" ]; then
    age_h=$(( (now - $(path_epoch "$newest")) / 3600 ))
    if [ "$age_h" -le "$tol_h" ]; then
      state="ok"
    else
      state="STALE"
      status_problems+=("$tier backup is stale: newest is ${age_h}h old (> ${tol_h}h) — $newest")
    fi
  else
    # No artifact we can see. Distinguish the three causes, because they are three
    # different fixes: the volume is not there, the directory is there but empty,
    # or it is there and unreadable — the last one being the state that used to
    # look identical to a healthy one.
    if [ ! -e "$dir" ]; then
      state="ABSENT"
      status_problems+=("$tier backup root does not exist: $dir (backup volume not mounted, or access denied)")
    elif [ ! -d "$dir" ]; then
      state="NOTDIR"
      status_problems+=("$tier backup root is not a directory: $dir")
    elif ! ls "$dir" >/dev/null 2>&1; then
      state="DENIED"
      status_problems+=("$tier backup root is unreadable: $dir (TCC / permissions — a job in this context cannot see it)")
    else
      state="NONE"
      status_problems+=("$tier has never produced a backup in $dir")
    fi
    age_h=-1
  fi

  scheduler_evidence "$tier"
  case "$S_RESULT" in
    ok)   sched="ok(${S_AGE_H}h)" ;;
    fail)
      # The reason goes IN the token, not only in the verbose block: under
      # --quiet this one line is all scripts/soak-health.sh folds into its
      # summary, and an alarm that says "FAILED" without saying why costs the
      # reader another command at exactly the moment they are least inclined to
      # run one. Preference order: the matched failure signature (short, stable,
      # greppable), else the first 60 characters of the line.
      sig="$(printf '%s' "$S_DETAIL" | grep -oE "$LOG_SIG_RE" | head -1)"
      [ -n "$sig" ] || sig="$(printf '%s' "$S_DETAIL" | cut -c1-60)"
      sched="FAILED(${S_AGE_H}h:${sig})"
      status_problems+=("$tier most recent automatic attempt FAILED (${S_SOURCE}, ${S_AGE_H}h ago): ${S_DETAIL}") ;;
    *)    sched="no-evidence" ;;
  esac

  if [ "$STATUS_QUIET" -eq 0 ]; then
    printf '  %s: artifact=%s  sched=%s\n' "$tier" \
      "$([ "$age_h" -ge 0 ] && echo "$state(${age_h}h)" || echo "$state")" "$sched"
    printf '      newest      : %s\n' "${newest:-<none>}"
    printf '      scheduler   : %s%s\n' "$S_SOURCE" \
      "$([ -n "$S_EPOCH" ] && echo " at $(date -r "$S_EPOCH" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || echo "$S_EPOCH")" || echo "")"
    [ "$S_RESULT" = "none" ] || printf '      last detail : %s\n' "${S_DETAIL:0:200}"
    printf '      sandbox log : %s%s\n' "$log" "$([ -f "$log" ] && echo " ($(wc -c < "$log" | tr -d ' ') bytes)" || echo " (absent)")"
  fi

  if [ "$age_h" -ge 0 ]; then
    status_line="$status_line $tier=$state(${age_h}h)/sched=$sched"
  else
    status_line="$status_line $tier=$state/sched=$sched"
  fi
}

if [ "$STATUS_MODE" -eq 1 ]; then
  case "$STALE_LIGHT_H" in ''|*[!0-9]*) die "--stale-hours must be a non-negative integer" ;; esac
  case "$STALE_FULL_H"  in ''|*[!0-9]*) die "--full-stale-hours must be a non-negative integer" ;; esac
  # Ages come from file mtimes (see path_epoch). Without the interpreter the
  # fallback would silently report every artifact as 0h old — a stale-backup
  # check that answers "fresh" when it cannot read a clock is worse than no check,
  # so this refuses instead. (`command -v` also accepts an absolute BK_PYTHON.)
  command -v "${BK_PYTHON:-python3}" >/dev/null 2>&1 \
    || die "--status needs ${BK_PYTHON:-python3} to read file mtimes (BK_PYTHON overrides; without it a stale backup would be reported fresh)"

  tier_verdict light "$BACKUP_ROOT/light" "$STALE_LIGHT_H"
  tier_verdict full  "$BACKUP_ROOT/full"  "$STALE_FULL_H"

  echo "backup: ${status_line# }"
  if [ ${#status_problems[@]} -eq 0 ]; then
    exit 0
  fi
  # --quiet is the one-line contract the health check consumes: the verdict token
  # already carries the reason (see `sched=FAILED(...)` above), so the detail
  # lines are for a human running this by hand and are suppressed here.
  if [ "$STATUS_QUIET" -eq 0 ]; then
    for p in "${status_problems[@]}"; do echo "  - $p"; done
  fi
  exit 1
fi

[ -n "$DEST" ] || { usage; echo >&2; die "--dest is required"; }
case "$KEEP" in ''|*[!0-9]*) die "--keep must be a non-negative integer (got '$KEEP')" ;; esac

validate_dest

[ -d "$DATA" ] || fail "source tree not found: $DATA"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
TARGET="$DEST_ABS/blitzkrieg-data-$STAMP"

if [ "$DRY_RUN" -eq 1 ]; then
  echo "== data-backup :: DRY RUN (nothing will be written) =="
  echo "  source : $DATA_ABS"
  echo "  dest   : $DEST_ABS"
  echo "  target : $TARGET"
  echo "  keep   : $KEEP"
  [ "$EXCLUDE_ARCHIVE" -eq 1 ] && echo "  exclude: data/archive"
  echo "  files  : $(find "$DATA" -type f 2>/dev/null | wc -l | tr -d ' ')"
  # `wc -c` rather than `stat`: it means the same thing on both platforms, and a
  # failed `stat` here would silently print an empty total while the pipeline's
  # `awk` still exited 0, so the `||` fallback would never fire.
  echo "  bytes  : $(find "$DATA" -type f -exec wc -c {} \; 2>/dev/null | awk '{s+=$1} END {print s+0}')"
  echo "DRY RUN: no changes made."
  exit 0
fi

# ── never clobber: disambiguate within the same UTC second ──────────────────
# Two runs can land in the same second (a `--keep 1` check immediately followed
# by an `--exclude-archive` check, or a double invocation). The name is a
# timestamp, not a unique id, so a collision means "same second", not "the same
# backup". Refusing here would be a spurious failure indistinguishable from a
# real one — so walk to the first free `-N` suffix instead. Exhausting 1000
# candidates is not a collision, it is something else, and is still refused.
N=1
while [ -e "$TARGET-$(printf '%03d' "$N")" ]; do
  N=$((N + 1))
  [ "$N" -le 1000 ] || die "$TARGET and -001..-1000 all exist; refusing to guess a name"
done
[ ! -e "$TARGET" ] || TARGET="$TARGET-$(printf '%03d' "$N")"

SRC_PARENT="$(dirname "$DATA_ABS")"
SRC_BASE="$(basename "$DATA_ABS")"

TAR_ARGS=(-czf)
EXCLUDES=()
[ "$EXCLUDE_ARCHIVE" -eq 1 ] && EXCLUDES+=(--exclude="$SRC_BASE/archive")

echo "== data-backup :: $STAMP =="
echo "  source : $DATA_ABS"
echo "  dest   : $TARGET"

mkdir -p "$TARGET" || fail "cannot create $TARGET"

# ── archive first; the manifest then describes the ARCHIVE ──────────────────
# The capture core appends to `data/archive/events.jsonl` continuously. If the
# manifest were hashed from the live tree before tar (the original design), it
# would describe a version of that file that never existed, and every verify on
# a live system would fail on exactly the file that matters most. So: tar
# first, then hash what the archive actually holds. What could instead be LOST
# — a source file that never made it into the tar — is checked at creation
# time (coverage assertion below), while failing is still possible.
MAN="$TARGET/MANIFEST.sha256"
FILES_LIST="$(mktemp)"   # files present in the SOURCE tree, tar-relative
TAR_LIST="$(mktemp)"     # files actually held by the ARCHIVE, tar-relative
TMP="$(mktemp -d)"
trap 'rm -f "$FILES_LIST" "$TAR_LIST"; [ -n "$TMP" ] && rm -rf "$TMP"' EXIT

if [ "$EXCLUDE_ARCHIVE" -eq 1 ]; then
  find "$DATA_ABS" -path "$DATA_ABS/archive" -prune -o -type f -print
else
  find "$DATA_ABS" -type f -print
fi | LC_ALL=C sort | while IFS= read -r f; do
  [ -n "$f" ] || continue
  printf '%s/%s\n' "$SRC_BASE" "${f#"$DATA_ABS/"}"
done > "$FILES_LIST"

if ! tar "${TAR_ARGS[@]}" "$TARGET/data.tar.gz" "${EXCLUDES[@]+"${EXCLUDES[@]}"}" -C "$SRC_PARENT" "$SRC_BASE"; then
  fail "tar failed; a partial backup is left at $TARGET (do not treat it as valid)"
fi
ARC_SHA="$(sha256_file "$TARGET/data.tar.gz")" || fail "cannot hash archive"
ARC_BYTES="$(file_bytes "$TARGET/data.tar.gz")"

tar -xzf "$TARGET/data.tar.gz" -C "$TMP" \
  || fail "archive was written but does not extract; a partial backup is left at $TARGET (do not treat it as valid)"
# List the EXTRACTED tree: the manifest must describe exactly what a restorer
# will get, byte for byte, and this list is also what gets hashed below.
(cd "$TMP" && find . -type f -print) | LC_ALL=C sort | sed 's|^\./||' > "$TAR_LIST"

# ── coverage assertion (creation-time, the live-tree counterpart) ───────────
# A source file absent from the archive means the tree changed mid-capture in
# a way the backup did not capture. `data/archive/events*` is exempt: the live
# core appends to and rotates those segments while the tar runs, which is
# tolerated drift (the archive simply holds an earlier prefix of the stream).
# Anything else missing is a real gap — fail so the backup is never a
# self-consistent but incomplete artifact.
MISSING="$(comm -23 "$FILES_LIST" "$TAR_LIST")"
if [ -n "$MISSING" ]; then
  UNCOVERED="$(printf '%s\n' "$MISSING" | while IFS= read -r m; do
    case "$m" in
      "$SRC_BASE"/archive/events*)
        if [ "$EXCLUDE_ARCHIVE" -eq 1 ]; then
          printf '%s\n' "$m"
        else
          echo "  note  live-capture drift, tolerated: $m" >&2
        fi ;;
      *) printf '%s\n' "$m" ;;
    esac
  done)"
  if [ -n "$UNCOVERED" ]; then
    fail "source files missing from the archive (tree changed during capture; re-run the backup):
$UNCOVERED"
  fi
fi

# ── manifest (hashed from the extracted archive) ────────────────────────────
N_FILES=0
N_BYTES=0
while IFS= read -r rel; do
  [ -n "$rel" ] || continue
  h="$(sha256_file "$TMP/$rel")" || fail "cannot hash $rel"
  printf '%s  %s\n' "$h" "$rel" >> "$MAN"
  N_FILES=$((N_FILES + 1))
  sz="$(file_bytes "$TMP/$rel")"
  N_BYTES=$((N_BYTES + sz))
done < "$TAR_LIST"

printf '# archive-sha256 %s\n' "$ARC_SHA" >> "$MAN"
echo "  files  : $N_FILES ($N_BYTES bytes)"
echo "  archive: $ARC_BYTES bytes  sha256 ${ARC_SHA:0:16}…"

# A backup records what produced it, so a restore can name the revision.
HEAD_SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
cat > "$TARGET/BACKUP.json" <<JSON
{
  "createdAt": "$STAMP",
  "host": "$(hostname -s 2>/dev/null || echo unknown)",
  "repoRoot": "$ROOT",
  "repoHead": "$HEAD_SHA",
  "source": "$DATA_ABS",
  "excludedArchive": $([ "$EXCLUDE_ARCHIVE" -eq 1 ] && echo true || echo false),
  "fileCount": $N_FILES,
  "sourceBytes": $N_BYTES,
  "archiveBytes": $ARC_BYTES,
  "archiveSha256": "$ARC_SHA",
  "verify": "scripts/data-backup.sh --verify $TARGET"
}
JSON

# ── prune (only our own strictly-named directories, directly under dest) ────
if [ "$KEEP" -gt 0 ]; then
  # Collect strictly-named backup dirs directly under the destination, then sort
  # chronologically. Names are UTC timestamps, so a lexical sort is chronological.
  # `-print0` + read -d '' keeps paths with spaces intact (the repo itself lives
  # under a path containing a space).
  SORTED_FILE="$(mktemp)"
  find "$DEST_ABS" -maxdepth 1 -mindepth 1 -type d -name 'blitzkrieg-data-*' -print0 2>/dev/null \
    | tr '\0' '\n' | LC_ALL=C sort > "$SORTED_FILE"
  TOTAL="$(grep -c . "$SORTED_FILE" || true)"
  TOTAL="${TOTAL:-0}"

  if [ "$TOTAL" -gt "$KEEP" ]; then
    REMOVE=$((TOTAL - KEEP))
    # Read the victims from a file into a `while` in THIS shell, rather than
    # piping into it. A pipeline would put the loop in a subshell, where `fail`
    # (an `exit`) would only end the subshell: a failed prune would print a
    # message and then report success — a guard that cannot fail is not a guard.
    PRUNE_LIST="$(mktemp)"
    head -n "$REMOVE" "$SORTED_FILE" > "$PRUNE_LIST"
    while IFS= read -r d; do
      [ -n "$d" ] || continue
      base="$(basename "$d")"
      # Every guard must hold; a single failure skips (never deletes) the entry.
      if [ -L "$d" ]; then echo "  skip prune (symlink): $d" >&2; continue; fi
      if [ ! -d "$d" ]; then echo "  skip prune (not a dir): $d" >&2; continue; fi
      if [ "$(dirname "$d")" != "$DEST_ABS" ]; then echo "  skip prune (not at dest root): $d" >&2; continue; fi
      if ! [[ "$base" =~ $BACKUP_NAME_RE ]]; then echo "  skip prune (unexpected name): $d" >&2; continue; fi
      if [ "$d" = "$TARGET" ]; then echo "  skip prune (just created)" >&2; continue; fi
      echo "  prune  : $base"
      rm -rf -- "$d" || fail "could not prune $d"
    done < "$PRUNE_LIST"
    rm -f "$PRUNE_LIST"
  fi
  rm -f "$SORTED_FILE"
fi

echo
echo "BACKUP OK: $TARGET"
echo "  verify with: scripts/data-backup.sh --verify $TARGET"
echo "  restore with: tar -xzf $TARGET/data.tar.gz -C <parent-dir>"
exit 0
