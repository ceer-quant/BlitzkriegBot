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
# ── WHY A LAUNCHER ON THE INTERNAL DISK (issue #217) ────────────────────────
# The first version of this installer pointed ProgramArguments at
# `<repo>/scripts/data-backup-cli.sh` on the external volume. The first scheduled
# run then produced exactly this, and nothing else, for a day:
#
#   /bin/sh: /Volumes/Hard Disk/BlitzkriegBot/scripts/data-backup-cli.sh: Operation not permitted
#
# macOS TCC denies a launchd-spawned process access to a removable volume, and
# the denial covers BOTH read and exec (measured with a probe agent: `ls` of the
# repository and `head` of a file inside it both return EPERM). So:
#   * the launchd job itself is registered and "installed" — launchctl list still
#     showed exit code 0 — while zero backups existed, and
#   * moving the launcher to the internal disk is NECESSARY (a /bin/sh that
#     cannot even read its script cannot report anything) but NOT SUFFICIENT:
#     the script it runs still lives on the denied volume.
#
# What this installer therefore generates is a small launcher on the INTERNAL
# disk, $HOME/Library/Application Support/blitzkrieg/data-backup-launch.sh, whose
# only job is to fail LOUDLY and with a non-zero exit code when the context
# cannot read the repository, and otherwise exec the real CLI. The remaining
# requirement — Full Disk Access for /bin/sh — is a security-posture change and
# belongs to the user (D-33); the install-time probe below tells them, in one
# command, whether it is in effect yet.
#
# Full Disk Access is NOT needed for the alternative route, which needs no
# posture change and works today: scripts/data-backup-loop.sh (a detached process
# from the operator's own session, inheriting that session's TCC access, exactly
# like scripts/soak-resident.sh). It does not survive a reboot; see its header.
#
# Usage:
#   bash scripts/data-backup-install.sh              install + start + probe
#   bash scripts/data-backup-install.sh --no-verify  install without the probe
#   bash scripts/data-backup-install.sh --verify-timeout 120   probe window (s)
#   bash scripts/data-backup-install.sh --uninstall  remove both agents + launcher
#
# Env: BLITZKRIEG_BACKUP_DIR overrides the destination root
#      (default: /Volumes/Hard Disk/BlitzkriegBotBackup).
#      BK_REPO_ROOT points the launcher at a different checkout (default: this one).

set -eu

# Usage text is this file's own header comment: one place to keep true, and it
# cannot drift from the flags below. Stops at the first non-comment line.
usage() { awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "$0"; }

repo_root="${BK_REPO_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
backup_cli="$repo_root/scripts/data-backup-cli.sh"
backup_sh="$repo_root/scripts/data-backup.sh"

BACKUP_DIR="${BLITZKRIEG_BACKUP_DIR:-/Volumes/Hard Disk/BlitzkriegBotBackup}"
LABELS=(com.blitzkrieg.databackup.full com.blitzkrieg.databackup.light)
LAUNCH_DIR="$HOME/Library/LaunchAgents"
LOG_DIR="$HOME/Library/Logs"
SUPPORT_DIR="$HOME/Library/Application Support/blitzkrieg"
LAUNCHER="$SUPPORT_DIR/data-backup-launch.sh"
STATUS_DIR="$HOME/Library/Logs"      # where scripts/lib/backup-attempt.sh writes
UID_N=$(id -u)

VERIFY=1
PROBE_SECS=45

while [ $# -gt 0 ]; do
  case "$1" in
    --uninstall) UNINSTALL=1; shift ;;
    --no-verify) VERIFY=0; shift ;;
    --verify-timeout) PROBE_SECS="${2:-45}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown option: $1" >&2; exit 2 ;;
  esac
done

[ -f "$backup_sh" ] || { echo "error: $backup_sh missing" >&2; exit 1; }
[ -f "$backup_cli" ] || { echo "error: $backup_cli missing" >&2; exit 1; }

