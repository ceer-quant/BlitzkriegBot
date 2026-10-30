#!/bin/sh
# upgrade-artifacts.sh — the non-binary half of a release (#252, #244).
#
# The upgrade builds in `target/bk-main-build` but the kernel RUNS from the main
# checkout, and it reads more from that checkout than the four binaries: its
# config files, the strategy cdylibs it `dlopen`s, and the panel's webui bundle.
# None of those used to travel with a release, so an upgrade shipped new code and
# left the old config / old strategies / old panel in place — the three ways a
# deploy could look green and behave like the previous build.
#
# This file stages all of them, reports what differs, installs them with a
# rollback copy, verifies what landed, and can undo itself. It also owns the one
# pre-build verdict the panel needs (`ua_node_modules_stale`: has the webui's
# node_modules fallen behind its lockfile?). It is SOURCED by
# `scripts/upgrade.sh`, never executed on its own, and it deliberately knows
# nothing about cargo, npm or git: every function works on directories the caller
# names, which is what lets `scripts/upgrade-propagate-test.sh` exercise the whole
# install/rollback path with no build and no network. (The panel-probe helpers
# at the bottom are the one exception that touches the network — the propagate
# test drives them with a local throwaway listener instead.)
#
# Contract — the caller exports, all paths absolute:
#   REPO_ROOT   the running checkout (what the kernel reads)
#   BUILD_WT    the production build worktree (where the build wrote)
#   STAGE       this run's staged artifacts (created by ua_stage)
#   ROLLBACK    the previous versions, for ua_rollback (created by ua_install)
#   ARTIFACTS   space-separated binary names in <REPO_ROOT>/target/release
#   DYLIB_DIRS  space-separated checkout-relative trees holding *.dylib/*.so
#   CONFIG_DIR  checkout-relative config directory (*.toml)
#   DIST_DIR    checkout-relative webui bundle directory
# and provides one reporter: `ua_die <text>` (fatal, never returns).
#
# STAGE layout: bin/<name>, dylibs/<rel_dir>/<name>, dylibs.list, dist/,
# configs/ (only when a caller stages one — ua_stage itself ships no toml).

# Legacy config files the kernel read before the `.env` single source (issue
# 751). On an UNMIGRATED deployment these are OPERATOR STATE: the account
# loader still falls back to accounts.toml when ACCOUNT_LIST is absent, so an
# upgrade must never replace them with factory copies (#758). On a MIGRATED
# deployment the same files are dead residue: `.env` is the truth and every
# runtime write face refuses while a legacy toml exists, so an upgrade must
# never (re)install them either — both directions are enforced below.
UA_LEGACY_CONFIGS="accounts.toml shadow_evolution.toml execution_policy.toml update.toml default.toml"
# Which legacy files actually carry operator state while the deployment is
# unmigrated. default.toml is documentation-only and is NOT state.
UA_OPERATOR_STATE_CONFIGS="accounts.toml shadow_evolution.toml execution_policy.toml update.toml"

# The last assignment of one key in $REPO_ROOT/.env, one optional matching
# quote layer stripped (env_file.rs / the launcher's parser semantics). Prints
# nothing when the key is absent.
ua_env_value() {
  key="$1"
  [ -f "$REPO_ROOT/.env" ] || return 0
  v=$(sed -n "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*//p" "$REPO_ROOT/.env" | tail -n 1)
  case "$v" in
    \"*\") v=${v#\"}; v=${v%\"} ;;
    \'*\') v=${v#\'}; v=${v%\'} ;;
  esac
  printf '%s\n' "$v"
}

