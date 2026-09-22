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
# ── THE FILES IT INSTALLS ARE TEMPLATES IN THIS REPOSITORY ──────────────────
# The launcher and both plists live in scripts/templates/ and are rendered here
# (placeholders only — see render_template). They are checked in, rather than
# generated from heredocs, for two reasons that are really one:
#   * the next person must be able to READ what will run, and the rendered copy
#     lives in ~/Library where nobody looks; "the plist exists" is precisely the
#     false comfort #217 is about, and
#   * a second copy of the launcher inside this installer would drift from the
#     checked-in one, and a launcher that disagrees with its template is the same
#     class of bug one level up.
# scripts/templates/README.md documents the manual route (copy the template,
# substitute, bootout/bootstrap) and states what the route cannot do on its own:
# moving the launcher does not grant anything, the repository must still be
# readable, and without Full Disk Access (D-33) EVERY scheduled run is refused.
#
# Usage:
#   bash scripts/data-backup-install.sh              install + start + probe
#   bash scripts/data-backup-install.sh --no-verify  install without the probe
#   bash scripts/data-backup-install.sh --verify-timeout 120   probe window (s)
#   bash scripts/data-backup-install.sh --uninstall  remove both agents + launcher
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
TEMPLATE_DIR="$repo_root/scripts/templates"
LAUNCHER_TEMPLATE="$TEMPLATE_DIR/data-backup-launch.sh"

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

# ── rendering the checked-in templates ──────────────────────────────────────
# One substitution list for both templates. A placeholder that does not occur in the
# template being rendered is a no-op, which is what lets the launcher and the plists
# share it — and lets data-backup-check.mjs assert that every `__X__` in
# scripts/templates/ appears here (a placeholder nobody substitutes ships a launcher
# that tries to read a directory literally named __REPO_ROOT__).
#
# `|` as the delimiter: no substituted value may contain one. Textual, not
# interpolated, so the launcher's own `$1`/`$?`/`${VAR:-…}` survive verbatim. Written
# to a sibling file and moved, rather than `sed -i`: BSD and GNU disagree about
# `-i ''`, and the plists must be produced identically on both.
render_template() { # <template file> <tier> <label>   → stdout
  sed -e "s|__LABEL__|$3|g" \
      -e "s|__TIER__|$2|g" \
      -e "s|__LAUNCHER__|$LAUNCHER|g" \
      -e "s|__REPO_ROOT__|$repo_root|g" \
      -e "s|__BACKUP_DIR__|$BACKUP_DIR|g" \
      -e "s|__LOG_DIR__|$LOG_DIR|g" \
      "$1"
}

# ── the internal-disk launcher ──────────────────────────────────────────────
# Rendered from scripts/templates/data-backup-launch.sh. It sits on the internal
# disk because that is the one thing a TCC-denied launchd context can still read
# (which is what makes the loud failure possible at all), and it is a template in
# the repository because what a scheduled job will run must be reviewable in review.
#
# Tier and label are empty here: the launcher template has neither placeholder, and
# an unmatched `s|__TIER__||g` leaves the text alone.
write_launcher() {
  [ -f "$LAUNCHER_TEMPLATE" ] || {
    echo "error: $LAUNCHER_TEMPLATE missing (BK_REPO_ROOT=$repo_root?)" >&2
    exit 1
  }
  render_template "$LAUNCHER_TEMPLATE" "" "" > "$LAUNCHER.new" || {
    echo "error: could not render $LAUNCHER_TEMPLATE" >&2
    rm -f "$LAUNCHER.new"
    exit 1
  }
  mv -f "$LAUNCHER.new" "$LAUNCHER"
  chmod 755 "$LAUNCHER"
  # Both a parse check and a placeholder check: a launcher that parses but still
  # contains __REPO_ROOT__ would fail at 04:00, in a context nobody watches.
  sh -n "$LAUNCHER" || { echo "error: rendered launcher does not parse: $LAUNCHER" >&2; exit 1; }
  if grep -q '__[A-Z_]*__' "$LAUNCHER"; then
    echo "error: rendered launcher still contains a placeholder:" >&2
    grep -n '__[A-Z_]*__' "$LAUNCHER" >&2
    exit 1
  fi
}

write_launcher
echo "launcher: $LAUNCHER   (from ${LAUNCHER_TEMPLATE#$repo_root/})"

# Both agents are rendered from scripts/templates/com.blitzkrieg.databackup.<tier>.plist,
# so the schedule an operator reads in review is the schedule launchd loads. The
# template file name carries the tier: label and tier are the same three strings
# everywhere, and deriving one from the other is what keeps them from disagreeing.
emit_plist() { # <tier>
  local tier="$1" label="com.blitzkrieg.databackup.$1"
  local template="$TEMPLATE_DIR/$label.plist"
  [ -f "$template" ] || { echo "error: $template missing" >&2; exit 1; }
  render_template "$template" "$tier" "$label" > "$LAUNCH_DIR/$label.plist.new" || {
    echo "error: could not render $template" >&2
    rm -f "$LAUNCH_DIR/$label.plist.new"
    exit 1
  }
  mv -f "$LAUNCH_DIR/$label.plist.new" "$LAUNCH_DIR/$label.plist"
  # A rendered plist is text with paths substituted into it, so a value containing
  # `&` or `<` silently produces XML that launchd refuses to load — at 04:00, into a
  # log nobody reads. Lint it here instead, where a human is watching.
  if command -v plutil >/dev/null 2>&1; then
    plutil -lint "$LAUNCH_DIR/$label.plist" >/dev/null 2>&1 || {
      echo "error: rendered plist is not valid XML: $LAUNCH_DIR/$label.plist" >&2
      echo "       (a substituted path containing & or < would do this)" >&2
      exit 1
    }
  fi
}

emit_plist light
emit_plist full

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