if [ "${UNINSTALL:-0}" = "1" ]; then
  for label in "${LABELS[@]}"; do
    launchctl bootout "gui/$UID_N/$label" 2>/dev/null || true
    rm -f "$LAUNCH_DIR/$label.plist"
    echo "removed: $label"
  done
  # The launcher is generated, so it goes with the agents. The directory is left
  # in place (another blitzkrieg agent may use it) but is only removed when empty.
  rm -f "$LAUNCHER"
  rmdir "$SUPPORT_DIR" 2>/dev/null || true
  echo "removed: $LAUNCHER"
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

mkdir -p "$LAUNCH_DIR" "$LOG_DIR" "$SUPPORT_DIR"

# ── the internal-disk launcher ──────────────────────────────────────────────
# Generated, not templated in the plist, for three reasons: the preflight needs
# several lines, it must be editable by the operator without re-reading a plist,
# and a file on the internal disk is the one thing a TCC-denied launchd context
# can still read (which is what makes the loud failure possible at all).
#
# Placeholders are substituted rather than interpolated, so the launcher's own
# `$1`/`$?` survive into the generated file verbatim.
write_launcher() {
  cat <<'LAUNCHER_HEAD' > "$LAUNCHER"
#!/bin/sh
# data-backup-launch.sh — GENERATED by scripts/data-backup-install.sh; edits are
# overwritten on re-install. Lives on the internal disk on purpose (issue #217):
# a launchd-spawned /bin/sh can read a file here even when macOS TCC denies it the
# external volume this repository lives on. Its contract:
#   * repository readable  → exec the real CLI (which records the attempt)
#   * repository NOT readable → say so, with a date and a reason, EXIT NON-ZERO
# so that a refused run can never again look like a schedule that simply has not
# fired yet.
# Called by launchd as: /bin/sh <this file> <light|full>
LAUNCHER_HEAD
  cat <<'LAUNCHER_BODY' >> "$LAUNCHER"
set -u

TIER="${1:-light}"
REPO="${BK_REPO_ROOT:-__REPO_ROOT__}"
BACKUP_ROOT="${BLITZKRIEG_BACKUP_DIR:-__BACKUP_DIR__}"
LOGFILE="${BK_BACKUP_LOG_DIR:-$HOME/Library/Logs}/blitzkrieg-data-backup-$TIER.log"
CLI="$REPO/scripts/data-backup-cli.sh"

# Emitted on both streams: launchd redirects StandardOutPath and
# StandardErrorPath at $LOGFILE, so these lines land in the scheduler log even if
# this process cannot write there itself. The LAST line is deliberately a bare
# `FAIL: …` — that is the shape data-backup.sh --status classifies as a failure,
# and it carries the TCC phrase so either signature matches.
deny() {
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] launchd context cannot access the repository: $1"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] macOS TCC denies a launchd-spawned process access to the volume this"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] repository lives on ($REPO). Grant Full Disk Access to /bin/sh to allow it:"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')]   System Settings > Privacy & Security > Full Disk Access > + > /bin/sh (D-33)"
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] or run the no-FDA route: 'blitzkrieg backup' / scripts/data-backup-loop.sh start"
  echo "FAIL: $TIER backup refused — $1 (Operation not permitted). See https://github.com/ceer-quant/BlitzkriegBot/issues/217"
  exit 126
}

[ -n "$REPO" ] || deny "BK_REPO_ROOT is empty"
# A real read, not `[ -r ]`: TCC enforces at open(2), and a permission bit is not
# the question being asked here.
[ -d "$REPO" ] || deny "repository not found at $REPO"
head -c 1 "$CLI" >/dev/null 2>&1 || deny "cannot read $CLI"
head -c 1 "$REPO/Cargo.toml" >/dev/null 2>&1 || deny "cannot read $REPO/Cargo.toml"
if [ -e "$BACKUP_ROOT" ]; then
  ls "$BACKUP_ROOT" >/dev/null 2>&1 || deny "cannot read the backup root $BACKUP_ROOT"
fi

# The repo is reachable: hand over to the real CLI, which writes the attempt
# record and prunes. `--attempt-source launchd` is what makes its outcome visible
# to `blitzkrieg backup --status` as SCHEDULER evidence (a hand run must not count).
if [ "$TIER" = "light" ]; then
  exec /bin/sh "$CLI" --light --attempt-source launchd
