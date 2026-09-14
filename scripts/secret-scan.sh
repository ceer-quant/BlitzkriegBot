#!/usr/bin/env bash
# Dependency-free secret scanner for BlitzkriegBot.
#
# Scans TRACKED files (so it respects .gitignore) for high-signal credential
# patterns: private-key blocks, cloud/provider tokens, JWTs and generic
# secret=... assignments. No network, no external tools — safe to run locally,
# in a pre-commit hook, and in CI.
#
# Usage:
#   scripts/secret-scan.sh              # tracked files (docs/ tests/ md/ lockfiles excluded)
#   scripts/secret-scan.sh --all        # scan everything tracked, including docs/tests/markdown
#   scripts/secret-scan.sh --history    # additionally scan all commit diffs
#
# Exit: 0 = clean, 1 = findings, 2 = scanner error.
set -uo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "not a git repository" >&2; exit 2; }
cd "$ROOT"
PAT_FILE="scripts/secret-scan.patterns"
[ -f "$PAT_FILE" ] || { echo "missing $PAT_FILE" >&2; exit 2; }

SCAN_ALL=0
SCAN_HISTORY=0
for arg in "$@"; do
  case "$arg" in
    --all)     SCAN_ALL=1 ;;
    --history) SCAN_HISTORY=1 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

# Paths that legitimately hold *example*/fake credentials. Excluded from the
# default scan; --all includes them. The scanner's own definition files are
# always excluded — they must be able to name the patterns they detect.
EXCLUDES=(
  ':!.env.example'
  ':!*.md'
  ':!package-lock.json'
  ':!**/Cargo.lock'
  ':!assets/**'
  ':!public/**'
  ':!node_modules/**'
  ':!scripts/secret-scan.sh'
  ':!scripts/secret-scan.patterns'
)
if [ "$SCAN_ALL" -eq 0 ]; then
  EXCLUDES+=( ':!docs/**' ':!tests/**' )
fi

# Only report a file:line, never the matched value.
locate() { sed -E 's/^([^:]+:[0-9]+):.*/\1/'; }

FINDINGS=0
echo "== secret-scan :: tracked working tree =="
while IFS= read -r pat; do
  [ -z "$pat" ] && continue
  hits="$(git grep -nIE -e "$pat" -- . "${EXCLUDES[@]}" 2>/dev/null || true)"
  if [ -n "$hits" ]; then
    echo "  !! rule: $pat"
    printf '%s\n' "$hits" | locate | sed 's/^/     /'
    FINDINGS=$((FINDINGS + $(printf '%s\n' "$hits" | grep -c .)))
  fi
done < "$PAT_FILE"

if [ "$SCAN_HISTORY" -eq 1 ]; then
  echo "== secret-scan :: full history (added+removed lines) =="
  # A dependency-free history sweep: grep every commit diff, honouring the same
  # path exclusions as the working-tree scan (docs/tests/markdown legitimately
  # contain placeholder key bodies, and the scanner files name the patterns).
  HIST_PATHS=( . ':!.env.example' ':!*.md' ':!docs/**' ':!tests/**' ':!assets/**'
               ':!public/**' ':!package-lock.json' ':!**/Cargo.lock' ':!node_modules/**'
               ':!scripts/secret-scan.sh' ':!scripts/secret-scan.patterns' )
  COMBINED="$(tr '\n' '|' < "$PAT_FILE" | sed 's/|$//')"
  HITS="$(git log --all -p --pretty=format: -- "${HIST_PATHS[@]}" 2>/dev/null \
            | grep -nIE -e "$COMBINED" || true)"
  if [ -n "$HITS" ]; then
    n="$(printf '%s\n' "$HITS" | grep -c .)"
    echo "  !! history findings: $n line(s)"
    printf '%s\n' "$HITS" | sed -E 's/^([0-9]+:[+-]+).*/\1 <redacted>/' | head -20
    FINDINGS=$((FINDINGS + n))
  fi
fi

echo
if [ "$FINDINGS" -gt 0 ]; then
  echo "FAIL: $FINDINGS potential secret line(s) found. Review the locations above." >&2
  exit 1
fi
echo "OK: no secrets detected."
exit 0
