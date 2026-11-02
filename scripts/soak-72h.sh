#!/usr/bin/env bash
# 72-hour dry-run launcher checklist (#868).
#
# Default: preflight only. Does not start a process and does not set
# DRY_RUN=false. Pass --print-argv to see the dry command. Pass --start only
# when target/release/blitzkrieg-core exists and you mean to run a dry kernel
# in the foreground. --start refuses unless preflight passed and DRY_RUN is true.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

node "$ROOT/scripts/soak-72h-check.mjs" --preflight

# shellcheck disable=SC1091
set -a
# shellcheck source=/dev/null
source "$ROOT/user_layer/configs/production-72h.env"
set +a

if [[ "${DRY_RUN}" != "true" ]]; then
  echo "soak-72h: refusing to continue, DRY_RUN=${DRY_RUN}" >&2
  exit 1
fi

ARGV=(
  "$ROOT/target/release/blitzkrieg-core"
  --mode dry
  --no-strategy-state
  --config "$ROOT/user_layer/configs/production-72h.toml"
)

while IFS= read -r name; do
  [[ -z "$name" ]] && continue
  ARGV+=(--enable-strategy "$name")
done < <(node -e "
  const fs = require('fs');
  const t = fs.readFileSync('user_layer/configs/production-72h.allowlist','utf8');
  for (const line of t.split('\n')) {
    const s = line.replace(/#.*/, '').trim();
    if (s) console.log(s);
  }
")

echo "soak-72h dry argv (not started):"
printf '  %q' "${ARGV[@]}"
echo

if [[ "${1:-}" == "--print-argv" || "${1:-}" == "" ]]; then
  echo "soak-72h: preflight passed. Nothing was started."
  echo "After the run: node scripts/soak-72h-check.mjs --trades <trades.jsonl>"
  exit 0
fi

if [[ "${1:-}" == "--start" ]]; then
  if [[ ! -x "${ARGV[0]}" ]]; then
    echo "soak-72h: missing ${ARGV[0]}" >&2
    exit 2
  fi
  exec "${ARGV[@]}"
fi

echo "soak-72h: unknown argument ${1}" >&2
exit 2