fi
exec /bin/sh "$CLI" --attempt-source launchd
LAUNCHER_BODY
  # Path substitution. `|` as the delimiter; neither value can contain one. Written
  # to a sibling file and moved, rather than `sed -i`: BSD and GNU disagree about
  # `-i ''`, and this file must stay readable on both.
  sed -e "s|__REPO_ROOT__|$repo_root|g" -e "s|__BACKUP_DIR__|$BACKUP_DIR|g" \
    "$LAUNCHER" > "$LAUNCHER.new" && mv -f "$LAUNCHER.new" "$LAUNCHER"
  rm -f "$LAUNCHER.new"
  chmod 755 "$LAUNCHER"
  sh -n "$LAUNCHER" || { echo "error: generated launcher does not parse: $LAUNCHER" >&2; exit 1; }
}

write_launcher
echo "launcher: $LAUNCHER"

emit_plist() {
  local label="$1" schedule_xml="$2" tier="$3" log="$4"
  cat > "$LAUNCH_DIR/$label.plist" <<XML
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>$LAUNCHER</string>
    <string>$tier</string>
  </array>
  $schedule_xml
  <key>EnvironmentVariables</key>
  <dict>
    <key>BK_REPO_ROOT</key><string>$repo_root</string>
    <key>BLITZKRIEG_BACKUP_DIR</key><string>$BACKUP_DIR</string>
    <key>BK_BACKUP_LOG_DIR</key><string>$LOG_DIR</string>
    <key>PATH</key><string>/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin</string>
  </dict>
  <key>ProcessType</key><string>Background</string>
  <key>Nice</key><integer>10</integer>
  <key>LowPriorityIO</key><true/>
  <key>StandardOutPath</key><string>$log</string>
  <key>StandardErrorPath</key><string>$log</string>
</dict>
</plist>
XML
}

emit_plist "com.blitzkrieg.databackup.light" \
  '<key>StartCalendarInterval</key><dict><key>Hour</key><integer>4</integer><key>Minute</key><integer>0</integer></dict>' \
  "light" "$LOG_DIR/blitzkrieg-data-backup-light.log"

# Weekday 0 = Sunday: one full backup per week, offset from the daily light run.
emit_plist "com.blitzkrieg.databackup.full" \
  '<key>StartCalendarInterval</key><dict><key>Weekday</key><integer>0</integer><key>Hour</key><integer>4</integer><key>Minute</key><integer>30</integer></dict>' \
  "full" "$LOG_DIR/blitzkrieg-data-backup-full.log"

# Register: bootout first so a re-install replaces cleanly (bootstart from a
# stale, already-loaded plist would keep the OLD baked-in schedule).
for label in "${LABELS[@]}"; do
  launchctl bootout "gui/$UID_N/$label" 2>/dev/null || true
  chmod 644 "$LAUNCH_DIR/$label.plist"
  launchctl bootstrap "gui/$UID_N" "$LAUNCH_DIR/$label.plist" || {
    echo "error: launchctl bootstrap failed for $label (see above)" >&2
    exit 1
  }
  echo "installed: $label  ($LAUNCH_DIR/$label.plist)"
done

echo
echo "KI-24 periodic backup is registered (D-27 plan):"
echo "  light : daily 04:00    → $BACKUP_DIR/light   (keep 7, archive excluded)"
echo "  full  : Sunday 04:30   → $BACKUP_DIR/full    (keep 4)"
echo "  logs  : $LOG_DIR/blitzkrieg-data-backup-*.log"

