#!/usr/bin/env bash
# 72-hour dry soak of postonly_pair, with market-data archiving.
#
# macOS: system bash is 3.2 and the user shell is zsh. This file uses neither
# bash-4 features nor GNU coreutils. `zsh scripts/run-72h-dry.sh` works too.
# On Darwin the kernel is wrapped in `caffeinate -ims` so the machine does not
# idle-sleep for the run (-s holds only while on AC power).
#
# The child is always DRY_RUN=true and --mode dry. This script does not edit
# .env.example.
set -eu
if [ -n "${BASH_VERSION:-}" ]; then
  set -o pipefail
elif [ -n "${ZSH_VERSION:-}" ]; then
  setopt pipefail
fi

script_file() {
  if [ -n "${ZSH_VERSION:-}" ]; then
    eval 'printf %s "${(%):-%x}"'
  elif [ -n "${BASH_VERSION:-}" ]; then
    printf %s "${BASH_SOURCE[0]}"
  else
    printf %s "$0"
  fi
}

ROOT="$(cd "$(dirname "$(script_file)")/.." && pwd -P)"
cd "$ROOT"

install_launchd() {
  if [ "$(uname -s)" != "Darwin" ]; then
    echo "run-72h-dry: --install-launchd is macOS only" >&2
    exit 2
  fi
  log_dir="${HOME}/Library/Logs/blitzkrieg"
  dest="${HOME}/Library/LaunchAgents/com.blitzkrieg.soak72h.plist"
  mkdir -p "$log_dir" "${HOME}/Library/LaunchAgents"
  python3 - "$ROOT/scripts/templates/com.blitzkrieg.soak72h.plist" "$ROOT" "$log_dir" "$dest" <<'PY'
import pathlib, sys
src, root, log_dir, dest = sys.argv[1:]
def xml(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
text = pathlib.Path(src).read_text()
text = text.replace("__REPO_ROOT__", xml(root)).replace("__LOG_DIR__", xml(log_dir))
pathlib.Path(dest).write_text(text)
PY
  if ! /usr/bin/plutil -lint "$dest"; then
    echo "run-72h-dry: rendered plist failed plutil -lint" >&2
    exit 2
  fi
  echo "run-72h-dry: wrote $dest"
  echo "  log: $log_dir/soak72h.log"
  echo "  load:   launchctl bootstrap gui/$(id -u) \"$dest\""
  echo "  unload: launchctl bootout gui/$(id -u)/com.blitzkrieg.soak72h"
  echo "  This restarts the DRY soak only. It does not start live trading."
  if [ "${1:-}" = "--load" ]; then
    launchctl bootout "gui/$(id -u)/com.blitzkrieg.soak72h" >/dev/null 2>&1 || true
    launchctl bootstrap "gui/$(id -u)" "$dest"
    echo "run-72h-dry: launchd job loaded"
  fi
}

if [ "${1:-}" = "--install-launchd" ]; then
  install_launchd "${2:-}"
  exit 0
fi

BIN="${BK_CORE_BIN:-$ROOT/target/release/blitzkrieg-core}"
if [ ! -x "$BIN" ]; then
  echo "run-72h-dry: missing $BIN" >&2
  echo "  build it with: cargo build --release -p blitzkrieg-core" >&2
  exit 2
fi

mkdir -p "$ROOT/data/archive" "$ROOT/data/trades"

echo "run-72h-dry: DRY_RUN=true, strategy=postonly_pair, archive=data/archive/events.jsonl"
echo "  trade log: data/trades/trades.jsonl"
echo "  stop with Ctrl-C. Score later with: node scripts/soak-72h-check.mjs --trades data/trades/trades.jsonl"
if [ "$(uname -s)" = "Darwin" ]; then
  echo "  caffeinate -ims is on. Plug into AC power; system-sleep assertion holds only on AC."
fi

set -- env DRY_RUN=true \
  "$BIN" \
  --mode dry \
  --engine \
  --no-strategy-state \
  --enable-strategy postonly_pair \
  --config "$ROOT/user_layer/configs/production-72h.toml" \
  --event-archive "$ROOT/data/archive/events.jsonl" \
  --trade-log "$ROOT/data/trades/trades.jsonl" \
  --seed-balance 100 \
  --min-shares 1 \
  --max-shares 2 \
  --max-order-notional 2 \
  --max-order-notional-pct 2 \
  --size-pct 1

if [ "$(uname -s)" = "Darwin" ] && [ -x /usr/bin/caffeinate ]; then
  exec /usr/bin/caffeinate -ims "$@"
fi
exec "$@"
