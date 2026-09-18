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
#     * Every backup carries a SHA-256 manifest of the source, so "the backup is
#       good" is a checkable claim rather than a feeling.
#
# Usage:
#   scripts/data-backup.sh --dest <dir> [--keep N] [--exclude-archive] [--dry-run]
#   scripts/data-backup.sh --verify <backup-dir>
#
#   --dest <dir>        Destination root. Required. Must be outside the repo.
#   --keep N            Keep the N newest backups in dest (default 7; 0 = keep all).
#   --exclude-archive   Skip data/archive (it is 95%+ of the bytes; use for a
#                       fast, frequent light backup).
#   --dry-run           Print exactly what would happen, write nothing.
#   --verify <dir>      Re-check an existing backup directory against its manifest.
#   --data <dir>        Source tree override (default: <repo>/data). For tests.
#
# Exit: 0 = ok, 1 = failure, 2 = usage error or refusal.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# The repo root is identified by markers that actually exist post-Node-removal.
# (`package.json` was deleted with the Node layer, so a check for it would
# refuse to run in the very tree this script lives in.)
[ -f "$ROOT/Cargo.toml" ] && [ -d "$ROOT/.git" ] \
  || { echo "refusing: not a BlitzkriegBot checkout ($ROOT)" >&2; exit 2; }

DATA="$ROOT/data"
DEST=""
KEEP=7
EXCLUDE_ARCHIVE=0
DRY_RUN=0
VERIFY_DIR=""

usage() { sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --dest)            DEST="${2:-}"; shift 2 ;;
    --keep)            KEEP="${2:-}"; shift 2 ;;
    --exclude-archive) EXCLUDE_ARCHIVE=1; shift ;;
    --dry-run)         DRY_RUN=1; shift ;;
    --verify)          VERIFY_DIR="${2:-}"; shift 2 ;;
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

# ── manifest (computed from the SOURCE, before archiving) ───────────────────
# Recorded first so a corrupt archive can never masquerade as a good backup:
# the hashes describe what was supposed to be captured.
#
# The manifest must describe the SAME tree the archive will hold. Hashing the
# whole source and then excluding a subtree from the tar produces a backup that
# disagrees with itself: `--verify` would report every excluded file as missing.
# So the exclusion is applied to the file list too, not just to tar.
MAN="$TARGET/MANIFEST.sha256"
FILES_LIST="$(mktemp)"
trap 'rm -f "$FILES_LIST"' EXIT

if [ "$EXCLUDE_ARCHIVE" -eq 1 ]; then
  find "$DATA_ABS" -path "$DATA_ABS/archive" -prune -o -type f -print \
    | LC_ALL=C sort > "$FILES_LIST"
else
  find "$DATA_ABS" -type f -print | LC_ALL=C sort > "$FILES_LIST"
fi
N_FILES=0
N_BYTES=0
while IFS= read -r f; do
  [ -n "$f" ] || continue
  rel="${f#"$SRC_PARENT"/}"
  h="$(sha256_file "$f")" || fail "cannot hash $rel"
  printf '%s  %s\n' "$h" "$rel" >> "$MAN"
  N_FILES=$((N_FILES + 1))
  sz="$(file_bytes "$f")"
  N_BYTES=$((N_BYTES + sz))
done < "$FILES_LIST"

echo "  files  : $N_FILES ($N_BYTES bytes)"

# ── archive ─────────────────────────────────────────────────────────────────
if ! tar "${TAR_ARGS[@]}" "$TARGET/data.tar.gz" "${EXCLUDES[@]+"${EXCLUDES[@]}"}" -C "$SRC_PARENT" "$SRC_BASE"; then
  fail "tar failed; a partial backup is left at $TARGET (do not treat it as valid)"
fi
ARC_SHA="$(sha256_file "$TARGET/data.tar.gz")" || fail "cannot hash archive"
ARC_BYTES="$(file_bytes "$TARGET/data.tar.gz")"
printf '# archive-sha256 %s\n' "$ARC_SHA" >> "$MAN"
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