# Migration confirmation, per config family (#758: "does the target actually
# carry the migrated keys?" — never "the new release supports env"). ACCOUNT_LIST
# is the account loader's switch (absent => it falls back to accounts.toml);
# the other families use their own env keys.
ua_account_config_migrated() {
  [ -n "$(ua_env_value ACCOUNT_LIST)" ]
}
ua_shadow_config_migrated() {
  [ -n "$(ua_env_value SHADOW_ENABLED)" ] || [ -n "$(ua_env_value SHADOW_AUTO_EVOLVE)" ]
}
ua_policy_config_migrated() {
  [ -f "$REPO_ROOT/.env" ] &&
    grep -qE '^[[:space:]]*POLICY_[A-Za-z0-9_]+[[:space:]]*=' "$REPO_ROOT/.env"
}
ua_update_config_migrated() {
  [ -f "$REPO_ROOT/.env" ] &&
    grep -qE '^[[:space:]]*BLITZKRIEG_UPDATE_[A-Za-z0-9_]+[[:space:]]*=' "$REPO_ROOT/.env"
}
ua_deployment_is_migrated() {
  ua_account_config_migrated || ua_shadow_config_migrated ||
    ua_policy_config_migrated || ua_update_config_migrated
}

# Is this shipped config operator state? Only while its OWN family has not
# been confirmed migrated. A migrated family's legacy file is residue, so the
# keep rule deliberately does not fire (replacement is allowed by the guard;
# whether it is actually installed is a separate question, see
# ua_config_is_retired_legacy).
ua_config_is_operator_state() {
  case "$1" in
    accounts.toml) ua_account_config_migrated && return 1 ;;
    shadow_evolution.toml) ua_shadow_config_migrated && return 1 ;;
    execution_policy.toml) ua_policy_config_migrated && return 1 ;;
    update.toml) ua_update_config_migrated && return 1 ;;
    default.toml) return 1 ;;
  esac
  for known in $UA_OPERATOR_STATE_CONFIGS; do
    [ "$1" = "$known" ] && return 0
  done
  return 1
}

# A legacy toml whose family IS already migrated. `.env` is the source of
# truth and the kernel's write faces refuse while the file exists, so the
# artifact step must never put it back (#758's "已迁移部署也需避免重新安装
# legacy TOML").
ua_config_is_retired_legacy() {
  for known in $UA_LEGACY_CONFIGS; do
    [ "$1" = "$known" ] || continue
    case "$1" in
      accounts.toml) ua_account_config_migrated ;;
      shadow_evolution.toml) ua_shadow_config_migrated ;;
      execution_policy.toml) ua_policy_config_migrated ;;
      update.toml) ua_update_config_migrated ;;
      default.toml) ua_deployment_is_migrated ;;
      *) return 1 ;;
    esac
    return $?
  done
  return 1
}

