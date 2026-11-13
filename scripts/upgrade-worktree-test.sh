#!/bin/sh
# upgrade-worktree-test.sh — exercise scripts/lib/upgrade-worktree.sh.
#
# The build worktree is the only tree a release is compiled from, and it lives
# under `target/` — so it is routinely deleted by exactly the things that are
# supposed to be safe (disk reclamation, `cargo clean`, `git worktree prune`).
# The upgrade used to die on it at step 1 with git's own "cannot change to …"
# after the operator had already asked for a deploy, so the bootstrap is now a
# library of its own, with this regression net.
#
# What it asserts:
#   1. a MISSING worktree is created, detached at the requested commit, and is a
#      worktree of the main checkout (not a clone, not a branch);
#   2. a healthy worktree is REUSED untouched — a second call must not throw away
#      the cargo/npm cache it exists to hold;
#   3. a path that is present but is not a worktree (a plain dir, or a worktree
#      whose admin entry was pruned while the directory survived) is recreated;
#   4. a git checkout of ANOTHER repository parked at the build path is refused
#      — building a release from a foreign tree is the one thing this must never
#      do silently (reverse-acceptance: dropping the --git-common-dir check
#      turns this red);
#   5. a path outside the main checkout is refused, so the `rm -rf` can never
#      reach anything the checkout does not own.
#
# No network, no cargo, no npm: every case is a throwaway repo under a tmpdir.
# Run it from anywhere: `sh scripts/upgrade-worktree-test.sh` (exit 0 = pass).

set -eu

here=$(cd "$(dirname "$0")" && pwd)
lib="$here/lib/upgrade-worktree.sh"
[ -f "$lib" ] || { echo "missing $lib" >&2; exit 2; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bk-upgrade-worktree-XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

pass=0
fail=0
ok() { printf '  ok   %s\n' "$1"; pass=$((pass + 1)); }
no() {
  printf '  FAIL %s\n         expected: %s\n         actual:   %s\n' "$1" "$2" "$3" >&2
  fail=$((fail + 1))
}
check() { if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "$2" "$3"; fi; }

# The library's one contract with its caller: `ua_die <text>`. Defined before the
# source so the library is driven the way upgrade.sh drives it.
ua_die() { printf 'FATAL: %s\n' "$1" >&2; exit 90; }
. "$lib"

# A throwaway repo with one commit: no remote, no branch to protect, nothing to
# fetch. `git init -q` under -c so no runner's global config can change the shape.
git_quiet() { git -c init.defaultBranch=main -c advice.detachedHead=false "$@"; }
repo="$tmp/repo"
git_quiet init -q "$repo"
git -C "$repo" -c user.email=test@example.invalid -c user.name=test \
  commit -q --allow-empty -m "one"
sha=$(git -C "$repo" rev-parse HEAD)

# expect_die <name> <expected substring> <command...> — the command must fail and
# say why. Runs in a subshell: ua_die exits, and `set -e` must not take the whole
# test down with it.
expect_die() {
  name=$1; want=$2; shift 2
  if out=$("$@" 2>&1); then
    no "$name" "a fatal error containing: $want" "exit 0, output: $out"
  else
    case "$out" in
      *"$want"*) ok "$name" ;;
      *) no "$name" "a fatal error containing: $want" "$out" ;;
    esac
  fi
}

# ── 1. a missing build worktree is created ──────────────────────────────────
wt="$repo/target/bk-main-build"
out=$(ua_wt_ensure "$repo" "$wt" "$sha")
check "created: the worktree directory exists" "yes" "$([ -d "$wt" ] && echo yes)"
check "created: it is detached at the requested commit" "$sha" "$(git -C "$wt" rev-parse HEAD)"
check "created: HEAD is detached (no branch checked out)" "no" \
  "$(git -C "$wt" symbolic-ref -q HEAD >/dev/null 2>&1 && echo yes || echo no)"
# The build worktree must be a worktree OF $repo, not a clone of it: same
# physical git-common-dir. Resolved from each checkout's own toplevel, because
# `--git-common-dir` is a RELATIVE `.git` for a main checkout (resolving it from
# this script's cwd would name whatever checkout the caller stands in).
common_dir() {
  ( cd "$(git -C "$1" rev-parse --show-toplevel)" && cd "$(git rev-parse --git-common-dir)" && pwd -P )
}
check "created: it belongs to the main checkout" "$(common_dir "$repo")" "$(common_dir "$wt")"
check "created: the main checkout's own worktree list names it" "yes" \
  "$(git -C "$repo" worktree list --porcelain | grep -qF "$(cd "$wt" && pwd -P)" && echo yes)"
