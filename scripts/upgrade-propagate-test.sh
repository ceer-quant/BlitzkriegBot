#!/bin/sh
# upgrade-propagate-test.sh — exercise scripts/lib/upgrade-artifacts.sh (#252).
#
# The propagation half of an upgrade is the half that used to be missing, and it
# is also the half nobody can test by running `blitzkrieg upgrade` (that needs a
# network, a release build, and a live stack). The helpers in the library work on
# directories the caller names, so this test drives the WHOLE install/verify/
# rollback path against two throwaway trees — no cargo, no npm, no network.
#
# What it asserts:
#   1. staging copies binaries, every strategy cdylib, the panel bundle and the
#      config files, and refuses to stage an empty cdylib set;
#   2. the drift report names what differs (`~`) and what does not (`=`);
#   3. installing replaces the running copies byte-for-byte;
#   4. a tampered install is caught by ua_verify before the stack is restarted;
#   5. rollback restores the previous binaries, cdylibs, bundle AND configs —
#      including a config the operator had edited locally;
#   6. the rollback pruner keeps the three newest sets and nothing else;
#   7. the webui node_modules verdict: a fresh install is kept, a lockfile
#      newer than the install is stale (the 2026-10-06 @vue-flow incident —
#      the bare `[ -d node_modules ]` it replaced calls that tree fresh), a
#      missing install is stale, and a lockfile-less tree is left to the
#      build's own loud failure;
#   8. the operator's live state is NOT in the upgrade carry face (issue 751,
#      the .env single source): every runtime write lands in `.env` (ACCOUNT_*
#      / SHADOW_* keys), so `UA_OPERATOR_STATE_CONFIGS` is empty by ruling and
#      the operator's `.env` is never staged, never drifted, never installed
#      over and never rolled back.
#
# Run it from anywhere: `sh scripts/upgrade-propagate-test.sh` (exit 0 = pass).

set -eu