# Stage everything the release carries. Fails loudly on an empty set: "the build
# produced nothing here" must never be read as "nothing to ship".
ua_stage() {
  mkdir -p "$STAGE/bin" "$STAGE/dist"

  for f in $ARTIFACTS; do
    if [ ! -f "$BUILD_WT/target/release/$f" ]; then
      ua_die "the build produced no $f at $BUILD_WT/target/release/$f"
    fi
    cp "$BUILD_WT/target/release/$f" "$STAGE/bin/$f"
  done

  # Strategy cdylibs. The kernel scans its approved roots recursively for
  # *.dylib/*.so, so these are code, not build residue: a stale cdylib means the
  # running kernel executes a previous build.
  : > "$STAGE/dylibs.list"
  for d in $DYLIB_DIRS; do
    for src in "$BUILD_WT/$d"/*.dylib "$BUILD_WT/$d"/*.so; do
      [ -f "$src" ] || continue
      name=$(basename "$src")
      mkdir -p "$STAGE/dylibs/$d"
      cp "$src" "$STAGE/dylibs/$d/$name"
      printf '%s %s\n' "$d" "$name" >> "$STAGE/dylibs.list"
    done
  done
  [ -s "$STAGE/dylibs.list" ] || ua_die "no strategy cdylib was built under: $DYLIB_DIRS"

  if [ ! -d "$BUILD_WT/$DIST_DIR" ]; then
    ua_die "the panel build produced no $DIST_DIR"
  fi
  cp -a "$BUILD_WT/$DIST_DIR/." "$STAGE/dist/"

  # No factory toml is carried: `.env` is the sole configuration source (issue
  # 751), so shipping $CONFIG_DIR/*.toml would only resurrect stale factory
  # values over the operator's live state on upgrade.

  ( cd "$STAGE" && find . -type f ! -name SHA256SUMS -print0 | xargs -0 shasum -a 256 > SHA256SUMS )
}

# What the running checkout would gain — printed for both `--check` and a real
# upgrade, because "the deploy is green" and "the deploy changed something" are
# different questions and only the second one explains behaviour that did not move.
ua_drift() {
  # Config files are NOT staged by ua_stage any more (#763): `.env` is the
  # sole source, so shipping a factory toml could only resurrect stale values.
  # A caller that DOES stage a configs/ dir (an older release bundle, or the
  # #758 preserve/replace acceptance test) still gets an honest report — and
  # the migration-aware keep/retire verdicts that protect operator state.
  if [ -d "$STAGE/configs" ]; then
    printf '  %s:\n' "$CONFIG_DIR"
    for f in "$STAGE/configs"/*.toml; do
      [ -f "$f" ] || continue
      name=$(basename "$f")
      live="$REPO_ROOT/$CONFIG_DIR/$name"
      if ua_config_is_operator_state "$name" && [ -f "$live" ]; then
        # Unmigrated operator state: the running copy wins, the shipped copy
        # is never installed over it.
        printf '    = %s (operator state kept — migration not confirmed)\n' "$name"
      elif ua_config_is_retired_legacy "$name"; then
        printf '    - %s (legacy retired — .env is the source; not reinstalled)\n' "$name"
      elif [ ! -f "$live" ]; then
        printf '    + %s (new)\n' "$name"
      elif cmp -s "$f" "$live"; then
        printf '    = %s\n' "$name"
      else
        printf '    ~ %s (the shipped value differs; the running copy is backed up and replaced)\n' "$name"
      fi
    done
  fi

  printf '  strategy cdylibs:\n'
  while read -r d name; do
    live="$REPO_ROOT/$d/$name"
    if [ ! -f "$live" ]; then
      printf '    + %s (new)\n' "$name"
    elif cmp -s "$STAGE/dylibs/$d/$name" "$live"; then
      printf '    = %s\n' "$name"
    else
      printf '    ~ %s (rebuilt — the kernel has been running the previous build)\n' "$name"
    fi
  done < "$STAGE/dylibs.list"

  printf '  %s:\n' "$DIST_DIR"
  if [ ! -d "$REPO_ROOT/$DIST_DIR" ]; then
    printf '    + the running checkout has no bundle\n'
  elif cmp -s "$STAGE/dist/index.html" "$REPO_ROOT/$DIST_DIR/index.html"; then
    printf '    = the bundle matches\n'
  else
    printf '    ~ the bundle differs from the running one\n'
  fi
}

# `rm -rf` is the one irreversible step in here, so it never sees a bare variable:
# the path must be inside the checkout it belongs to, or the caller hears about it.
ua_rm_tree() {
  case "$1" in
    "$REPO_ROOT"/?*) rm -rf "$1" ;;
    *) ua_die "refusing to remove '$1': not inside $REPO_ROOT" ;;
  esac
}

# Install the staged set, keeping every replaced file under ROLLBACK first.
# Callers stop the stack before this and verify after it.
ua_install() {
  mkdir -p "$ROLLBACK/bin" "$ROLLBACK/dylibs"
  # Configs this install deliberately did NOT touch (operator state on an
  # unmigrated deployment, or a legacy toml on a migrated one). ua_verify
  # skips them; ua_rollback leaves them alone.
  : > "$ROLLBACK/configs-skipped.list"
  # Files the running checkout did NOT have before this install. Rollback restores
  # what it replaced; these are the ones it has to take away again, and recording
  # them here is what keeps that list exact instead of guessed.
  : > "$ROLLBACK/added.list"

  for f in $ARTIFACTS; do
    if [ -f "$REPO_ROOT/target/release/$f" ]; then
      cp -a "$REPO_ROOT/target/release/$f" "$ROLLBACK/bin/$f"
    fi
    cp "$STAGE/bin/$f" "$REPO_ROOT/target/release/$f.new"
    mv "$REPO_ROOT/target/release/$f.new" "$REPO_ROOT/target/release/$f"
  done

  while read -r d name; do
    live="$REPO_ROOT/$d/$name"
    if [ -f "$live" ]; then
      cp -a "$live" "$ROLLBACK/dylibs/$name"
    else
      printf '%s\n' "$d/$name" >> "$ROLLBACK/added.list"
    fi
    mkdir -p "$REPO_ROOT/$d"
    cp "$STAGE/dylibs/$d/$name" "$live.new"
    mv "$live.new" "$live"
  done < "$STAGE/dylibs.list"

  if [ -d "$REPO_ROOT/$DIST_DIR" ]; then
    cp -a "$REPO_ROOT/$DIST_DIR" "$ROLLBACK/dist"
  fi
  ua_rm_tree "$REPO_ROOT/$DIST_DIR"
  mkdir -p "$REPO_ROOT/$DIST_DIR"
  cp -a "$STAGE/dist/." "$REPO_ROOT/$DIST_DIR/"

  # No factory toml is carried or installed by ua_stage (see ua_stage): `.env`
  # is the sole configuration source, so a normal upgrade never touches
  # $CONFIG_DIR. If a caller DOES stage configs/ (an older bundle, or the #758
  # test), the migration-aware rules below still apply:
  #   * unmigrated + live file   -> preserve the operator copy (the account
  #                                loader would otherwise fall back to factory
  #                                values after the upgrade);
  #   * migrated legacy toml     -> never reinstall it (the kernel's write
  #                                faces refuse while it exists; `.env` is the
  #                                truth now);
  #   * everything else          -> normal install with rollback support.
  if [ -d "$STAGE/configs" ]; then
    for f in "$STAGE/configs"/*.toml; do
      [ -f "$f" ] || continue
      name=$(basename "$f")
      live="$REPO_ROOT/$CONFIG_DIR/$name"
      if ua_config_is_operator_state "$name" && [ -f "$live" ]; then
        printf '  = %s (operator state kept — migration not confirmed)\n' "$name"
        printf '%s\n' "$name" >> "$ROLLBACK/configs-skipped.list"
        continue
      fi
      if ua_config_is_retired_legacy "$name"; then
        printf '  - %s (legacy retired — .env is the source; not reinstalled)\n' "$name"
        printf '%s\n' "$name" >> "$ROLLBACK/configs-skipped.list"
        continue
      fi
      mkdir -p "$REPO_ROOT/$CONFIG_DIR"
      if [ -f "$live" ]; then
        mkdir -p "$ROLLBACK/configs"
        cp -a "$live" "$ROLLBACK/configs/$name"
      else
        printf '%s\n' "$CONFIG_DIR/$name" >> "$ROLLBACK/added.list"
      fi
      cp "$f" "$live.new"
      mv "$live.new" "$live"
    done
  fi
}

# Byte-for-byte comparison of what landed against what was staged. A `cp` that
# ran out of disk is the failure this catches, and it must be caught BEFORE the
# stack is restarted on the half-written set.
ua_verify() {
  for f in $ARTIFACTS; do
    want=$(shasum -a 256 "$STAGE/bin/$f" | awk '{print $1}')
    got=$(shasum -a 256 "$REPO_ROOT/target/release/$f" | awk '{print $1}')
    [ "$want" = "$got" ] || ua_die "$f did not land intact in target/release"
  done

  while read -r d name; do
    cmp -s "$STAGE/dylibs/$d/$name" "$REPO_ROOT/$d/$name" ||
      ua_die "$d/$name did not land intact"
  done < "$STAGE/dylibs.list"

  cmp -s "$STAGE/dist/index.html" "$REPO_ROOT/$DIST_DIR/index.html" ||
    ua_die "the panel bundle did not land intact"

  for f in "$STAGE/configs"/*.toml; do
    [ -f "$f" ] || continue
    name=$(basename "$f")
    if grep -qxF "$name" "$ROLLBACK/configs-skipped.list" 2>/dev/null; then
      continue # preserved operator state or a retired legacy toml: not installed
    fi
    cmp -s "$f" "$REPO_ROOT/$CONFIG_DIR/$name" ||
      ua_die "$name did not land intact"
  done
}

# Put the previous set back. Used only by the failure path of a real upgrade, so
# it must work with whatever part of ROLLBACK exists (a first install has no
# previous config, say) rather than assuming a complete set.
ua_rollback() {
  for f in $ARTIFACTS; do
    if [ -f "$ROLLBACK/bin/$f" ]; then
      cp -a "$ROLLBACK/bin/$f" "$REPO_ROOT/target/release/$f"
    fi
  done

  if [ -f "$STAGE/dylibs.list" ]; then
    while read -r d name; do
      if [ -f "$ROLLBACK/dylibs/$name" ]; then
        cp -a "$ROLLBACK/dylibs/$name" "$REPO_ROOT/$d/$name"
      fi
    done < "$STAGE/dylibs.list"
  fi

  if [ -d "$ROLLBACK/dist" ]; then
    ua_rm_tree "$REPO_ROOT/$DIST_DIR"
    cp -a "$ROLLBACK/dist" "$REPO_ROOT/$DIST_DIR"
  fi

  for f in "$ROLLBACK/configs"/*.toml; do
    if [ -f "$f" ]; then
      cp -a "$f" "$REPO_ROOT/$CONFIG_DIR/$(basename "$f")"
    fi
  done

  # Last, so it also covers a config or cdylib the install added: the previous
  # state did not contain these, and a rollback that leaves them behind is not a
  # rollback — the kernel would load a strategy from the build that was undone.
  if [ -f "$ROLLBACK/added.list" ]; then
    while read -r rel; do
      if [ -n "$rel" ]; then
        ua_rm_tree "$REPO_ROOT/$rel"
      fi
    done < "$ROLLBACK/added.list"
  fi
}

# Keep the three newest rollback sets. Each one holds a copy of the binaries, so
# an unbounded pile is real disk (15 MB per set) — and the point of a rollback
# point is the last one, not the archive of every upgrade ever run.
ua_prune_rollbacks() {
  ls -1dt "$REPO_ROOT"/target/rollback-* 2>/dev/null | tail -n +4 | while read -r old; do
    case "$old" in
      "$REPO_ROOT"/target/rollback-*) ua_rm_tree "$old" ;;
      *) ua_die "refusing to prune '$old': not a rollback directory" ;;
    esac
  done
}

# ── the post-start panel probe (#688) ────────────────────────────────────────
# Three helpers, each pure enough for the propagate test to drive with no
# network and no live stack.

# The address the panel actually listens on, following the same precedence the
# launcher resolves at startup (`blitzkrieg run`, see run_unified):
#   $1 = --addr if given   >   BLITZKRIEG_PANEL_ADDR (the repo .env carries it)
#   >   127.0.0.1:51888 (loopback default)
# The old probe hardcoded the loopback default. This production server binds
# 0.0.0.0:<port> via .env, so the probe shot a host:port that answers —
# verified 2026-10-28 — yet every upgrade logged `panel probe 000000`.
ua_panel_addr() {
  addr_env=""
  if [ -f "$REPO_ROOT/.env" ]; then
    # Last assignment wins: parse_env_file/set_var semantics in env_file.rs.
    addr_env=$(sed -n 's/^[[:space:]]*BLITZKRIEG_PANEL_ADDR=[[:space:]]*//p' "$REPO_ROOT/.env" | tail -n 1)
    # One optional layer of matching quotes, as the .env parser strips.
    case "$addr_env" in
      \"*\") addr_env=${addr_env#\"}; addr_env=${addr_env%\"} ;;
      \'*\') addr_env=${addr_env#\'}; addr_env=${addr_env%\'} ;;
    esac
  fi
  if [ -n "$1" ]; then
    printf '%s\n' "$1"
  elif [ -n "$addr_env" ]; then
    printf '%s\n' "$addr_env"
  else
    printf '127.0.0.1:51888\n'
  fi
}