case "$out" in
  *"created, detached"*) ok "created: it says so on stdout" ;;
  *) no "created: it says so on stdout" "a 'build worktree: … (created, detached at …)' line" "$out" ;;
esac

# ── 2. a healthy worktree is reused, cache intact ────────────────────────────
marker="$wt/target/release/keep-me"
mkdir -p "$(dirname "$marker")"
printf 'cargo cache\n' > "$marker"
count_before=$(git -C "$repo" worktree list --porcelain | grep -c '^worktree ')
out=$(ua_wt_ensure "$repo" "$wt" "$sha")
check "reuse: the cargo cache survives the call" "yes" "$([ -f "$marker" ] && echo yes)"
check "reuse: it still resolves to the same commit" "$sha" "$(git -C "$wt" rev-parse HEAD)"
check "reuse: no second worktree was registered" "$count_before" \
  "$(git -C "$repo" worktree list --porcelain | grep -c '^worktree ')"
check "reuse: it reports nothing on stdout" "" "$out"

# ── 3. a broken build path is recreated ──────────────────────────────────────
# The plain dir is the sharp case: it sits INSIDE the main work tree, so git
# resolves it to the main checkout. A bootstrap that trusts `--git-dir` alone
# would call it healthy and let step 1's `checkout --detach` run against the MAIN
# work tree — so the main checkout's HEAD and branch are pinned across the call.
main_head=$(git -C "$repo" rev-parse HEAD)
main_branch=$(git -C "$repo" symbolic-ref -q HEAD || echo "(detached)")
rm -rf "$wt"
mkdir -p "$wt"
printf 'stale residue\n' > "$wt/stale.txt"
ua_wt_ensure "$repo" "$wt" "$sha" >/dev/null
check "recreate: a plain dir left at the build path is replaced by a worktree" "no" \
  "$([ -f "$wt/stale.txt" ] && echo yes || echo no)"
check "recreate: and it is detached at the requested commit" "$sha" "$(git -C "$wt" rev-parse HEAD)"
check "recreate: the main checkout's HEAD did not move" "$main_head" "$(git -C "$repo" rev-parse HEAD)"
check "recreate: the main checkout is still on its branch" "$main_branch" \
  "$(git -C "$repo" symbolic-ref -q HEAD || echo "(detached)")"

# A dangling symlink at the build path: `-e` is false for it, but `worktree add`
# still cannot own the path, so it is residue like any other.
rm -rf "$wt"
ln -s "$tmp/does-not-exist" "$wt"
ua_wt_ensure "$repo" "$wt" "$sha" >/dev/null
check "recreate: a dangling symlink at the build path is replaced by a worktree" "yes" \
  "$([ -d "$wt" ] && [ ! -L "$wt" ] && [ "$(git -C "$wt" rev-parse HEAD)" = "$sha" ] && echo yes)"

# The prune arm: an admin entry whose directory is gone makes `worktree add`
# refuse ("missing but already registered"), which is what ua_wt_ensure prunes.
orphan="$repo/target/orphan-wt"
git_quiet -C "$repo" worktree add --detach "$orphan" "$sha" >/dev/null 2>&1
rm -rf "$orphan"
ua_wt_ensure "$repo" "$orphan" "$sha" >/dev/null
check "recreate: an admin entry with no directory is pruned and re-added" "yes" \
  "$([ -d "$orphan" ] && [ "$(git -C "$orphan" rev-parse HEAD)" = "$sha" ] && echo yes)"

# ── 4. a foreign repository at the build path is refused ─────────────────────
foreign="$repo/target/foreign"
git_quiet init -q "$foreign"
git -C "$foreign" -c user.email=test@example.invalid -c user.name=test \
  commit -q --allow-empty -m "foreign"
expect_die "refuse: a checkout of another repository" "not of $repo" \
  ua_wt_ensure "$repo" "$foreign" "$sha"
check "refuse: the foreign tree is left in place" "yes" \
  "$([ -f "$foreign/.git/HEAD" ] || [ -f "$foreign/.git" ] && echo yes)"

# ── 5. a path outside the checkout is refused ────────────────────────────────
outside="$tmp/outside-build"
expect_die "refuse: a path outside the main checkout" "refusing to remove" \
  ua_wt_ensure "$repo" "$outside" "$sha"
check "refuse: nothing was created outside the checkout" "no" \
  "$([ -e "$outside" ] && echo yes || echo no)"

printf '\n%s\n' "upgrade-worktree: $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1