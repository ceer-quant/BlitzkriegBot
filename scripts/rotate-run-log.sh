#!/usr/bin/env bash
# rotate-run-log.sh — bound run.log so it can never grow into the hundreds of MB.
#
# Why: a huge log is not just disk — when an agent greps/cats it, that output
# enters the conversation context and is re-sent on EVERY later turn (compounding
# token cost). Rotation keeps the file small; combined with bounded reads
# (`tail -c 200000` / `grep -m 50`) it keeps monitoring cheap.
#
# Safe on a live file: `node dist/index.js >> run.log` opens with O_APPEND, so
# truncating in place keeps the same inode/fd and the next write lands at offset
# 0 — no restart, no lost file handle, no sparse hole.
#
# Usage:  scripts/rotate-run-log.sh [max_bytes]   (default 20 MiB)
# Keeps one compressed archive: run.log.1.gz (replaced each run).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

MAX=${1:-20971520}   # 20 MiB
LOG=run.log
[ -f "$LOG" ] || { echo "rotate-run-log: no $LOG"; exit 0; }

size=$(stat -f%z "$LOG" 2>/dev/null || stat -c%s "$LOG")
if [ "$size" -lt "$MAX" ]; then
  echo "rotate-run-log: $LOG is $((size/1024/1024))MiB (< $((MAX/1024/1024))MiB) — nothing to do"
  exit 0
fi

# Archive a compressed copy (replaces the previous archive), then truncate in place.
if gzip -c "$LOG" > "$LOG.1.gz" 2>/dev/null; then
  : > "$LOG"          # truncate; safe under O_APPEND for the live process
  echo "rotate-run-log: archived $((size/1024/1024))MiB -> $LOG.1.gz; $LOG truncated"
else
  echo "rotate-run-log: gzip failed; leaving $LOG untouched" >&2
  exit 1
fi