# Normalize a listen addr to a probe URL. A wildcard host is replaced with the
# loopback literal — `curl http://0.0.0.0:51888/` connects nowhere on macOS
# (BSD sendto: sin_zero), while on Linux it would loop back and *miss* the
# 0.0.0.0-vs-LAN-IP listening problem this probe exists to surface. IPv6 is
# handled explicitly: an already-bracketed `[::1]:51888` passes through, a bare
# `::1` gets brackets — the naive `host=${addr%%:*}` split mangles both, which
# is why the shapes are separated before any splitting happens.
ua_probe_url() {
  addr="$1"
  case "$addr" in
    # Already-bracketed IPv6: the port is after the last colon; the host keeps
    # its closing bracket (`%:*` strips only the port — `%%]:*` would eat the
    # bracket too, which is exactly the mangling this branch exists to avoid).
    \[*:*)
      host="${addr%:*}"
      port="${addr##*:}"
      printf 'http://%s:%s/panel/\n' "$host" "$port"
      ;;
    # All-loopback IPv6, bare — the common LAN shorthand.
    "::1")
      printf 'http://[::1]:51888/panel/\n'
      ;;
    # Any other bare IPv6 (colons beyond the port separator): bracket all but
    # the last colon; an empty/unspecified host (`::`) probes the loopback.
    *:*:*|::)
      host="[$(printf '%s' "$addr" | sed 's/:[^:]*$//')]"; port=${addr##*:}
      case "$host" in "[::]"|"[]") host="127.0.0.1" ;; esac
      printf 'http://%s:%s/panel/\n' "$host" "$port"
      ;;
    # IPv4 / hostname[:port] — the everyday shapes.
    *)
      host=${addr%%:*}; port=${addr#*:}
      [ "$host" = "0.0.0.0" ] && host="127.0.0.1"
      printf 'http://%s:%s/panel/\n' "$host" "$port"
      ;;
  esac
}

