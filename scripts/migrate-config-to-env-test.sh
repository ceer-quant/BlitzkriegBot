#!/bin/sh
# migrate-config-to-env-test.sh — acceptance for issue #760 (#759 regression).
#
# The migration script used to emit account policy keys in the wrong order
# (`POLICY_<ID>_<TAIL>`, which the kernel never reads) and to only PRINT rule
# arrays, then retire the source anyway. This test pins the fixed contract:
#
#   1. positive: a scratch deployment migrates global `max_budget_usd = 50`,
#      `[accounts.paper] max_budget_usd = 5` and both rule arrays into
#      `POLICY_MAX_BUDGET_USD`, `POLICY_MAX_BUDGET_USD_PAPER`, `POLICY_RULES`,
#      `POLICY_RULES_PAPER`. The proof is NOT a hand-written expected file: the
#      real kernel loader (`Policy::load_for_reload`) reads the migrated `.env`
#      through `core/blitzkrieg_core/tests/policy_env_oracle.rs` and the test
#      asserts the ORACLE lines it prints (budget 50 / 5, one rule each, skip
#      at two open positions).
#   2. negative: an unconvertible rule (two `then` actions) refuses the WHOLE
#      run — non-zero exit, the policy source and `.env` byte-identical, the
#      account source not moved, no backup dir.
#   3. idempotence: a second run is a no-op that leaves `.env` byte-identical.
#   4. negative: a `[accounts.<id>]` policy section whose id is not in the
#      migrated ACCOUNT_LIST refuses the whole run. The kernel only registers
#      env-only policy sections for ACCOUNT_LIST ids, so those keys would never
#      be read and the account would silently fall back to the global policy —
#      the oracle arm above caught this against the real loader before the guard
#      existed.
#
# Run it from anywhere: `sh scripts/migrate-config-to-env-test.sh` (exit 0 =
# pass). Cargo must be available for the kernel oracle; there is deliberately
# no "skip the kernel" arm, because that is exactly the check #760 was missing.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
migrate="$here/migrate-config-to-env.sh"
[ -f "$migrate" ] || { echo "missing $migrate" >&2; exit 2; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bk-migrate-env-XXXXXX")
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
check_absent() { # check_absent <name> <path>
  if [ -e "$2" ]; then no "$1" "absent" "still exists: $2"; else ok "$1"; fi
}
check_no_line() { # check_no_line <name> <basic regex> <file>
  if grep -qE "$2" "$3"; then no "$1" "no match for $2" "matched"; else ok "$1"; fi
}

# ── 1. positive: real conversion, proven by the real kernel loader ───────────
printf '\npositive migration (kernel oracle)\n'
root="$tmp/root-a"
mkdir -p "$root/user_layer/configs"

cat > "$root/user_layer/configs/accounts.toml" <<'EOF'
[[account]]
id = "default"
name = "main book"
strategies = ["flash_arb"]

[[account]]
id = "paper"
name = "paper book"
EOF

cat > "$root/user_layer/configs/execution_policy.toml" <<'EOF'
version = 1

[defaults]
max_budget_usd = 50
rules = [
  { name = "global_backstop", priority = 100, enabled = true, when = "open_positions >= 2", then = { action = "skip" }, reason = "global backstop" },
]

[accounts.paper]
max_budget_usd = 5
rules = [
  { name = "paper_backstop", priority = 10, enabled = true, when = "open_positions >= 1", then = { action = "skip" }, reason = "paper stops at one" },
]
EOF

printf 'KEEP_ME=1\nPOLICY_MAX_BUDGET_USD=999\n' > "$root/.env"

rc=0
"$migrate" "$root" > "$tmp/pos.out" 2>&1 || rc=$?
check "migration exits 0" "0" "$rc"
if [ "$rc" -ne 0 ]; then
  sed 's/^/         | /' "$tmp/pos.out" >&2
fi

# Key grammar is `POLICY_<TAIL>_<ID>`: tail first, upper-cased account suffix
# last. The old wrong-order key must not exist.
check "global budget key (POLICY_MAX_BUDGET_USD)" "1" \
  "$(grep -cFx 'POLICY_MAX_BUDGET_USD="50"' "$root/.env" || true)"
check "account budget key (POLICY_MAX_BUDGET_USD_PAPER)" "1" \
  "$(grep -cFx 'POLICY_MAX_BUDGET_USD_PAPER="5"' "$root/.env" || true)"
check_no_line "no wrong-order key (POLICY_PAPER_MAX_BUDGET_USD)" \
  '^POLICY_PAPER_MAX_BUDGET_USD=' "$root/.env"
check "global rules key rendered" "1" \
  "$(grep -cE '^POLICY_RULES=' "$root/.env" || true)"
check "paper rules key rendered" "1" \
  "$(grep -cE '^POLICY_RULES_PAPER=' "$root/.env" || true)"
check "account list migrated from accounts.toml" "1" \
  "$(grep -cFx 'ACCOUNT_LIST="default,paper"' "$root/.env" || true)"
check "unowned .env line preserved" "1" \
  "$(grep -cFx 'KEEP_ME=1' "$root/.env" || true)"

# The source is retired only after the verified write.
check_absent "policy source retired" "$root/user_layer/configs/execution_policy.toml"
check_absent "account source retired" "$root/user_layer/configs/accounts.toml"
check "both sources are in the backup dir" "2" \
  "$(find "$root/data" -path '*/config-backup-*/*.toml' -type f | wc -l | tr -d ' ')"

# The oracle: the REAL loader reads the REAL migrated .env. A wrong key order
# or a rules array the loader cannot parse fails here even if the lines match
# this script's own expectations.
oracle_out="$tmp/oracle.txt"
if ( cd "$repo" && \
     BK_ORACLE_ENV="$root/.env" BK_ORACLE_ACCOUNTS="default,paper" \
     cargo test -p blitzkrieg-core --test policy_env_oracle -- --ignored --nocapture ) \
     > "$oracle_out" 2>&1; then
  ok "kernel oracle ran"
else
  no "kernel oracle ran" "cargo test exit 0" "non-zero (see below)"
  sed 's/^/         | /' "$oracle_out" >&2
fi
check_oracle() { # check_oracle <name> <exact ORACLE line>
  if grep -qxF "$2" "$oracle_out"; then
    ok "$1"
  else
    no "$1" "$2" "$(grep '^ORACLE ' "$oracle_out" | tr '\n' ' ' || true)"
  fi
}
check_oracle "oracle: defaults.max_budget_usd = 50" "ORACLE defaults.max_budget_usd=50"
check_oracle "oracle: defaults.rules parsed" "ORACLE defaults.rules=1"
check_oracle "oracle: default resolves to the global budget" "ORACLE default.max_budget_usd=50"
check_oracle "oracle: default sees the rule" "ORACLE default.rules=1"
check_oracle "oracle: default places 50 at zero positions" "ORACLE default.budget_usd=50"
check_oracle "oracle: default skips at two positions" "ORACLE default.evaluate=skip"
check_oracle "oracle: paper.max_budget_usd = 5" "ORACLE paper.max_budget_usd=5"
check_oracle "oracle: paper.rules parsed" "ORACLE paper.rules=1"
check_oracle "oracle: paper places 5 at zero positions" "ORACLE paper.budget_usd=5"
check_oracle "oracle: paper skips at two positions" "ORACLE paper.evaluate=skip"

# ── 2. idempotence: a second run is a no-op ──────────────────────────────────
printf '\nidempotence\n'
cp "$root/.env" "$tmp/env-first"
rc=0
"$migrate" "$root" > "$tmp/idem.out" 2>&1 || rc=$?
check "second run exits 0" "0" "$rc"
check_same "second run leaves .env byte-identical" "$tmp/env-first" "$root/.env"
check "second run reports a no-op" "1" \
  "$(grep -c 'no-op' "$tmp/idem.out" || true)"

# ── 3. negative: an unconvertible rule refuses the whole source ──────────────
printf '\nnegative migration (unconvertible rule)\n'
root_b="$tmp/root-b"
mkdir -p "$root_b/user_layer/configs"
cat > "$root_b/user_layer/configs/accounts.toml" <<'EOF'
[[account]]
id = "default"
name = "main book"
strategies = ["flash_arb"]
EOF
# Two `then` actions: the kernel accepts exactly one, and guessing would not be
# a faithful conversion. The script must refuse, not drop a field.
cat > "$root_b/user_layer/configs/execution_policy.toml" <<'EOF'
version = 1

[defaults]
max_budget_usd = 50
rules = [
  { name = "ambiguous", enabled = true, when = "open_positions >= 1", then = { action = "skip", cooldown_sec = 5 } },
]
EOF
printf 'KEEP_ME=1\n' > "$root_b/.env"
cp "$root_b/.env" "$tmp/neg-env-before"
cp "$root_b/user_layer/configs/execution_policy.toml" "$tmp/neg-policy-before"
cp "$root_b/user_layer/configs/accounts.toml" "$tmp/neg-accounts-before"

rc=0
"$migrate" "$root_b" > "$tmp/neg.out" 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
  ok "refusal exits non-zero"
else
  no "refusal exits non-zero" "non-zero" "0"
fi
check_same "refusal leaves .env byte-identical" "$tmp/neg-env-before" "$root_b/.env"
check_same "refusal leaves the policy source byte-identical" "$tmp/neg-policy-before" \
  "$root_b/user_layer/configs/execution_policy.toml"
check_same "refusal leaves the account source byte-identical (nothing moved early)" \
  "$tmp/neg-accounts-before" "$root_b/user_layer/configs/accounts.toml"
check_no_line "no policy key reached .env" '^POLICY_' "$root_b/.env"
check "no backup directory was created" "0" \
  "$(find "$root_b" -maxdepth 2 -name 'config-backup-*' | wc -l | tr -d ' ')"
check "the scratch workdir is cleaned up" "0" \
  "$(find "$root_b" -maxdepth 1 -name '.migrate-to-env.*' | wc -l | tr -d ' ')"
check "refusal names the policy source" "yes" \
  "$(grep -q 'execution_policy.toml' "$tmp/neg.out" && echo yes || echo no)"
check "refusal says both sides are unchanged" "1" \
  "$(grep -c '源文件与 .env 均未改动' "$tmp/neg.out" || true)"

# ── 4. negative: a policy account the account source does not list ───────────
# The kernel discovers env-only policy sections FROM ACCOUNT_LIST. Keys for an
# id that list does not name are never read, so that account would silently
# inherit the GLOBAL policy — a relaxation, not a migration. The oracle arm
# above caught exactly this against the real loader; this arm pins the refusal.
printf '\nnegative migration (policy account absent from ACCOUNT_LIST)\n'
root_c="$tmp/root-c"
mkdir -p "$root_c/user_layer/configs"
cat > "$root_c/user_layer/configs/accounts.toml" <<'EOF'
[[account]]
id = "default"
name = "main book"
EOF
cat > "$root_c/user_layer/configs/execution_policy.toml" <<'EOF'
version = 1

[defaults]
max_budget_usd = 50

[accounts.ghost]
max_budget_usd = 5
EOF
printf 'KEEP_ME=1\n' > "$root_c/.env"
cp "$root_c/.env" "$tmp/orphan-env-before"
cp "$root_c/user_layer/configs/execution_policy.toml" "$tmp/orphan-policy-before"
cp "$root_c/user_layer/configs/accounts.toml" "$tmp/orphan-accounts-before"

rc=0
"$migrate" "$root_c" > "$tmp/orphan.out" 2>&1 || rc=$?
check "orphan policy account refuses the run" "yes" \
  "$(if [ "$rc" -ne 0 ]; then echo yes; else echo no; fi)"
check_same "orphan refusal leaves .env byte-identical" "$tmp/orphan-env-before" "$root_c/.env"
check_same "orphan refusal leaves the policy source byte-identical" "$tmp/orphan-policy-before" \
  "$root_c/user_layer/configs/execution_policy.toml"
check_same "orphan refusal leaves the account source byte-identical" \
  "$tmp/orphan-accounts-before" "$root_c/user_layer/configs/accounts.toml"
check_no_line "no policy key reached .env (orphan)" '^POLICY_' "$root_c/.env"
check "orphan refusal names the id and the account list" "yes" \
  "$(grep -q 'accounts.ghost' "$tmp/orphan.out" && grep -q 'ACCOUNT_LIST' "$tmp/orphan.out" && echo yes || echo no)"

printf '\n%s\n' "migrate-config-to-env: $pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
