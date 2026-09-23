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
# rollback copy, verifies what landed, and can undo itself. It is SOURCED by
# `scripts/upgrade.sh`, never executed on its own, and it deliberately knows
# nothing about cargo, npm or git: every function works on directories the caller
# names, which is what lets `scripts/upgrade-propagate-test.sh` exercise the whole
# install/rollback path with no build and no network.
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
# STAGE layout: bin/<name>, dylibs/<rel_dir>/<name>, dylibs.list, dist/, configs/

# Stage everything the release carries. Fails loudly on an empty set: "the build
# produced nothing here" must never be read as "nothing to ship".
ua_stage() {
  mkdir -p "$STAGE/bin" "$STAGE/dist" "$STAGE/configs"

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

  for f in "$BUILD_WT/$CONFIG_DIR"/*.toml; do
    [ -f "$f" ] || continue
    cp "$f" "$STAGE/configs/$(basename "$f")"
  done
  ls "$STAGE/configs" | grep -q . || ua_die "the source has no $CONFIG_DIR/*.toml to ship"

  ( cd "$STAGE" && find . -type f ! -name SHA256SUMS -print0 | xargs -0 shasum -a 256 > SHA256SUMS )
}

# What the running checkout would gain — printed for both `--check` and a real
# upgrade, because "the deploy is green" and "the deploy changed something" are
# different questions and only the second one explains behaviour that did not move.
ua_drift() {
  printf '  %s:\n' "$CONFIG_DIR"
  for f in "$STAGE/configs"/*.toml; do
    [ -f "$f" ] || continue
    name=$(basename "$f")
    live="$REPO_ROOT/$CONFIG_DIR/$name"
    if [ ! -f "$live" ]; then
      printf '    + %s (new)\n' "$name"
    elif cmp -s "$f" "$live"; then
      printf '    = %s\n' "$name"
    else
      printf '    ~ %s (the shipped value differs; the running copy is backed up and replaced)\n' "$name"
    fi
  done

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
  mkdir -p "$ROLLBACK/bin" "$ROLLBACK/configs" "$ROLLBACK/dylibs"
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

  for f in "$STAGE/configs"/*.toml; do
    [ -f "$f" ] || continue
    name=$(basename "$f")
    live="$REPO_ROOT/$CONFIG_DIR/$name"
    if [ -f "$live" ]; then
      cp -a "$live" "$ROLLBACK/configs/$name"
    else
      printf '%s\n' "$CONFIG_DIR/$name" >> "$ROLLBACK/added.list"
    fi
    cp "$f" "$live.new"
    mv "$live.new" "$live"
  done
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
    cmp -s "$f" "$REPO_ROOT/$CONFIG_DIR/$(basename "$f")" ||
      ua_die "$(basename "$f") did not land intact"
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