# The probe itself: `$1` = full URL, `$2` = attempts, `$3` = per-attempt curl
# timeout in seconds. Retries, because the launcher spawns the core and the web
# thread concurrently — after .core-lock carries the new identity the bind can
# still be microseconds away. Prints the final HTTP code (000 = could not
# connect; curl already said it once, so no `|| echo` doubling).
ua_panel_probe() {
  url="$1"; attempts="$2"; timeout="$3"
  code=000
  n=0
  while [ "$n" -lt "$attempts" ]; do
    # `|| true` keeps the failure status off the assignment: under the caller's
    # `set -e` a connection refusal (curl exit 7) would otherwise abort the
    # function before the final printf, and the probe would report NOTHING.
    code=$(curl -4 -s -o /dev/null -w '%{http_code}' --connect-timeout "$timeout" "$url" 2>/dev/null || true)
    [ -n "$code" ] || code=000
    n=$((n + 1))
    [ "$code" = "000" ] || break
    [ "$n" -lt "$attempts" ] && sleep 2
  done
  printf '%s\n' "$code"
}

# Is the webui's node_modules behind its lockfile? A pure file-mtime verdict —
# this library still knows nothing about npm. The bare `[ -d node_modules ]`
# this replaces skipped the install forever after the first one, so a dependency
# ADDED to package.json never landed and the build died on the import
# (2026-10-06: Rollup could not resolve @vue-flow/core).
#   * no node_modules                  -> stale (nothing installed yet)
#   * package-lock.json NEWER than it  -> stale (deps changed since the install;
#                                         npm ci recreates the tree, so a fresh
#                                         install always carries the newer mtime,
#                                         and a checkout/merge that updates the
#                                         lockfile is exactly the signal wanted)
#   * anything else (no lockfile ...)  -> NOT stale: the build itself fails loudly
#                                         on a broken tree, and that error is the
#                                         right one — this verdict exists only to
#                                         catch the silent-skip case.
# Returns 0 = stale, 1 = fresh. $1 = the webui root.
ua_node_modules_stale() {
  webui="$1"
  [ -d "$webui/node_modules" ] || return 0
  [ -f "$webui/package-lock.json" ] && [ "$webui/package-lock.json" -nt "$webui/node_modules" ] && return 0
  return 1
}
