#!/bin/bash
# data-backup-install.sh — make the KI-24 backup actually periodic.
#
# scripts/data-backup.sh (verified, refusal-guarded) plus scripts/data-backup-cli.sh
# (the 傻白甜 front door) already exist; what the incident left us without was a
# SCHEDULE — a manual backup still depends on a human remembering, and 2026-09-17
# is the recorded cost of that. This installer registers two macOS LaunchAgents
# with the plan DECIDED in D-27 (user ruling 2026-09-19):
#
#   com.blitzkrieg.databackup.light  daily 04:00        — archive skipped, keep 7
#   com.blitzkrieg.databackup.full   Sunday 04:30       — everything, keep 4
#
# A day's capture is therefore never more than a week from the last full copy,
# and the mutable small state (trades/orders/positions/soak/strategy-state) is
# at most a day old. The two destinations are separate subroots (light/ full/)
# because the prune in data-backup.sh matches its name pattern within one root
# — two schedules sharing a root would prune each other.
#
# Usage:
#   bash scripts/data-backup-install.sh              install + start schedules
#   bash scripts/data-backup-install.sh --uninstall  remove both agents
#
# Env: BLITZKRIEG_BACKUP_DIR overrides the destination root
#      (default: /Volumes/Hard Disk/BlitzkriegBotBackup).

set -eu

repo_root=$(cd "$(dirname "$0")/.." && pwd)
backup_cli="$repo_root/scripts/data-backup-cli.sh"
backup_sh="$repo_root/scripts/data-backup.sh"

BACKUP_DIR="${BLITZKRIEG_BACKUP_DIR:-/Volumes/Hard Disk/BlitzkriegBotBackup}"
LABELS=(com.blitzkrieg.databackup.full com.blitzkrieg.databackup.light)
LAUNCH_DIR="$HOME/Library/LaunchAgents"
LOG_DIR="$HOME/Library/Logs"
UID_N=$(id -u)

[ -f "$backup_sh" ] || { echo "error: $backup_sh missing" >&2; exit 1; }
[ -f "$backup_cli" ] || { echo "error: $backup_cli missing" >&2; exit 1; }

if [ "${1:-}" = "--uninstall" ]; then
  for label in "${LABELS[@]}"; do
    launchctl bootout "gui/$UID_N/$label" 2>/dev/null || true
    rm -f "$LAUNCH_DIR/$label.plist"
    echo "removed: $label"
  done
  echo "(backups themselves are untouched: $BACKUP_DIR)"
  exit 0
fi

# The destination must exist before a schedule can use it: a silent missing
# volume would turn the nightly run into a daily cron of failure emails — or
# worse, log-quiet no-ops. Fail here, at install time, with a real message.
for sub in light full; do
  mkdir -p "$BACKUP_DIR/$sub" 2>/dev/null || {
    echo "error: cannot create $BACKUP_DIR/$sub — is the backup volume mounted?" >&2
    echo "       mount it, or set BLITZKRIEG_BACKUP_DIR to an existing external path" >&2
    exit 2
  }
done

mkdir -p "$LAUNCH_DIR" "$LOG_DIR"

emit_plist() {
  local label="$1" schedule_xml="$2" args="$3" log="$4"
  cat > "$LAUNCH_DIR/$label.plist" <<XML
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-lc</string>
    <string>$args</string>
  </array>
  $schedule_xml
  <key>ProcessType</key><string>Background</string>
  <key>Nice</key><integer>10</integer>
  <key>LowPriorityIO</key><true/>
  <key>StandardOutPath</key><string>$log</string>
  <key>StandardErrorPath</key><string>$log</string>
</dict>
</plist>
XML
}

FULL_ARGS="\"$backup_cli\""
LIGHT_ARGS="\"$backup_cli\" --light"

emit_plist "com.blitzkrieg.databackup.light" \
  '<key>StartCalendarInterval</key><dict><key>Hour</key><integer>4</integer><key>Minute</key><integer>0</integer></dict>' \
  "$LIGHT_ARGS" "$LOG_DIR/blitzkrieg-data-backup-light.log"

# Weekday 0 = Sunday: one full backup per week, offset from the daily light run.
emit_plist "com.blitzkrieg.databackup.full" \
  '<key>StartCalendarInterval</key><dict><key>Weekday</key><integer>0</integer><key>Hour</key><integer>4</integer><key>Minute</key><integer>30</integer></dict>' \
  "$FULL_ARGS" "$LOG_DIR/blitzkrieg-data-backup-full.log"

# Register: bootout first so a re-install replaces cleanly (bootstart from a
# stale, already-loaded plist would keep the OLD baked-in schedule).
uid=$(id -u)
for label in "${LABELS[@]}"; do
  launchctl bootout "gui/$uid/$label" 2>/dev/null || true
  chmod 644 "$LAUNCH_DIR/$label.plist"
  launchctl bootstrap "gui/$uid" "$LAUNCH_DIR/$label.plist" || {
    echo "error: launchctl bootstrap failed for $label (see above)" >&2
    exit 1
  }
  echo "installed: $label  ($LAUNCH_DIR/$label.plist)"
done

echo
echo "KI-24 periodic backup is live (D-27 plan):"
echo "  light : daily 04:00    → $BACKUP_DIR/light   (keep 7, archive excluded)"
echo "  full  : Sunday 04:30   → $BACKUP_DIR/full    (keep 4)"
echo "  logs  : $LOG_DIR/blitzkrieg-data-backup-*.log"
echo
echo "manual one-click (no args to remember):"
echo "  blitzkrieg backup            # full now"
echo "  blitzkrieg backup --list     # what exists"
echo "  blitzkrieg backup --verify   # check the newest full backup"
