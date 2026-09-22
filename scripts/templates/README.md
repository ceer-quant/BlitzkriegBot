# scripts/templates/

The two files a scheduled backup is made of, checked in so they can be **read in
review** rather than only after they land in `~/Library` (issue #217):

| File | What it becomes |
| --- | --- |
| `data-backup-launch.sh` | `~/Library/Application Support/blitzkrieg/data-backup-launch.sh` — the launcher launchd actually runs |
| `com.blitzkrieg.databackup.light.plist` | `~/Library/LaunchAgents/com.blitzkrieg.databackup.light.plist` — daily 04:00 light tier |
| `com.blitzkrieg.databackup.full.plist` | `~/Library/LaunchAgents/com.blitzkrieg.databackup.full.plist` — Sunday 04:30 full tier |

Placeholders are substituted by `scripts/data-backup-install.sh` (`render_template`):
`__LABEL__`, `__TIER__`, `__LAUNCHER__`, `__REPO_ROOT__`, `__BACKUP_DIR__`,
`__LOG_DIR__`. The launcher uses only the last two. `node scripts/data-backup-check.mjs`
(§12) asserts that every placeholder appearing in this directory is on that list, that
the rendered launcher parses and keeps no placeholder behind, and that the refusal path
records itself.

## Read this before you install: what the failure actually was

`com.blitzkrieg.databackup.light` ran daily for a month and produced **zero** backups.
Nothing looked wrong: the plist was in `~/Library/LaunchAgents` and `launchctl list` had
an entry. The only outward trace was one 94-byte log line (the issue's read of the
exit-code column recorded `0`; a fresh read on 2026-09-23 shows `126` — nothing consumed
either one):

```
/bin/sh: /Volumes/Hard Disk/BlitzkriegBot/scripts/data-backup-cli.sh: Operation not permitted
```

macOS TCC denies a launchd-spawned process access to this repository's volume — **read
and exec alike**. The job could not even open its own script.

So, two facts to keep straight:

- **A plist that exists is not a backup that runs.** Neither is `launchctl list`
  showing exit code 0. The only evidence that counts is a fresh artifact on disk plus
  a `blitzkrieg backup --status` line that says `ok` — see below.
- **Moving the launcher to the internal disk is necessary but NOT sufficient.** The
  launcher is still only a shell script: to back anything up it must **read the
  repository and the backup volume**, and TCC denies exactly that. Without Full Disk
  Access for `/bin/sh`, every scheduled run is refused. What the internal-disk launcher
  buys is that the refusal is now *loud*: a `result=fail` attempt record on the internal
  disk, a dated `FAIL: … (Operation not permitted)` line, and exit code `126`.

Granting that access is a user action (decision D-33) and cannot be automated from
here. Route 2 and route 3 of issue #217 — move the checkout to the internal disk, or
have the (already TCC-authorized) trading core trigger the backup itself — are the
alternatives that avoid it; see `README.md` §3.6 and `scripts/README.md` § "Scheduled
backups".

## Preferred: run the installer (it renders these and then probes)

```bash
bash scripts/data-backup-install.sh                 # render + bootstrap + probe
bash scripts/data-backup-install.sh --no-verify     # skip the probe
bash scripts/data-backup-install.sh --verify-timeout 120
bash scripts/data-backup-install.sh --uninstall     # bootout + remove launcher/plists
```

It renders each template into place, lints the rendered plist with `plutil -lint`,
parses the rendered launcher with `sh -n`, refuses to finish if a placeholder survived,
and then **kickstarts the light agent and watches it**: a `PASS` means a new backup
directory appeared, a `FAIL` prints the refusal and exits non-zero (re-running it *is*
the re-probe after granting Full Disk Access).

## Manual install (only if you cannot run the installer)

The installer is the supported path — these templates exist so it has a readable,
reviewable source, not to encourage a hand-rolled variant that skips the probe. If you
must do it by hand, do all of it, in this order:

```bash
# 0. Variables (must match what you will substitute below).
repo="/Volumes/Hard Disk/BlitzkriegBot"     # BK_REPO_ROOT
dest="/Volumes/Hard Disk/BlitzkriegBotBackup"
logdir="$HOME/Library/Logs"
mkdir -p "$dest/light" "$dest/full" "$logdir" \
         "$HOME/Library/LaunchAgents" "$HOME/Library/Application Support/blitzkrieg"

# 1. Render the launcher (⚠ without step 4 below, it can only refuse — loudly).
sed -e "s|__REPO_ROOT__|$repo|g" -e "s|__BACKUP_DIR__|$dest|g" \
  "$repo/scripts/templates/data-backup-launch.sh" \
  > "$HOME/Library/Application Support/blitzkrieg/data-backup-launch.sh"
chmod 755 "$HOME/Library/Application Support/blitzkrieg/data-backup-launch.sh"
sh -n "$HOME/Library/Application Support/blitzkrieg/data-backup-launch.sh"
grep -n '__[A-Z_]*__' "$HOME/Library/Application Support/blitzkrieg/data-backup-launch.sh"   # must print nothing

# 2. Render both plists.
launcher="$HOME/Library/Application Support/blitzkrieg/data-backup-launch.sh"
for tier in light full; do
  label="com.blitzkrieg.databackup.$tier"
  sed -e "s|__LABEL__|$label|g" -e "s|__TIER__|$tier|g" -e "s|__LAUNCHER__|$launcher|g" \
      -e "s|__REPO_ROOT__|$repo|g" -e "s|__BACKUP_DIR__|$dest|g" -e "s|__LOG_DIR__|$logdir|g" \
      "$repo/scripts/templates/$label.plist" > "$HOME/Library/LaunchAgents/$label.plist"
  plutil -lint "$HOME/Library/LaunchAgents/$label.plist"    # must say OK
done

# 3. Load them.
for tier in light full; do
  label="com.blitzkrieg.databackup.$tier"
  launchctl bootout "gui/$(id -u)/$label" 2>/dev/null          # replace a stale job cleanly
  launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$label.plist"
done

# 4. What is left is yours: Full Disk Access for /bin/sh.
#    System Settings → Privacy & Security → Full Disk Access → + → ⌘⇧G → /bin/sh → switch ON.

# 5. Prove it, do not assume it. Kickstart one light run and read the verdict.
launchctl kickstart -k "gui/$(id -u)/com.blitzkrieg.databackup.light"
sleep 60
blitzkrieg backup --status        # exit 0 and `ok` — anything else is still broken
```

Two substitutions are textual, so a path containing `|` (or `&`/`<` for the plists)
would corrupt the output — hence the `plutil -lint` / `sh -n` / leftover-placeholder
checks above. If a path contains any of those, use the installer instead.

The `FAIL:` line goes to `$logdir/blitzkrieg-data-backup-<tier>.log`, and the attempt
record to `$logdir/<tier>.status` (the `BK_BACKUP_STATUS_DIR` the plists set). Both are
on the internal disk, so both survive the denial they describe.

## The check that makes a forgotten restart loud

```bash
blitzkrieg backup --status      # one line; exit 1 = a tier is stale or its scheduler is failing
```

Under launchd this can also be checked automatically: `scripts/stack-watchdog.sh`
alerts (exit `4`, and a `BACKUP_STALE` marker) when the last successful backup is older
than 26 h (`light`) / 192 h (`full`). **That check only runs if the watchdog agent is
actually loaded** — `com.blitzkrieg.stack-watchdog.plist` is checked in but not
installed on this machine, and the same TCC question applies to it, so verify it rather
than assuming it (the watchdog's own header documents its route). It is the piece that
stops a refused schedule from sitting quiet until someone happens to read a log.