# ── install-time probe (issue #217) ─────────────────────────────────────────
# Registering a schedule is not the same as having a working one, and the
# difference used to take a day to notice. This kickstarts the LIGHT agent — a
# real run, small (the archive is excluded, ~1 GB here vs 8 GB for full) — and
# watches for the ONE thing that must never again be quiet: the TCC refusal.
#
# Only light is kicked: a full kickstart would write an 8 GB backup on install,
# and the failure mode being probed is identical for both agents (same launcher).
probe_light() {
  local label="com.blitzkrieg.databackup.light"
  local log="$LOG_DIR/blitzkrieg-data-backup-light.log"
  local start rc elapsed last artifact
  start="$(date +%s)"
  echo
  echo "probing the light agent (kickstart; ${PROBE_SECS}s window)…"
  launchctl kickstart -k "gui/$UID_N/$label" 2>&1 || {
    echo "  note: kickstart returned non-zero — reading the result anyway"
  }

  elapsed=0
  while [ "$elapsed" -lt "$PROBE_SECS" ]; do
    # Success evidence: a backup directory created after the kickstart.
    artifact="$(ls -dt "$BACKUP_DIR"/light/blitzkrieg-data-* 2>/dev/null | head -1 || true)"
    if [ -n "$artifact" ] && [ "$(python3 -c 'import os,sys;print(int(os.path.getmtime(sys.argv[1])))' "$artifact" 2>/dev/null || echo 0)" -ge "$start" ]; then
      echo "  PASS: a new light backup appeared — $(basename "$artifact")"
      echo "        ($artifact)"
      return 0
    fi
    # Refusal evidence: the launcher's FAIL line, or the CLI's own error line.
    if [ -f "$log" ]; then
      last="$(grep -v '^[[:space:]]*$' "$log" 2>/dev/null | tail -1)"
      case "$last" in
        *"Operation not permitted"*|*"FAIL:"*|*"refusing:"*|*"error:"*)
          echo "  FAIL: the scheduled run was refused:"
          echo "        $last"
          return 1 ;;
      esac
    fi
    # Refusal evidence that never reached the log: a non-zero last exit code.
    # `launchctl print` rather than `launchctl list <label>`: the legacy form
    # prints a whole dict for a loaded job (its `$2` is not the status), so the
    # parse that "looks standard" is exactly the one that silently reads nothing.
    # `(never exited)` is what an un-run job reports, and is not a failure.
    rc="$(launchctl print "gui/$UID_N/$label" 2>/dev/null \
          | sed -n 's/^[[:space:]]*last exit code = //p' | head -1)"
    case "${rc:-}" in
      ''|*'(never exited)'*) : ;;
      0) : ;;
      *) echo "  FAIL: $label exited $rc — last log line: ${last:-<none>}"
         return 1 ;;
    esac
    sleep 5
    elapsed=$((elapsed + 5))
  done

  # Neither: the run is under way (a real light backup takes minutes). Not a pass,
  # not a failure — say which, and say how to settle it.
  echo "  IN PROGRESS: no refusal seen in ${PROBE_SECS}s and no artifact yet."
  echo "        Confirm with: bash scripts/data-backup.sh --status   (or 'blitzkrieg backup --status')"
  return 0
}

probe_rc=0
if [ "$VERIFY" = "1" ]; then
  probe_light || probe_rc=1
else
  echo
  echo "probe skipped (--no-verify). Verify with: bash scripts/data-backup.sh --status"
fi

if [ "$probe_rc" -ne 0 ]; then
  cat >&2 <<'FDA'

────────────────────────────────────────────────────────────────────────────
The schedule is registered but the launchd route CANNOT READ THIS REPOSITORY
until macOS grants it access. This is a user action; it cannot be automated:

  1. Open System Settings → Privacy & Security → Full Disk Access
  2. Click +, press ⌘⇧G, type /bin/sh, add it, and make sure its switch is ON
     (/bin/sh is the executable launchd starts — see ProgramArguments[0] in
      ~/Library/LaunchAgents/com.blitzkrieg.databackup.*.plist)
  3. Run: bash scripts/data-backup-install.sh        # re-probe
     …or wait for it to be exercised, then: bash scripts/data-backup.sh --status
  4. Verify the result is `PASS` / an `ok` status line — not merely "installed".

UNTIL THEN: use the route that needs no permission change —
    blitzkrieg backup                 # one backup now, from your own session
    scripts/data-backup-loop.sh start # the daily/weekly schedule, detached
(does not survive a reboot; see scripts/data-backup-loop.sh)

This is issue #217.
────────────────────────────────────────────────────────────────────────────
FDA
  exit 1
fi

echo
echo "manual one-click (no args to remember):"
echo "  blitzkrieg backup            # full now"
echo "  blitzkrieg backup --light    # light now (the daily tier)"
echo "  blitzkrieg backup --list     # what exists"
echo "  blitzkrieg backup --verify   # check the newest full backup"
echo "  blitzkrieg backup --status   # is a backup actually HAPPENING? (exit 1 if not)"