here=$(cd "$(dirname "$0")" && pwd)
lib="$here/lib/upgrade-artifacts.sh"
[ -f "$lib" ] || { echo "missing $lib" >&2; exit 2; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bk-upgrade-propagate-XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

pass=0
fail=0
ok() { printf '  ok   %s\n' "$1"; pass=$((pass + 1)); }
no() {
  printf '  FAIL %s\n         expected: %s\n         actual:   %s\n' "$1" "$2" "$3" >&2
  fail=$((fail + 1))
}
check() { if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "$2" "$3"; fi; }
check_same() { # check_same <name> <expected file> <actual file>
  if cmp -s "$2" "$3"; then ok "$1"; else no "$1" "same bytes as $2" "differs from $2"; fi
}

ua_die() { printf 'ERROR: %s\n' "$1" >&2; exit 1; }

# ── two trees: what is running, and what the build produced ──────────────────
REPO_ROOT="$tmp/repo"
BUILD_WT="$tmp/build"
ARTIFACTS="blitzkrieg blitzkrieg-core libpolymarket_extension.dylib ui_kit_panel"
DYLIB_DIRS="user_layer/parity_strategy/target/release"
CONFIG_DIR="user_layer/configs"
DIST_DIR="ui/webapp/webui/dist"
STAGE="$tmp/stage"
ROLLBACK="$tmp/rollback"
export REPO_ROOT BUILD_WT ARTIFACTS DYLIB_DIRS CONFIG_DIR DIST_DIR STAGE ROLLBACK

mkdir -p "$REPO_ROOT/target/release" "$REPO_ROOT/$CONFIG_DIR" \
  "$REPO_ROOT/ui/webapp/webui/dist" "$BUILD_WT/target/release" \
  "$BUILD_WT/$CONFIG_DIR" "$BUILD_WT/ui/webapp/webui/dist"

for f in $ARTIFACTS; do
  printf 'OLD %s\n' "$f" > "$REPO_ROOT/target/release/$f"
  printf 'NEW %s\n' "$f" > "$BUILD_WT/target/release/$f"
done
# One cdylib exists on both sides (rebuilt, so it is reported and replaced) and a
# second one is new in the build and must ship too. Two names in ONE directory on
# purpose: what ships is driven by DYLIB_DIRS, not by a hard-coded set of trees.
for d in $DYLIB_DIRS; do
  mkdir -p "$REPO_ROOT/$d" "$BUILD_WT/$d"
done
printf 'OLD parity\n' > "$REPO_ROOT/user_layer/parity_strategy/target/release/libparity_strategy.dylib"
printf 'NEW parity\n' > "$BUILD_WT/user_layer/parity_strategy/target/release/libparity_strategy.dylib"
printf 'NEW fresh\n' > "$BUILD_WT/user_layer/parity_strategy/target/release/libfresh_strategy.dylib"

printf '<html>OLD</html>\n' > "$REPO_ROOT/$DIST_DIR/index.html"
printf '<html>NEW</html>\n' > "$BUILD_WT/$DIST_DIR/index.html"
printf 'assets\n' > "$BUILD_WT/$DIST_DIR/bundle.js"

printf 'round_sec = 900\n' > "$BUILD_WT/$CONFIG_DIR/default.toml"
printf 'round_sec = 900\n' > "$REPO_ROOT/$CONFIG_DIR/default.toml"
# shadow_evolution.toml and accounts.toml ship as plain factory configs since
# issue 751 (the .env single source): the evolution switches write SHADOW_* into
# `.env` and every account write face upserts ACCOUNT_* into `.env`, so NEITHER
# file is a kernel runtime write target anymore and `UA_OPERATOR_STATE_CONFIGS`
# is empty. The live copies below are locally-edited configs like any other:
# an upgrade replaces them (backing them up into ROLLBACK) and a rollback
# brings them back.
printf 'auto_evolve = true\n' > "$BUILD_WT/$CONFIG_DIR/shadow_evolution.toml"
printf 'enabled = true\nauto_evolve = false  # panel-flipped locally\n' > "$REPO_ROOT/$CONFIG_DIR/shadow_evolution.toml"
cp "$REPO_ROOT/$CONFIG_DIR/shadow_evolution.toml" "$tmp/local-shadow.toml"
printf '[[account]]\nid = "default"\nstrategies = ["spread_arb"]\n' > "$BUILD_WT/$CONFIG_DIR/accounts.toml"
printf '[[account]]\nid = "default"\nstrategies = ["flash_arb"]\n' > "$REPO_ROOT/$CONFIG_DIR/accounts.toml"
cp "$REPO_ROOT/$CONFIG_DIR/accounts.toml" "$tmp/local-accounts.toml"
cp "$REPO_ROOT/target/release/blitzkrieg" "$tmp/old-blitzkrieg"

. "$lib"

# ── 1. staging ───────────────────────────────────────────────────────────────
printf '\nstaging\n'
ua_stage
check "binaries staged" "4" "$(ls "$STAGE/bin" | wc -l | tr -d ' ')"
check "cdylibs staged (1 rebuilt + 1 new)" "2" "$(wc -l < "$STAGE/dylibs.list" | tr -d ' ')"
check "configs staged" "3" "$(ls "$STAGE/configs" | wc -l | tr -d ' ')"
check_same "bundle staged" "$BUILD_WT/$DIST_DIR/index.html" "$STAGE/dist/index.html"
check_same "nested bundle file staged" "$BUILD_WT/$DIST_DIR/bundle.js" "$STAGE/dist/bundle.js"
# 4 binaries + 2 cdylibs + dylibs.list + 3 configs + 2 bundle files.
check "checksums written" "12" "$(wc -l < "$STAGE/SHA256SUMS" | tr -d ' ')"
if ( BUILD_WT="$tmp/empty-build" ua_stage ) >/dev/null 2>&1; then
  no "an empty build is refused" "non-zero exit" "exit 0"
else
  ok "an empty build is refused"
fi

# ── 2. drift report ──────────────────────────────────────────────────────────
printf '\ndrift report\n'
ua_drift > "$tmp/drift.txt"
check "drift: unchanged config marked =" "1" "$(grep -c '= default.toml' "$tmp/drift.txt" | tr -d ' ')"
check "drift: locally-edited config marked ~ (no operator-state keep since issue 751)" "1" \
  "$(grep -c '~ shadow_evolution.toml (the shipped value differs' "$tmp/drift.txt" | tr -d ' ')"
check "drift: locally-edited config marked ~ (no operator-state keep since issue 751)" "1" \
  "$(grep -c '~ accounts.toml (the shipped value differs' "$tmp/drift.txt" | tr -d ' ')"
check "drift: no operator-state line exists at all (the list is empty by ruling)" "0" \
  "$(grep -c 'operator state kept' "$tmp/drift.txt" | tr -d ' ')"
check "drift: rebuilt cdylib marked ~" "1" "$(grep -c '~ lib.*_strategy.dylib' "$tmp/drift.txt" | tr -d ' ')"
check "drift: new cdylib marked +" "1" "$(grep -c '+ libfresh_strategy.dylib' "$tmp/drift.txt" | tr -d ' ')"

# ── 3. install + verify ──────────────────────────────────────────────────────
printf '\ninstall\n'
ua_install
ua_verify
check_same "binary replaced" "$STAGE/bin/blitzkrieg-core" "$REPO_ROOT/target/release/blitzkrieg-core"
check_same "cdylib replaced" "$STAGE/dylibs/user_layer/parity_strategy/target/release/libparity_strategy.dylib" \
  "$REPO_ROOT/user_layer/parity_strategy/target/release/libparity_strategy.dylib"
check_same "new cdylib installed" "$STAGE/dylibs/user_layer/parity_strategy/target/release/libfresh_strategy.dylib" \
  "$REPO_ROOT/user_layer/parity_strategy/target/release/libfresh_strategy.dylib"
check_same "bundle replaced" "$STAGE/dist/index.html" "$REPO_ROOT/$DIST_DIR/index.html"
check "bundle holds exactly the staged set" "2" "$(ls "$REPO_ROOT/$DIST_DIR" | wc -l | tr -d ' ')"
check_same "locally-edited config replaced by the shipped copy" "$STAGE/configs/accounts.toml" \
  "$REPO_ROOT/$CONFIG_DIR/accounts.toml"
check_same "locally-edited config replaced by the shipped copy" "$STAGE/configs/shadow_evolution.toml" \
  "$REPO_ROOT/$CONFIG_DIR/shadow_evolution.toml"
check_same "the replaced config is kept for rollback" "$tmp/local-accounts.toml" \
  "$ROLLBACK/configs/accounts.toml"
check_same "the replaced config is kept for rollback" "$tmp/local-shadow.toml" \
  "$ROLLBACK/configs/shadow_evolution.toml"
check_same "the previous binary is kept for rollback" "$tmp/old-blitzkrieg" "$ROLLBACK/bin/blitzkrieg"

# ── 4. a tampered install must not reach the restart ─────────────────────────
printf '\nverification\n'
printf 'truncated\n' > "$REPO_ROOT/target/release/blitzkrieg-core"
if ( ua_verify ) >/dev/null 2>&1; then
  no "a half-written binary is caught" "non-zero exit" "exit 0"
else
  ok "a half-written binary is caught"
fi
cp "$STAGE/bin/blitzkrieg-core" "$REPO_ROOT/target/release/blitzkrieg-core"
ua_verify

# ── 5. rollback restores everything, including the operator's config ─────────
printf '\nrollback\n'
ua_rollback
check_same "binary restored" "$tmp/old-blitzkrieg" "$REPO_ROOT/target/release/blitzkrieg"
check "cdylib restored" "OLD parity" "$(cat "$REPO_ROOT/user_layer/parity_strategy/target/release/libparity_strategy.dylib")"
check "the cdylib the install ADDED is taken away again" "0" \
  "$(ls "$REPO_ROOT/user_layer/parity_strategy/target/release" | grep -c fresh || true)"
check "bundle index restored" "<html>OLD</html>" "$(cat "$REPO_ROOT/$DIST_DIR/index.html")"
check_same "the locally-edited config is restored" "$tmp/local-shadow.toml" \
  "$REPO_ROOT/$CONFIG_DIR/shadow_evolution.toml"
check_same "the other locally-edited config is restored too" "$tmp/local-accounts.toml" \
  "$REPO_ROOT/$CONFIG_DIR/accounts.toml"

# ── 5b. a MISSING config is installed (factory seed) ─────────────────────────
# With UA_OPERATOR_STATE_CONFIGS empty there is no keep-instead-of-install
# exception left: a checkout without a staged file (first install, or the
# operator deleted it) must gain the factory copy, and a rollback of that
# install must take it away again.
printf '\ninstall with no live accounts.toml\n'
rm "$REPO_ROOT/$CONFIG_DIR/accounts.toml"
ua_install
ua_verify
check_same "missing operator-state config seeded from the factory copy" \
  "$BUILD_WT/$CONFIG_DIR/accounts.toml" "$REPO_ROOT/$CONFIG_DIR/accounts.toml"
if grep -q "^$CONFIG_DIR/accounts.toml$" "$ROLLBACK/added.list"; then
  ok "the seeded config is recorded in added.list"
else
  no "the seeded config is recorded in added.list" "listed" "missing from added.list"
fi
ua_rollback
check "the seeded config is taken away again by rollback" "0" \
  "$(ls "$REPO_ROOT/$CONFIG_DIR" | grep -c accounts.toml || true)"

# ── 5c. the operator's .env is NOT in the carry face (issue 751) ─────────────
# Every runtime write face now upserts into `.env` (ACCOUNT_* keys, SHADOW_*
# switches), so the operator's live state lives in a file the upgrade never
# stages, never drifts, never installs over and never rolls back — the exact
# guarantee UA_OPERATOR_STATE_CONFIGS used to give accounts.toml, now structural
# instead of enumerated. This pins the boundary through a full cycle, and pins
# the cleared list itself.
printf '\nthe operator .env stays out of the carry face (issue 751)\n'
printf 'ACCOUNT_LIST=default\nACCOUNT_DEFAULT_NAME=live book\nSHADOW_ENABLED=true\nBLITZKRIEG_PANEL_ADDR=0.0.0.0:51888\n' \
  > "$REPO_ROOT/.env"
cp "$REPO_ROOT/.env" "$tmp/operator-dot-env"
for t in default.toml accounts.toml shadow_evolution.toml; do
  if ua_config_is_operator_state "$t"; then
    no "no shipped config is operator state ($t)" "not in the cleared list" "matched"
  else
    ok "no shipped config is operator state ($t)"
  fi
done
ua_stage
ua_drift > "$tmp/drift-env.txt"
check "the carry face stages no .env anywhere" "0" "$(find "$STAGE" -name .env | wc -l | tr -d ' ')"
check "the drift report never mentions .env" "0" "$(grep -c '\.env' "$tmp/drift-env.txt" || true)"
ua_install
ua_verify
check_same "install leaves the operator .env byte-identical" "$tmp/operator-dot-env" "$REPO_ROOT/.env"
ua_rollback
check_same "rollback leaves the operator .env byte-identical too" "$tmp/operator-dot-env" "$REPO_ROOT/.env"
rm "$REPO_ROOT/.env"

# ── 6. the rollback pruner keeps the three newest ────────────────────────────
printf '\nrollback pruner\n'
for n in 1 2 3 4 5; do
  mkdir -p "$REPO_ROOT/target/rollback-2026010${n}T000000"
  sleep 0.05
done
mkdir -p "$REPO_ROOT/target/keepme-20260101T000000"
ua_prune_rollbacks
check "three newest rollback sets survive" "3" \
  "$(ls -1d "$REPO_ROOT"/target/rollback-* | wc -l | tr -d ' ')"
check "the survivors are the three newest" "rollback-20260103T000000 rollback-20260104T000000 rollback-20260105T000000" \
  "$(ls -1d "$REPO_ROOT"/target/rollback-* | xargs -n1 basename | sort | tr '\n' ' ' | sed 's/ $//')"
check "an unrelated target/ dir is untouched" "1" \
  "$(ls -1d "$REPO_ROOT"/target/keepme-* | wc -l | tr -d ' ')"

# ── 7. the webui node_modules verdict ────────────────────────────────────────
printf '\nwebui node_modules verdict\n'
# Existence guard first: on the two fresh-expected arms a missing function
# would be indistinguishable from a real "fresh" verdict (both non-zero) and
# read as a false green. A missing verdict is a hard red instead.
if ! type ua_node_modules_stale >/dev/null 2>&1; then
  no "the verdict function exists in the library" "ua_node_modules_stale" "missing — §7 cannot run"
  printf '\n%s\n' "upgrade-propagate: $pass passed, $fail failed"
  exit 1
fi
# mtimes are pinned with -t stamps (and set AFTER the files exist), so the
# fresh/stale ordering can never race the test's own file creation.
mkdir -p "$tmp/webui-fresh/node_modules" "$tmp/webui-stale/node_modules" \
  "$tmp/webui-nolock/node_modules" "$tmp/webui-missing"
: > "$tmp/webui-fresh/package-lock.json"
: > "$tmp/webui-stale/package-lock.json"
: > "$tmp/webui-missing/package-lock.json"
touch -t 202601010000 "$tmp/webui-fresh/package-lock.json" \
  "$tmp/webui-stale/node_modules" "$tmp/webui-nolock/node_modules"
touch -t 202601020000 "$tmp/webui-fresh/node_modules" \
  "$tmp/webui-stale/package-lock.json"

if ua_node_modules_stale "$tmp/webui-fresh"; then
  no "a fresh install is kept" "fresh (exit 1)" "stale (exit 0)"
else
  ok "a fresh install is kept"
fi
# The 2026-10-06 incident's exact shape: dependencies ADDED to package.json
# after the last install. The bare `[ -d node_modules ]` this verdict replaces
# calls exactly this tree fresh — that skip-forever is what this arm pins.
if ua_node_modules_stale "$tmp/webui-stale"; then
  ok "a lockfile newer than the install is stale"
else
  no "a lockfile newer than the install is stale" "stale (exit 0)" "fresh (exit 1)"
fi
if ua_node_modules_stale "$tmp/webui-missing"; then
  ok "a missing install is stale"
else
  no "a missing install is stale" "stale (exit 0)" "fresh (exit 1)"
fi
if ua_node_modules_stale "$tmp/webui-nolock"; then
  no "a lockfile-less tree is left to the build" "fresh (exit 1)" "stale (exit 0)"
else
  ok "a lockfile-less tree is left to the build"
fi

# ── 8. the post-start panel probe helpers (#688) ─────────────────────────────
# The address helpers are pure; the probe itself is driven against a throwaway
# local listener — no production stack, no external network. `000000` (the old
# probe's literal output: curl printed 000, the `|| echo 000` appended another)
# is what every upgrade logged while the panel answered 200 the whole time.
printf '\npanel probe helpers (#688)\n'
if ! type ua_panel_addr >/dev/null 2>&1; then
  no "the probe helpers exist in the library" "ua_panel_addr/ua_probe_url/ua_panel_probe" "missing — §8 cannot run"
  printf '\n%s\n' "upgrade-propagate: $pass passed, $fail failed"
  exit 1
fi

# 8a. address precedence: --addr > BLITZKRIEG_PANEL_ADDR from $REPO_ROOT/.env
# (last assignment wins, quotes stripped — env_file.rs semantics) > loopback.
check "addr: explicit argument wins" "1.2.3.4:9999" "$(ua_panel_addr '1.2.3.4:9999')"
check "addr: no .env, no arg → loopback default" "127.0.0.1:51888" "$(ua_panel_addr '')"
: > "$REPO_ROOT/.env"
check "addr: empty .env → loopback default" "127.0.0.1:51888" "$(ua_panel_addr '')"
printf 'BLITZKRIEG_PANEL_KEY_TEST=x\nBLITZKRIEG_PANEL_ADDR=0.0.0.0:51888\n' > "$REPO_ROOT/.env"
check "addr: .env value wins over default" "0.0.0.0:51888" "$(ua_panel_addr '')"
printf 'BLITZKRIEG_PANEL_ADDR="0.0.0.0:6001"\nBLITZKRIEG_PANEL_ADDR=0.0.0.0:6002\n' > "$REPO_ROOT/.env"
check "addr: last assignment wins" "0.0.0.0:6002" "$(ua_panel_addr '')"
printf "BLITZKRIEG_PANEL_ADDR='0.0.0.0:6003'\n" > "$REPO_ROOT/.env"
check "addr: single quotes stripped" "0.0.0.0:6003" "$(ua_panel_addr '')"
rm "$REPO_ROOT/.env"

# 8b. URL normalization. A wildcard host is probed as the loopback literal:
# `curl http://0.0.0.0/` connects nowhere on macOS, and an IPv6 ::1 target of
# the old line was exactly the silent 000 this section pins.
check "url: loopback passthrough" "http://127.0.0.1:51888/panel/" "$(ua_probe_url '127.0.0.1:51888')"
check "url: wildcard host → loopback literal" "http://127.0.0.1:51888/panel/" "$(ua_probe_url '0.0.0.0:51888')"
check "url: custom port kept" "http://127.0.0.1:9999/panel/" "$(ua_probe_url '0.0.0.0:9999')"
check "url: ipv6 literal gets brackets" "http://[::1]:51888/panel/" "$(ua_probe_url '[::1]:51888')"
check "url: bare ipv6 gets brackets" "http://[::1]:51888/panel/" "$(ua_probe_url '::1')"

# 8c. the probe, end to end, against a throwaway listener. python3 supplies the
# HTTP and also a non-/panel 404 — a real HTTP answer must surface as its code.
live_port=""
for p in 5971 5972 5973 5974 5975; do
  if ! nc -z 127.0.0.1 "$p" 2>/dev/null; then live_port=$p; break; fi
done
[ -n "$live_port" ] || no "a free local port for the probe fixture" "one free in 5971-5975" "none"
if [ -n "$live_port" ]; then
  listen_py='
import socket, sys, threading
from http.server import BaseHTTPRequestHandler, HTTPServer
port = int(sys.argv[1])
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/panel/":
            self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
        else:
            self.send_response(404); self.end_headers(); self.wfile.write(b"no")
    def log_message(self, *a): pass
srv = HTTPServer(("127.0.0.1", port), H)
srv.serve_forever()
'
  python3 -c "$listen_py" "$live_port" &
  fixture_pid=$!
  i=0; up=""
  while [ $i -lt 25 ]; do
    if nc -z 127.0.0.1 "$live_port" 2>/dev/null; then up=1; break; fi
    sleep 0.2; i=$((i + 1))
  done
  if [ -n "$up" ]; then
    check "probe: healthy local gateway answers 200" "200" \
      "$(ua_panel_probe "http://127.0.0.1:$live_port/panel/" 2 2)"
    check "probe: a non-panel path reports its real code" "404" \
      "$(ua_panel_probe "http://127.0.0.1:$live_port/other" 1 2)"
    check "probe: nothing listening is 000 (said once)" "000" \
      "$(ua_panel_probe "http://127.0.0.1:1/panel/" 1 2)"
  else
    no "the probe fixture came up" "listener on $live_port" "no listener"
  fi
fi

printf '\n%s\n' "upgrade-propagate: $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
