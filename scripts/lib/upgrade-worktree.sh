#!/bin/sh
# upgrade-worktree.sh — the build worktree's bootstrap (step 1 of an upgrade).
#
# The upgrade builds in `target/bk-main-build` and the kernel RUNS from the main
# checkout, so this worktree is the only tree a release is ever compiled from —
# and it lives under `target/`, which is exactly where disk reclamation, a
# `cargo clean`, a `rm -rf target/*` or a `git worktree prune` reach. When it is
# gone the old step 1 (`git -C target/bk-main-build fetch ceer`) died with
# git's "cannot change to …: No such file or directory" — after the operator had
# already asked for a deploy, with nothing built and nothing diagnosed.
#
# So the rule here is the same one the upgrade has everywhere else: a missing
# build tree is not a red tree, it is a tree to put back. `ua_wt_ensure` makes
# the worktree exist and detached at the source commit, whatever state it is in:
#   * absent                       -> created (a COLD build: the cargo and npm
#                                     caches it held are gone with it; the next
#                                     upgrade after that is fast again);
#   * a registered worktree of the MAIN checkout -> reused untouched;
#   * present but not a worktree of this repo (a plain dir left behind, a .git
#     file whose admin entry was pruned) -> recreated;
#   * a git checkout of ANOTHER repository -> fatal. Building a release out of a
#     foreign tree is the one thing this must never do silently: the deploy would
#     ship code nobody reviewed in this checkout.
#
# It deliberately knows nothing about remotes, cargo or npm (this library never
# fetches, never builds), which is what lets `upgrade-worktree-test.sh` drive
# every branch against throwaway repos — no network, no release build.
#
# Contract — the caller passes, absolute paths:
#   $1 = REPO_ROOT   the running checkout (what the kernel reads)
#   $2 = BUILD_WT    the production build worktree this call must leave behind
#   $3 = SHA         the commit-ish to check out detached (any git rev)
# and provides one reporter: `ua_die <text>` (fatal, never returns).

# The PHYSICAL path of a checkout's git-common-dir, or nothing if $1 is not a
# usable checkout. Two details make this non-obvious:
#   * `--git-common-dir` answers `.git` (RELATIVE) for a main checkout and an
#     absolute path for a linked worktree, so it has to be resolved from that
#     checkout's own toplevel — resolving it from the caller's cwd silently lands
#     in whatever checkout the caller happens to stand in;
#   * `pwd -P`, so a repo reached through a symlinked path (/tmp → /private/tmp)
#     compares equal to the same repo reached physically. Without it every macOS
#     tmpdir fixture would look like a foreign repository.
ua_wt_common_dir() {
  _wt_top=$(git -C "$1" rev-parse --show-toplevel 2>/dev/null) || return 1
  ( cd "$_wt_top" && cd "$(git rev-parse --git-common-dir)" && pwd -P )
}

# Create (or recreate) BUILD_WT as a worktree of REPO_ROOT, detached at SHA.
# Returns 0 when the worktree is usable afterwards; dies on anything it refuses
# to guess about. Idempotent: a second call on a healthy worktree is a no-op.
ua_wt_ensure() {
  repo=$1; wt=$2; sha=$3

  [ -n "$repo" ] && [ -n "$wt" ] && [ -n "$sha" ] ||
    ua_die "ua_wt_ensure needs <repo_root> <build_wt> <sha>"

  # The one irreversible step below is an `rm -rf`, so it never sees a path that
  # is not provably inside the checkout it belongs to (same guard as ua_rm_tree:
  # a build cache is disposable, the checkout is not). The path is always
  # expanded in double quotes and never re-parsed by a shell — there is no `eval`
  # and no unquoted expansion in this file — so a space in the checkout path is a
  # space in the path, not a command separator.
  case "$wt" in
    "$repo"/?*) ;;
    *) ua_die "refusing to remove '$wt': not inside $repo" ;;
  esac

  git -C "$repo" rev-parse --git-dir >/dev/null 2>&1 ||
    ua_die "$repo is not a git checkout — cannot resolve a build worktree for it"

  # Already a worktree of THIS repo: nothing to do. Two facts have to hold, and
  # the first is the non-obvious one:
  #   * $wt is the ROOT of a work tree. A plain directory that merely sits INSIDE
  #     the main work tree (a stale build dir, say) resolves through `git -C` to
  #     the main checkout — `--git-dir` alone would call it healthy, and step 1's
  #     `checkout --detach` would then run against the MAIN work tree: the one
  #     tree an upgrade promises never to touch. `--show-toplevel` is what
  #     separates the two.
  #   * that work tree belongs to $repo, not to some other repository parked at
  #     the same path.
  if [ -d "$wt" ]; then
    top=$(git -C "$wt" rev-parse --show-toplevel 2>/dev/null) || top=""
    if [ -n "$top" ] && [ "$(cd "$top" && pwd -P)" = "$(cd "$wt" && pwd -P)" ]; then
      wt_common=$(ua_wt_common_dir "$wt") || wt_common=""
      repo_common=$(ua_wt_common_dir "$repo") || repo_common=""
      [ -n "$wt_common" ] && [ "$wt_common" = "$repo_common" ] && return 0
      [ -n "$wt_common" ] ||
        ua_die "$wt is a git checkout but its repository cannot be resolved — refusing to build from it"
      ua_die "$wt is a checkout of $wt_common, not of $repo — refusing to build a release from a foreign tree"
    fi
  fi

  # Not a usable worktree of this repo. Whatever it held (a cold cargo cache at
  # best) is already lost, so removing it is the only way forward. `-L` as well as
  # `-e`: a symlink left pointing at a deleted cache is not `-e`, and it still has
  # to go before `worktree add` can own the path.
  if [ -e "$wt" ] || [ -L "$wt" ]; then
    echo "note: $wt is not a build worktree of this checkout — recreating it (the next build is a cold one)" >&2
    rm -rf "$wt"
  fi
  # A worktree whose admin entry survived but whose directory did not makes
  # `worktree add` refuse ("missing but already registered"); prune is what turns
  # that into a clean create.
  git -C "$repo" worktree prune
  git -C "$repo" worktree add --detach "$wt" "$sha" >/dev/null ||
    ua_die "could not create the build worktree $wt at $sha"

  printf 'build worktree: %s (created, detached at %s)\n' \
    "$wt" "$(git -C "$wt" rev-parse --short=12 HEAD)"
}