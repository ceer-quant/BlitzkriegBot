#!/usr/bin/env bash
#
# scripts/migrate-config-to-env.sh — .env single-source migration (issue #751).
#
# Moves the five legacy config sources into the .env single source and retires
# the files: each source is MOVED (never rm'd, never copied-and-left) into
# data/config-backup-<timestamp>/. Key grammar, spelled exactly as the kernel
# parses it — read the authorities before editing either side:
#
#   user_layer/configs/accounts.toml
#     → ACCOUNT_LIST / ACCOUNT_DISPLAY / ACCOUNT_<ID>_{NAME,VENUE,STATUS,
#       TRADING_ENABLED,STRATEGIES,ROUNDS,API_KEY_ENV,SECRET_ENV,PASSPHRASE_ENV,
#       PRIVATE_KEY_ENV} / ACCOUNT_ARB_{LEG_A_VENUE,LEG_A_ACCOUNT,LEG_B_VENUE,
#       LEG_B_ACCOUNT,PAIRS}
#       (authority: core/blitzkrieg_core/src/account/config.rs —
#        load_account_ledgers_from_env / parse_env_account / env_suffix;
#        <ID> = the account id upper-cased, every non-alphanumeric → `_`)
#   user_layer/configs/shadow_evolution.toml
#     → SHADOW_ENABLED / SHADOW_AUTO_EVOLVE (the two file-owned switches;
#       authority: core/blitzkrieg_core/src/bin_support/boot.rs)
#   data/runtime/config.json
#     → RUNTIME_ROUND_DURATION_SEC (JSON field roundDurationSec; authority:
#       core/blitzkrieg_core/src/ipc/runtime_config.rs)
#   user_layer/configs/execution_policy.toml
#
# NOTE: user_layer/configs/default.toml is RETIRED (2026-10-28) — it carries no
# active configuration keys (engine keys moved to BK_* env in #588; exit/risk
# knobs stay at their compiled defaults unless uncommented in a copy). It is
# documentation only and is NOT migrated by this script; nothing to move.
#     → POLICY_BUDGET_RATIO / POLICY_MIN_BUDGET_USD / POLICY_MAX_BUDGET_USD /
#       POLICY_MIN_EQUITY_USD / POLICY_MAX_POSITIONS_PER_ASSET, global and
#       per-account in the kernel's EXACT grammar `POLICY_<TAIL>_<ID>` (the
#       tail first, the upper-cased account suffix last — e.g.
#       POLICY_MAX_BUDGET_USD_PAPER, NOT POLICY_PAPER_MAX_BUDGET_USD).
#       The [[*.rules]] arrays migrate for real into POLICY_RULES /
#       POLICY_RULES_<ID>, the JSON array `apply_env` parses. A rule or
#       section this script cannot convert faithfully refuses the WHOLE policy
#       source: non-zero exit, source file and .env both untouched.
#       (authority: core/blitzkrieg_core/src/execution_policy.rs —
#        apply_env / env_has_policy_section / env_account_suffix)
#   user_layer/configs/update.toml
#     → BLITZKRIEG_UPDATE_CHECK / BLITZKRIEG_UPDATE_AUTO /
#       BLITZKRIEG_UPDATE_INTERVAL_SECS (authority:
#       core/blitzkrieg_core/src/bin_support/boot.rs)
#
# Safety posture (§9.4 + the #751 ruling):
#   - Credential fields carry the NAMES of environment variables, never values;
#     the script reads only names and never prints any value it sees.
#   - .env lines the migration does not own (credential lines etc.) are
#     preserved byte-for-byte. Owned lines are replaced in place; new keys are
#     appended. The file is rewritten atomically (temp file in the same
#     directory + rename) and kept at 0600.
#   - data/ runtime projections (positions/trades/orders, strategy-state.json,
#     update/state.json, evolution/state.json) are NOT touched — runtime
#     state, not configuration.
#   - Any parse anomaly abandons that source BEFORE the .env is written: the
#     file stays in place, none of its keys land, and the report says so
#     (fail-closed, the same discipline the kernel's loaders run). An
#     unconvertible execution_policy.toml is stricter still: the WHOLE run
#     refuses with a non-zero exit and neither the source nor the .env moves.
#   - A source is retired (moved to the backup dir) only AFTER the .env has
#     been written atomically and verified: every rendered key line must be
#     present. If a move fails, the .env is already consistent and the source
#     stays in place; the run exits non-zero so the next invocation retries.
#
# Usage: scripts/migrate-config-to-env.sh [ROOT]
#   ROOT defaults to the repo root (this script's grandparent). Every path —
#   user_layer/configs, data/, .env — resolves under ROOT, so a rehearsal runs
#   against a scratch directory without touching production data:
#     scripts/migrate-config-to-env.sh ./target-mig-test/root-a
#
# Idempotent: sources already moved ⇒ a no-op that says so.

set -euo pipefail
export LC_ALL=C

usage() {
  sed -n '2,64p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}
case "${1:-}" in
  -h | --help) usage ;;
esac

ROOT="${1:-}"
if [ -z "$ROOT" ]; then
  ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi
ROOT="$(cd "$ROOT" && pwd)"
CONFIG_DIR="$ROOT/user_layer/configs"
DATA_DIR="$ROOT/data"
ENV_FILE="$ROOT/.env"

die() {
  printf 'migrate-config-to-env: FATAL: %s\n' "$*" >&2
  exit 1
}

# Scratch space lives under ROOT (never /tmp), on the same filesystem as the
# .env so the final rename stays atomic.
WORKDIR="$(mktemp -d "$ROOT/.migrate-to-env.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT
VARS_FILE="$WORKDIR/vars.lines" # one rendered KEY="value" line per key, commit order kept

PENDING=""        # one migrator's rendered lines, flushed only on success
PENDING_KEYS=""   # key names this migrator rendered (report; names only)
FILES_TO_MOVE=""  # sources whose migration fully succeeded
SKIPPED=""        # absent sources
LEFT_BEHIND=""    # sources that failed to migrate
NOTES=""          # manual follow-ups for the final report
# The account ids the account source migrated (ACCOUNT_LIST). The policy
# migration cross-checks against it: the kernel only registers an env-only
# policy section for an id ACCOUNT_LIST names, so a policy section for any
# other id would silently fall back to the global policy (issue #760).
MIGRATED_ACCOUNT_IDS=""
ANY_SOURCE=0
BACKUP_DIR=""
MOVED_LIST=""

say() { printf '%s\n' "$*"; }
warn() { printf '  ! %s\n' "$*" >&2; }

add_note() { NOTES="${NOTES}- $*"$'\n'; }

# The env-var suffix for one account id: uppercase, every non-alphanumeric
# byte-folded to `_` — byte-for-byte the kernel's env_suffix() (config.rs).
env_suffix() {
  python3 -c 'import sys
s = sys.argv[1]
sys.stdout.write("".join(c.upper() if c.isascii() and c.isalnum() else "_" for c in s))' "$1"
}

# Render one KEY=value line into $RENDERED the kernel's .env parser
# round-trips: double-quote by default, single-quote when the value itself
# carries a `"`, refuse (die — this runs in the main shell) a value carrying
# both (the .env format has no escapes — the same rule the kernel's
# env_file::render_line runs). Renders nothing for an empty value (env absence
# is the kernel's "unset" spelling for every key this script owns).
render() {
  RENDERED=""
  local key="$1" value="$2"
  [ -n "$value" ] || return 0
  case "$value" in
    *'"'*)
      case "$value" in
        *"'"*)
          die "refusing to write $key: its value carries both quote kinds — the .env format has no escapes; migrate that key by hand" ;;
        *) printf -v RENDERED -- "%s='%s'" "$key" "$value" ;;
      esac ;;
    *) printf -v RENDERED -- '%s="%s"' "$key" "$value" ;;
  esac
}

# Append a rendered key to the migrator's PENDING buffer.
emit() {
  render "$1" "$2"
  [ -n "$RENDERED" ] || return 0
  PENDING="${PENDING}${RENDERED}"$'\n'
  PENDING_KEYS="$PENDING_KEYS $1"
}

# Commit one migrator's PENDING to the merge input and queue its file for the
# backup move. $1 = the source file (moved later, only after ALL parsing).
commit() {
  local keys="${PENDING_KEYS# }"
  if [ -n "$PENDING" ]; then
    printf '%s' "$PENDING" >> "$VARS_FILE"
  fi
  FILES_TO_MOVE="${FILES_TO_MOVE}${1}"$'\n'
  say "  ok: ${keys:-（该源无键写入，仅退役文件）}"
  PENDING=""
  PENDING_KEYS=""
}

# Discard one migrator's PENDING and record the file as left in place.
abandon() {
  LEFT_BEHIND="${LEFT_BEHIND}${1}"$'\n'
  say "  !! 该源未迁移：键未写入，文件原位保留"
  PENDING=""
  PENDING_KEYS=""
}

# ── TOML line scanner ────────────────────────────────────────────────────────
# Deliberately NOT a TOML parser: a per-line, section-aware scanner for the
# fixed shapes this repo's config files use. Emits TAB-separated events:
#   T<TAB><section>             a table header ([[x]] → x#N#arr, [x.y] → x.y)
#   K<TAB>key<TAB>kind<TAB>val  an assignment; kind ∈
#                               s=quoted string (unquoted), a=flat one-line
#                               array (comma-joined items), b=bare token
#                               (inline comment stripped), t=inline table,
#                               m=multi-line array opener, x=malformed
# Unparseable lines go to stderr as warnings and are otherwise ignored.
scan_toml() {
  awk '
    function ltrim(s) { sub(/^[ \t\r]+/, "", s); return s }
    function rtrim(s) { sub(/[ \t\r]+$/, "", s); return s }
    function norm(raw,    v, q, i, ch, inner, n, parts, item, c, m) {
      v = raw
      v = ltrim(v); v = rtrim(v)
      VK = "b"; VV = v
      if (v == "") { VK = "x"; return }
      q = substr(v, 1, 1)
      if (q == "\"") {
        for (i = 2; i <= length(v); i++) {
          ch = substr(v, i, 1)
          if (ch == "\\") {
            i++
            if (i > length(v)) { VK = "x"; return }
            ch = substr(v, i, 1)
            if (ch == "\"" || ch == "\\") continue
            VK = "x"; return
          }
          if (ch == "\"") break
        }
        if (i > length(v)) { VK = "x"; return }
        inner = substr(v, 2, i - 2)
        m = ""
        for (i = 1; i <= length(inner); i++) {
          ch = substr(inner, i, 1)
          if (ch == "\\" && i < length(inner)) {
            i++; ch = substr(inner, i, 1)
            if (ch == "\"") ch = "\""
            else if (ch == "\\") ch = "\\"
            else { VK = "x"; return }
          }
          m = m ch
        }
        VK = "s"; VV = m
      } else if (q == "'"'"'") {
        c = index(substr(v, 2), "'"'"'")
        if (c == 0) { VK = "x"; return }
        VK = "s"; VV = substr(v, 2, c - 1)
      } else if (q == "[") {
        if (substr(v, length(v), 1) != "]") { VK = "m"; return }
        inner = substr(v, 2, length(v) - 2)
        if (index(inner, "[") || index(inner, "{") || index(inner, "]")) { VK = "x"; return }
        m = ""
        n = split(inner, parts, ",")
        for (i = 1; i <= n; i++) {
          item = parts[i]
          item = ltrim(item); item = rtrim(item)
          if (item == "") continue
          q = substr(item, 1, 1)
          if (q == "\"" || q == "'"'"'") {
            if (substr(item, length(item), 1) != q) { VK = "x"; return }
            item = substr(item, 2, length(item) - 2)
          }
          if (index(item, "\"") || index(item, "'"'"'")) { VK = "x"; return }
          if (m == "") m = item; else m = m "," item
        }
        VK = "a"; VV = m
      } else if (q == "{") {
        VK = "t"; VV = v
      } else {
        c = index(v, "#")
        if (c) v = substr(v, 1, c - 1)
        v = rtrim(v)
        if (v == "") { VK = "x"; return }
        VK = "b"; VV = v
      }
    }
    BEGIN { section = "" }
    {
      line = $0
      sub(/\r$/, "", line)
      trimmed = line
      sub(/^[ \t]+/, "", trimmed)
      if (trimmed == "" || substr(trimmed, 1, 1) == "#") next
      if (substr(trimmed, 1, 2) == "[[") {
        hdr = trimmed
        sub(/^\[\[/, "", hdr)
        sub(/\]\].*$/, "", hdr)
        hdr = rtrim(ltrim(hdr))
        cnt[hdr]++
        section = hdr "#" cnt[hdr] "#arr"
        print "T\t" section
        next
      }
      if (substr(trimmed, 1, 1) == "[") {
        hdr = trimmed
        sub(/^\[/, "", hdr)
        sub(/\].*$/, "", hdr)
        section = rtrim(ltrim(hdr))
        print "T\t" section
        next
      }
      eq = index(trimmed, "=")
      if (eq == 0) {
        print "warning: uninterpreted line: " trimmed > "/dev/stderr"
        next
      }
      key = rtrim(ltrim(substr(trimmed, 1, eq - 1)))
      if (index(key, "\"") || index(key, "'"'"'")) {
        print "warning: quoted key not supported: " key > "/dev/stderr"
        next
      }
      norm(substr(trimmed, eq + 1))
      print "K\t" key "\t" VK "\t" VV
      next
    }
  ' "$1"
}

# ── accounts.toml ────────────────────────────────────────────────────────────
# [[account]] tables in book order → ACCOUNT_LIST + per-account keys.
# The per-account suffix is only known once the id is read (usually the first
# key of the table, but TOML does not promise it), so keys render against a
# sentinel and the real suffix is substituted when the table closes.
# Fail-closed: any incoherence (no id, duplicate id, an unmappable status)
# abandons the WHOLE file — accounts.toml is the trading book; a half-migrated
# book is exactly what the kernel's loaders refuse to boot on.
migrate_accounts_toml() {
  local f="$CONFIG_DIR/accounts.toml"
  if [ ! -f "$f" ]; then
    SKIPPED="${SKIPPED}user_layer/configs/accounts.toml"$'\n'
    return 0
  fi
  ANY_SOURCE=1
  say "── user_layer/configs/accounts.toml"
  local ev="$WORKDIR/accounts.events"
  scan_toml "$f" > "$ev"

  local tag p1 p2 p3 sec="" cur_id="" cur_buf="" cur_keys="" ids="" display="" bad=0
  local sfx word in_arb=0 key_seen=0
  local ARB_LA_V="" ARB_LA_A="" ARB_LB_V="" ARB_LB_A="" ARB_PAIRS=""

  # Render one per-account key against the sentinel suffix (the real suffix is
  # substituted when the table closes). The die message names the readable
  # ACCOUNT_<ID>_<TAIL> spelling, the file line carries the sentinel.
  emit_acc() {
    local tail="$1" value="$2"
    render "ACCOUNT_<ID>_${tail}" "$value"
    [ -n "$RENDERED" ] || return 0
    RENDERED="ACCOUNT_SFXSENT_${tail}${RENDERED#ACCOUNT_<ID>_${tail}}"
    cur_buf="${cur_buf}${RENDERED}"$'\n'
    cur_keys="$cur_keys ACCOUNT_SFXSENT_${tail}"
  }

  _mig_flush_account() {
    if [ -n "$cur_id" ]; then
      sfx="$(env_suffix "$cur_id")"
      if [ -z "$sfx" ]; then
        warn "账户 id 非法（env 后缀为空）——accounts.toml 不迁移"
        bad=1
      elif [ -z "$ids" ]; then
        ids="$cur_id"
      else
        case ",$ids," in
          *",$cur_id,"*)
            warn "账户 id 重复: $cur_id —— accounts.toml 不迁移（ACCOUNT_LIST 不容重复）"
            bad=1 ;;
          *) ids="$ids,$cur_id" ;;
        esac
      fi
    elif [ -n "$cur_buf" ]; then
      warn "一个 [[account]] 表没有 id 字段——accounts.toml 不迁移"
      bad=1
    fi
    if [ "$bad" = 0 ] && [ -n "$cur_id" ]; then
      cur_buf="${cur_buf//SFXSENT/$sfx}"
      cur_keys="${cur_keys//SFXSENT/$sfx}"
      PENDING="${PENDING}${cur_buf}"
      PENDING_KEYS="$PENDING_KEYS$cur_keys"
    fi
    cur_id=""
    cur_buf=""
    cur_keys=""
  }

  while IFS=$'\t' read -r tag p1 p2 p3; do
    case "$tag" in
      T)
        _mig_flush_account
        sec="$p1"
        case "$sec" in
          cross_venue_arb) in_arb=1 ;;
          *) in_arb=0 ;;
        esac
        ;;
      K)
        key_seen=1
        if [ "$in_arb" = 1 ]; then
          case "$p1" in
            leg_a_venue) ARB_LA_V="$p3" ;;
            leg_a_account) ARB_LA_A="$p3" ;;
            leg_b_venue) ARB_LB_V="$p3" ;;
            leg_b_account) ARB_LB_A="$p3" ;;
            pairs)
              if [ "$p2" = "a" ]; then ARB_PAIRS="$p3"; fi ;;
          esac
          continue
        fi
        case "$sec" in
          account\#*#arr)
            case "$p1" in
              id)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then cur_id="$p3"; fi ;;
              name)
                if [ "$p2" = "s" ]; then emit_acc "NAME" "$p3"; fi ;;
              venue)
                if [ "$p2" = "s" ]; then emit_acc "VENUE" "$p3"; fi ;;
              status)
                case "$p2" in
                  s | b)
                    case "$p3" in
                      active | read_only | frozen | suspended)
                        emit_acc "STATUS" "$p3" ;;
                      *)
                        warn "未知 status 拼写（active/read_only/frozen/suspended 才合法）——accounts.toml 不迁移"
                        bad=1 ;;
                    esac ;;
                  t)
                    # map form: `status = { suspended = { reason = "..." } }` —
                    # the env grammar spells the bare word; the reason string
                    # cannot migrate.
                    word="$(printf '%s' "$p3" | sed -n 's/^{[[:space:]]*\([a-z_]*\).*/\1/p')"
                    case "$word" in
                      active | read_only | frozen | suspended)
                        warn "status 为 map 形式：reason 字符串无法迁入 .env，按裸词 $word 迁移"
                        emit_acc "STATUS" "$word" ;;
                      *)
                        warn "status 的 map 形式无法解析——accounts.toml 不迁移"
                        bad=1 ;;
                    esac ;;
                  *)
                    warn "status 值无法解析——accounts.toml 不迁移"
                    bad=1 ;;
                esac ;;
              trading_enabled)
                case "$p3" in
                  true) emit_acc "TRADING_ENABLED" "true" ;;
                  false) emit_acc "TRADING_ENABLED" "false" ;;
                  *) warn "trading_enabled 不是布尔——跳过该键（缺省 = true）" ;;
                esac ;;
              strategies)
                case "$p2" in
                  a) emit_acc "STRATEGIES" "$p3" ;;
                  m)
                    warn "strategies 数组跨行——该键不迁移（fail-closed：账户零 scope 启动，之后手动补 ACCOUNT_<ID>_STRATEGIES）" ;;
                  *) warn "strategies 不是数组——跳过该键" ;;
                esac ;;
              rounds)
                case "$p2" in
                  a)
                    if printf '%s' "$p3" | grep -q '[^0-9,]'; then
                      warn "rounds 含非整数项——仍按原文迁移（内核启动时校验轮次族）"
                    fi
                    emit_acc "ROUNDS" "$p3" ;;
                  m)
                    warn "rounds 数组跨行——该键不迁移（fail-closed：无轮次族限制，之后手动补 ACCOUNT_<ID>_ROUNDS）" ;;
                  *) warn "rounds 不是数组——跳过该键" ;;
                esac ;;
              api_key_env)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit_acc "API_KEY_ENV" "$p3"; fi ;;
              secret_env)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit_acc "SECRET_ENV" "$p3"; fi ;;
              passphrase_env)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit_acc "PASSPHRASE_ENV" "$p3"; fi ;;
              private_key_env)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit_acc "PRIVATE_KEY_ENV" "$p3"; fi ;;
              market_type) : ;; # 0.3 唯一有插件的市场类型是 prediction，env 文法没有这个键
            esac ;;
          "")
            case "$p1" in
              display_account)
                if [ "$p2" = "s" ]; then display="$p3"; fi ;;
            esac ;;
        esac ;;
    esac
  done < "$ev"
  _mig_flush_account

  if [ "$bad" = 1 ]; then
    abandon "$f"
    return 0
  fi
  if [ -z "$ids" ]; then
    if [ "$key_seen" = 0 ]; then
      warn "accounts.toml 没有任何 [[account]] 表"
    fi
    abandon "$f"
    return 0
  fi
  # ACCOUNT_LIST first, then the per-account lines, then the display pointer.
  MIGRATED_ACCOUNT_IDS="$ids"
  PENDING="ACCOUNT_LIST=\"${ids}\""$'\n'"${PENDING}"
  PENDING_KEYS=" ACCOUNT_LIST${PENDING_KEYS}"
  if [ -n "$display" ]; then
    case ",$ids," in
      *",$display,"*) : ;;
      *) warn "display_account=$display 不在 ACCOUNT_LIST 里——照迁（内核启动时 fail-closed 校验）" ;;
    esac
    emit "ACCOUNT_DISPLAY" "$display"
  fi
  # Cross-venue arb is all-or-nothing in the kernel; migrate only a complete
  # section — anything half-written stays a manual item.
  if [ -n "$ARB_LA_V$ARB_LA_A$ARB_LB_V$ARB_LB_A$ARB_PAIRS" ]; then
    if [ -n "$ARB_LA_V" ] && [ -n "$ARB_LA_A" ] && [ -n "$ARB_LB_V" ] && [ -n "$ARB_LB_A" ] && [ -n "$ARB_PAIRS" ]; then
      emit "ACCOUNT_ARB_LEG_A_VENUE" "$ARB_LA_V"
      emit "ACCOUNT_ARB_LEG_A_ACCOUNT" "$ARB_LA_A"
      emit "ACCOUNT_ARB_LEG_B_VENUE" "$ARB_LB_V"
      emit "ACCOUNT_ARB_LEG_B_ACCOUNT" "$ARB_LB_A"
      emit "ACCOUNT_ARB_PAIRS" "$ARB_PAIRS"
    else
      warn "[cross_venue_arb] 半配置——不迁移（内核拒绝半写的 arb 段）；请手动补 ACCOUNT_ARB_* 键"
      add_note "[cross_venue_arb] 段不完整，未迁移：手动核对备份文件并补 ACCOUNT_ARB_LEG_A_VENUE/_ACCOUNT、ACCOUNT_ARB_LEG_B_VENUE/_ACCOUNT、ACCOUNT_ARB_PAIRS。"
    fi
  fi
  if [ -n "$PENDING_KEYS" ]; then
    add_note "ACCOUNT_<ID>_STRATEGIES 只在 toml 行有该键且非空时写入；无键账户按 #552 语义零 scope 启动（default 账户由内核一次性从 data/strategy-state.json 物化，该文件保留不动）。"
  fi
  commit "$f"
}

# ── shadow_evolution.toml ────────────────────────────────────────────────────
# Only the two file-owned switches migrate (boot.rs reads SHADOW_ENABLED /
# SHADOW_AUTO_EVOLVE); the window/threshold knobs fall back to the compiled
# defaults, overridable per boot with BK_SE_*.
migrate_shadow_toml() {
  local f="$CONFIG_DIR/shadow_evolution.toml"
  if [ ! -f "$f" ]; then
    SKIPPED="${SKIPPED}user_layer/configs/shadow_evolution.toml"$'\n'
    return 0
  fi
  ANY_SOURCE=1
  say "── user_layer/configs/shadow_evolution.toml"
  local ev="$WORKDIR/shadow.events"
  scan_toml "$f" > "$ev"

  local tag p1 p2 p3 sec="" v_enabled="" v_auto=""
  while IFS=$'\t' read -r tag p1 p2 p3; do
    case "$tag" in
      T) sec="$p1" ;;
      K)
        if [ "$sec" != "shadow_evolution" ]; then continue; fi
        case "$p1" in
          enabled)
            case "$p3" in
              true) v_enabled="true" ;;
              false) v_enabled="false" ;;
              *) warn "shadow_evolution.enabled 不是布尔——该键不迁移" ;;
            esac ;;
          auto_evolve)
            case "$p3" in
              true) v_auto="true" ;;
              false) v_auto="false" ;;
              *) warn "shadow_evolution.auto_evolve 不是布尔——该键不迁移" ;;
            esac ;;
        esac ;;
    esac
  done < "$ev"
  if [ -n "$v_enabled" ] || [ -n "$v_auto" ]; then
    emit "SHADOW_ENABLED" "$v_enabled"
    emit "SHADOW_AUTO_EVOLVE" "$v_auto"
    add_note "shadow_evolution.toml 只迁两个开关；窗口/阈值等调参回落编译默认，个别要改的用 BK_SE_* 覆盖（见 .env.example）。"
    commit "$f"
  else
    warn "shadow_evolution.toml 没有可迁移的开关值"
    abandon "$f"
  fi
}

# ── execution_policy.toml ────────────────────────────────────────────────────
# [defaults] / [accounts.<id>] scalars and [[*.rules]] arrays → POLICY_* keys.
# The account grammar is the kernel's `POLICY_<TAIL>_<ID>` (apply_env appends
# `_<suffix>` AFTER the tail), e.g. POLICY_MAX_BUDGET_USD_PAPER. Rules become
# the JSON array apply_env parses (`POLICY_RULES` / `POLICY_RULES_<ID>`).
#
# The TOML is parsed with Python's tomllib (a real parser, not the line
# scanner the other sources use) because rules are nested tables and inline
# tables: a half-understood rule is exactly the "silently un-guarded kernel"
# this migration must not produce. EVERY rule/section is validated against the
# kernel's grammar before anything is emitted. An unconvertible rule or
# section refuses the WHOLE source (non-zero exit; the source and .env stay
# byte-identical) — a printed recipe is not a migration. A `[accounts.<id>]`
# section whose id is not in the migrated ACCOUNT_LIST is refused too: the
# kernel discovers env-only policy sections FROM ACCOUNT_LIST, so those keys
# would never be read and the account would silently inherit the global
# policy (issue #760).
migrate_policy_toml() {
  local f="$CONFIG_DIR/execution_policy.toml"
  if [ ! -f "$f" ]; then
    SKIPPED="${SKIPPED}user_layer/configs/execution_policy.toml"$'\n'
    return 0
  fi
  ANY_SOURCE=1
  say "── user_layer/configs/execution_policy.toml"

  local out rc=0 err="$WORKDIR/policy.err"
  # The account ids the kernel will know after this migration. A policy section
  # for any other id would migrate to keys the kernel never registers (its
  # env-only sections are discovered FROM ACCOUNT_LIST), silently dropping that
  # account back to the global policy — the exact relaxation #760 forbids. The
  # account source's migration result comes first in the run; an
  # already-migrated `.env` is the fallback when it did not run.
  local known_ids="${MIGRATED_ACCOUNT_IDS:-}"
  if [ -z "$known_ids" ] && [ -f "$ENV_FILE" ]; then
    known_ids="$(sed -n 's/^[[:space:]]*ACCOUNT_LIST[[:space:]]*=[[:space:]]*//p' "$ENV_FILE" |
      tail -n 1 | sed 's/^["'\'']//; s/["'\'']$//')"
  fi
  out="$(python3 - "$f" "$known_ids" 2>"$err" <<'PY'
import json
import math
import sys

try:
    import tomllib
except ImportError:  # Python < 3.11
    try:
        import tomli as tomllib
    except ImportError:
        print("需要 Python 3.11+ 的 tomllib（或 tomli）来忠实解析规则数组", file=sys.stderr)
        sys.exit(3)


def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(3)


SCALARS = (
    "budget_ratio",
    "min_budget_usd",
    "max_budget_usd",
    "min_equity_usd",
    "max_positions_per_asset",
)
DECIMAL_SCALARS = {
    "budget_ratio": "BUDGET_RATIO",
    "min_budget_usd": "MIN_BUDGET_USD",
    "max_budget_usd": "MAX_BUDGET_USD",
    "min_equity_usd": "MIN_EQUITY_USD",
}
TAIL_MAX_POSITIONS = "MAX_POSITIONS_PER_ASSET"
NUMERIC_FIELDS = {
    "available_balance",
    "total_equity",
    "open_positions",
    "current_price",
    "time_left_sec",
    "recent_pnl_1h",
    "consecutive_losses",
}
TEXT_FIELDS = {"symbol"}
FIELDS = NUMERIC_FIELDS | TEXT_FIELDS
TABLE_OPS = {"<", "<=", ">", ">=", "==", "=", "!=", "in", "lt", "le", "gt", "ge", "eq", "ne"}


def is_number(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def decimal_text(v, what):
    # wire_dec accepts numbers AND decimal strings; pass both through in the
    # spelling the kernel's Decimal::from_str_exact reads.
    if isinstance(v, str):
        return v
    if not is_number(v):
        fail(f"{what} 必须是数字或十进制字符串")
    if isinstance(v, float) and not math.isfinite(v):
        fail(f"{what} 必须是有限数字")
    return repr(v) if isinstance(v, float) else str(v)


def u32_text(v, what):
    if not isinstance(v, int) or isinstance(v, bool) or v < 0:
        fail(f"{what} 必须是非负整数")
    return str(v)


def env_suffix(account_id):
    return "".join(c if c.isascii() and c.isalnum() else "_" for c in account_id).upper()


def scalar_pairs(section, label):
    out = []
    for key in SCALARS:
        if key not in section:
            continue
        if key == "max_positions_per_asset":
            out.append((TAIL_MAX_POSITIONS, u32_text(section[key], f"{label}.{key}")))
        else:
            out.append((DECIMAL_SCALARS[key], decimal_text(section[key], f"{label}.{key}")))
    return out


def parse_decimal(text):
    try:
        from decimal import Decimal, InvalidOperation

        d = Decimal(text)
    except (InvalidOperation, ValueError):
        return None
    return d if d.is_finite() else None


def validate_when_string(text, label):
    text = text.strip()
    if not text:
        fail(f"{label}: when 不能为空")
    found = None
    for op in (">=", "<=", "!=", "==", "<", ">"):
        idx = text.find(op)
        if idx >= 0:
            found = (op, idx, len(op))
            break
    if found is None:
        idx = text.find(" in ")
        if idx >= 0:
            found = ("in", idx + 1, 2)
    if found is None:
        fail(f"{label}: when `{text}` 没有运算符")
    op, idx, oplen = found
    field = text[:idx].strip()
    rest = text[idx + oplen :].strip()
    if field not in FIELDS:
        fail(f"{label}: when 的字段 `{field}` 不是内核支持的 8 个字段之一")
    if field in TEXT_FIELDS:
        if rest.startswith("[") and rest.endswith("]"):
            items = [p.strip().strip("\"'") for p in rest[1:-1].split(",")]
            if not [p for p in items if p]:
                fail(f"{label}: when 的列表 `{rest}` 为空")
            return
        if len(rest) >= 2 and rest[0] == rest[-1] and rest[0] in "\"'":
            return
        fail(f"{label}: when 的 `{rest}` 既不是带引号的 symbol 也不是列表")
    else:
        if parse_decimal(rest) is None:
            fail(f"{label}: when 的 `{rest}` 不是数字")


def validate_when_table(when, label):
    extra = set(when) - {"field", "op", "value"}
    if extra:
        fail(f"{label}: when 含不支持的键: {', '.join(sorted(extra))}")
    for req in ("field", "op", "value"):
        if req not in when:
            fail(f"{label}: when 缺少 `{req}`")
    field, op, val = when["field"], when["op"], when["value"]
    if not isinstance(field, str) or field not in FIELDS:
        fail(f"{label}: when.field `{field}` 不是内核支持的字段")
    if not isinstance(op, str) or op not in TABLE_OPS:
        fail(f"{label}: when.op `{op}` 不是内核支持的运算符")
    if field in TEXT_FIELDS:
        if isinstance(val, str):
            return
        if isinstance(val, list) and all(isinstance(x, str) for x in val):
            return
        fail(f"{label}: when.value 对 symbol 字段必须是字符串或字符串列表")
    else:
        if not is_number(val):
            fail(f"{label}: when.value 对数值字段必须是数字")
        if isinstance(val, float) and not math.isfinite(val):
            fail(f"{label}: when.value 必须是有限数字")


def validate_rule(rule, label):
    if not isinstance(rule, dict):
        fail(f"{label} 必须是一个规则表")
    allowed = {"name", "priority", "enabled", "when", "then", "reason"}
    extra = set(rule) - allowed
    if extra:
        fail(f"{label} 含不支持的键: {', '.join(sorted(extra))}")
    name = rule.get("name")
    if not isinstance(name, str) or not name:
        fail(f"{label}.name 必须是非空字符串")
    enabled = rule.get("enabled")
    if not isinstance(enabled, bool):
        fail(f"{label}.enabled 必须是布尔值")
    if "priority" in rule:
        u32_text(rule["priority"], f"{label}.priority")
    when = rule.get("when")
    if isinstance(when, str):
        validate_when_string(when, label)
    elif isinstance(when, dict):
        validate_when_table(when, label)
    else:
        fail(f"{label}.when 必须是字符串或表")
    then = rule.get("then")
    if not isinstance(then, dict):
        fail(f"{label}.then 必须是表")
    allowed_then = {"action", "budget_ratio", "min_budget_usd", "max_budget_usd", "cooldown_sec"}
    extra = set(then) - allowed_then
    if extra:
        fail(f"{label}.then 含不支持的键: {', '.join(sorted(extra))}")
    present = [k for k in allowed_then if k in then]
    if len(present) != 1:
        fail(f"{label}.then 必须恰好有一个动作（当前 {len(present)} 个）")
    action = present[0]
    if action == "action":
        if then[action] != "skip":
            fail(f'{label}.then.action 只能是 "skip"')
    elif action == "cooldown_sec":
        v = then[action]
        if not isinstance(v, int) or isinstance(v, bool) or v <= 0:
            fail(f"{label}.then.cooldown_sec 必须是正整数")
    else:
        decimal_text(then[action], f"{label}.then.{action}")
    if "reason" in rule and not isinstance(rule["reason"], str):
        fail(f"{label}.reason 必须是字符串")
    out = {
        "name": name,
        "priority": rule.get("priority", 100),
        "enabled": enabled,
        "when": when,
        "then": then,
    }
    if "reason" in rule:
        out["reason"] = rule["reason"]
    return out


def rules_json(rules, label):
    if not isinstance(rules, list):
        fail(f"{label} 必须是规则表数组")
    validated = [validate_rule(r, f"{label}[{i}]") for i, r in enumerate(rules)]
    text = json.dumps(validated, ensure_ascii=True, separators=(",", ":"))
    # The .env format has no escapes. JSON syntax never uses a single quote, so
    # encoding every one as \u0027 keeps the whole value free of a literal
    # apostrophe; it can then live inside a single-quoted .env value even when
    # a rule's reason carries both quote kinds. serde_json reads it back
    # verbatim.
    return text.replace("'", "\\u0027")


path = sys.argv[1]
try:
    with open(path, "rb") as fh:
        doc = tomllib.load(fh)
except Exception as e:
    fail(f"无法解析 TOML: {e}")

if not isinstance(doc, dict):
    fail("顶层必须是表")
extra = set(doc) - {"version", "defaults", "accounts"}
if extra:
    fail(f"顶层含不支持的键: {', '.join(sorted(extra))}")
if "version" not in doc:
    fail("缺少 version（内核只讲 version = 1）")
version = doc["version"]
if not isinstance(version, int) or isinstance(version, bool) or version != 1:
    fail(f"version={version!r} 不受支持（内核讲 version = 1）")

pairs = []

defaults = doc.get("defaults", {})
if not isinstance(defaults, dict):
    fail("defaults 必须是表")
extra = set(defaults) - (set(SCALARS) | {"rules"})
if extra:
    fail(f"[defaults] 含不支持的键: {', '.join(sorted(extra))}")
pairs.extend(scalar_pairs(defaults, "[defaults]"))
if "rules" in defaults:
    pairs.append(("RULES", rules_json(defaults["rules"], "[defaults].rules")))

accounts = doc.get("accounts", {})
if not isinstance(accounts, dict):
    fail("accounts 必须是表")
# The ids ACCOUNT_LIST will carry after this migration. Guarded per section,
# not silently skipped: a section for an unknown id would migrate to keys the
# kernel never reads (it discovers env-only policy sections from ACCOUNT_LIST),
# so that account would quietly inherit the GLOBAL policy — the relaxation
# #760 exists to prevent.
known_ids = set()
if len(sys.argv) > 2 and sys.argv[2].strip():
    known_ids = {x.strip() for x in sys.argv[2].split(",") if x.strip()}
seen = {}
for account_id, section in accounts.items():
    if not isinstance(account_id, str) or not account_id:
        fail("账户 id 必须是非空字符串")
    if account_id not in known_ids:
        fail(
            f"[accounts.{account_id}] 的 id 不在迁移后的 ACCOUNT_LIST 里"
            f"（当前: {', '.join(sorted(known_ids)) or '空'}）——内核只为 ACCOUNT_LIST "
            "里的账户注册 policy 段，该段会静默回落到全局策略；请先在账户源里登记该 "
            "id（或从 policy 里删掉该段）再迁移"
        )
    sfx = env_suffix(account_id)
    if not sfx:
        fail(f"账户 id {account_id!r} 的 env 后缀为空")
    if sfx in seen:
        fail(f"账户 id {seen[sfx]!r} 与 {account_id!r} 折成同一个 env 后缀 {sfx}")
    seen[sfx] = account_id
    if not isinstance(section, dict):
        fail(f"[accounts.{account_id}] 必须是表")
    extra = set(section) - (set(SCALARS) | {"rules"})
    if extra:
        fail(f"[accounts.{account_id}] 含不支持的键: {', '.join(sorted(extra))}")
    for tail, val in scalar_pairs(section, f"[accounts.{account_id}]"):
        pairs.append((f"{tail}_{sfx}", val))
    if "rules" in section:
        pairs.append((f"RULES_{sfx}", rules_json(section["rules"], f"[accounts.{account_id}].rules")))

for key, val in pairs:
    print(f"POLICY_{key}\t{val}")
PY
)" || rc=$?
  if [ "$rc" -ne 0 ]; then
    if [ -s "$err" ]; then
      sed 's/^/  ! /' "$err" >&2
    fi
    die "execution_policy.toml 无法忠实迁移（退出码 $rc）——源文件与 .env 均未改动"
  fi
  if [ -n "$out" ]; then
    local key val
    while IFS=$'\t' read -r key val; do
      [ -n "$key" ] || continue
      emit "$key" "$val"
    done <<EOF
$out
EOF
  fi
  commit "$f"
}

# ── data/runtime/config.json ─────────────────────────────────────────────────
# Only the roundDurationSec cell migrates (RUNTIME_ROUND_DURATION_SEC); the
# credentials cell, if present, is drained by the kernel's own boot-time
# one-shot — never by this script, whose code path never reads its values.
migrate_runtime_json() {
  local f="$DATA_DIR/runtime/config.json"
  if [ ! -f "$f" ]; then
    SKIPPED="${SKIPPED}data/runtime/config.json"$'\n'
    return 0
  fi
  ANY_SOURCE=1
  say "── data/runtime/config.json"
  local out rc=0
  out="$(python3 -c 'import json, sys
try:
    with open(sys.argv[1], "r") as fh:
        doc = json.load(fh)
except Exception:
    sys.exit(0)
v = doc.get("roundDurationSec") if isinstance(doc, dict) else None
if v is None:
    sys.exit(0)
if isinstance(v, bool) or not isinstance(v, int):
    sys.exit(3)
print(v)' "$f" 2>/dev/null)" || rc=$?
  case "$rc" in
    0)
      if [ -n "$out" ]; then
        emit "RUNTIME_ROUND_DURATION_SEC" "$out"
      else
        warn "config.json 没有 roundDurationSec——无键写入，文件仍退役进备份"
      fi
      commit "$f" ;;
    3)
      warn "roundDurationSec 不是整数——该键不迁移，请手动核对"
      add_note "data/runtime/config.json 的 roundDurationSec 不是整数，未迁移：手动把 RUNTIME_ROUND_DURATION_SEC=<秒数> 写进 .env。"
      commit "$f" ;;
    *)
      warn "config.json 不可读/非 JSON——该源不动（fail-closed）"
      LEFT_BEHIND="${LEFT_BEHIND}${f}"$'\n' ;;
  esac
}

# ── user_layer/configs/update.toml ─────────────────────────────────────────────
# [update] section → BLITZKRIEG_UPDATE_* env vars.
migrate_update_toml() {
  local f="$CONFIG_DIR/update.toml"
  if [ ! -f "$f" ]; then
    SKIPPED="${SKIPPED}user_layer/configs/update.toml"$'\n'
    return 0
  fi
  ANY_SOURCE=1
  say "── user_layer/configs/update.toml"
  local ev="$WORKDIR/update.events"
  scan_toml "$f" > "$ev"

  local tag p1 p2 p3 sec="" v_check="" v_auto="" v_interval=""
  while IFS=$'\t' read -r tag p1 p2 p3; do
    case "$tag" in
      T) sec="$p1" ;;
      K)
        if [ "$sec" != "update" ]; then continue; fi
        case "$p1" in
          check_enabled)
            case "$p3" in
              true) v_check="true" ;;
              false) v_check="false" ;;
              *) warn "update.check_enabled 不是布尔——该键不迁移" ;;
            esac ;;
          auto_update)
            case "$p3" in
              true) v_auto="true" ;;
              false) v_auto="false" ;;
              *) warn "update.auto_update 不是布尔——该键不迁移" ;;
            esac ;;
          interval_secs)
            if [ "$p2" = "b" ] || [ "$p2" = "s" ]; then
              v_interval="$p3"
            else
              warn "update.interval_secs 不是整数——该键不迁移"
            fi ;;
        esac ;;
    esac
  done < "$ev"
  if [ -n "$v_check" ] || [ -n "$v_auto" ] || [ -n "$v_interval" ]; then
    emit "BLITZKRIEG_UPDATE_CHECK" "$v_check"
    emit "BLITZKRIEG_UPDATE_AUTO" "$v_auto"
    emit "BLITZKRIEG_UPDATE_INTERVAL_SECS" "$v_interval"
    commit "$f"
  else
    warn "update.toml 没有可迁移的键值"
    abandon "$f"
  fi
}

# ── backup move ──────────────────────────────────────────────────────────────
ensure_backup_dir() {
  [ -n "$BACKUP_DIR" ] && return 0
  local base cand n
  base="$DATA_DIR/config-backup-$(date +%Y%m%d-%H%M%S)"
  cand="$base"
  n=2
  while [ -e "$cand" ]; do
    cand="$base-$n"
    n=$((n + 1))
  done
  mkdir -p "$cand" || return 1
  chmod 700 "$cand" || return 1
  BACKUP_DIR="$cand"
}

move_to_backup() {
  local src="$1" name
  [ -f "$src" ] || return 0
  if ! ensure_backup_dir; then
    printf '  ! 无法创建备份目录，源文件保留: %s\n' "$src" >&2
    return 1
  fi
  name="$(basename "$src")"
  while [ -e "$BACKUP_DIR/$name" ]; do
    name="${name}.bak"
  done
  if ! mv "$src" "$BACKUP_DIR/$name"; then
    printf '  ! 退役失败（.env 已写入且一致；源文件保留）: %s\n' "$src" >&2
    return 1
  fi
  MOVED_LIST="${MOVED_LIST}  ${src#"$ROOT"/} → ${BACKUP_DIR#"$ROOT"/}/$name"$'\n'
}

# ── .env merge (read-merge-atomic-write) ─────────────────────────────────────
# One awk pass: every line whose key is being migrated is replaced by the
# rendered line (first occurrence wins; later duplicates of the same key are
# dropped); every other line (comments, credentials, unknown keys) passes
# through byte-for-byte; keys not yet in the file append at the end. Then
# rename into place + 0600.
merge_env_file() {
  local env_dir tmp summary
  summary="$WORKDIR/merge.summary"
  env_dir="$(dirname "$ENV_FILE")"
  mkdir -p "$env_dir"
  tmp="$(mktemp "$env_dir/.env.migrate.XXXXXX")"
  awk -v summary="$summary" '
    BEGIN { replaced = 0; appended = 0; dropped = 0 }
    NR == FNR {
      # The merge input carries one rendered line per key: KEY="value".
      if (!match($0, /^[A-Za-z_][A-Za-z0-9_]*=/)) {
        print "migrate-config-to-env: 内部错误：渲染行缺少 KEY= 前缀，已忽略" > "/dev/stderr"
        next
      }
      key = substr($0, 1, RLENGTH - 1)
      if (key in newline) {
        printf("migrate-config-to-env: 键 %s 渲染了两次（两个 id 折到同一 env 后缀？）——保留第一个\n", key) > "/dev/stderr"
        next
      }
      newline[key] = $0
      order[++n] = key
      next
    }
    {
      probe = $0
      sub(/^[ \t]+/, "", probe)
      sub(/^export[ \t]/, "", probe)
      if (match(probe, /^[A-Za-z_][A-Za-z0-9_]*=/)) {
        k = substr(probe, 1, RLENGTH - 1)
        if (k in newline) {
          if (k in done) {
            dropped++
            printf("migrate-config-to-env: .env 里 %s 有重复行——多余的行已删除\n", k) > "/dev/stderr"
          } else {
            print newline[k]
            done[k] = 1
            replaced++
          }
          next
        }
      }
      print
      next
    }
    END {
      for (j = 1; j <= n; j++) {
        if (!(order[j] in done)) {
          print newline[order[j]]
          appended++
        }
      }
      printf("%d %d %d\n", replaced, appended, dropped) > summary
    }
  ' "$VARS_FILE" "$ENV_FILE" > "$tmp"
  mv "$tmp" "$ENV_FILE"
  chmod 600 "$ENV_FILE"
}

# Verify that every key this run rendered landed in the .env exactly as
# rendered. The merge keeps the FIRST rendered line per key and drops later
# duplicates, so the check walks the first occurrence per key. A failure means
# the atomic write did not round-trip — refuse to retire any source.
verify_env_merge() {
  [ -s "$VARS_FILE" ] || return 0
  local line
  awk '{ k = $0; sub(/=.*/, "", k); if (!(k in seen)) { seen[k] = 1; print } }' \
    "$VARS_FILE" > "$WORKDIR/verify.lines"
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    if ! grep -Fqx -- "$line" "$ENV_FILE"; then
      die "迁移校验失败：.env 中找不到本次写入的键行（键名与值均不打印）"
    fi
  done < "$WORKDIR/verify.lines"
}

# ── run ──────────────────────────────────────────────────────────────────────
say "=== .env 单源配置迁移（issue #751）==="
say "root: $ROOT"
say ""

migrate_accounts_toml
migrate_shadow_toml
migrate_policy_toml
migrate_runtime_json
migrate_update_toml

say ""
if [ "$ANY_SOURCE" = 0 ]; then
  say "五个迁移源都不存在——本次为 no-op（配置已迁移过，或本目录从无这些文件）。"
fi

# Single atomic .env write, only when there is something to write. The write
# happens BEFORE any source is retired: a parse/refusal leaves every source in
# place, and a failed write never loses a source.
if [ -s "$VARS_FILE" ]; then
  if [ ! -f "$ENV_FILE" ]; then
    : > "$ENV_FILE"
  fi
  merge_env_file
  verify_env_merge
  read -r REPLACED APPENDED DROPPED < "$WORKDIR/merge.summary"
  say ""
  say ".env 更新完成：替换 ${REPLACED} 行，追加 ${APPENDED} 行，删除重复 ${DROPPED} 行（权限 0600；未涉及行逐字节保留）。"
fi

# Retire the sources (move = backup + exit the workspace in one step) only
# after the target is written and verified. A move failure leaves the .env
# consistent and the source in place; the non-zero exit asks for a re-run.
MOVE_FAILED=0
if [ -n "$FILES_TO_MOVE" ]; then
  say "备份并退役源文件（move，不 rm）……"
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if ! move_to_backup "$p"; then
      MOVE_FAILED=1
    fi
  done <<EOF
$FILES_TO_MOVE
EOF
  if [ "$MOVE_FAILED" = 0 ]; then
    say "备份目录: ${BACKUP_DIR#"$ROOT"/}"
    printf '%s' "$MOVED_LIST"
  fi
fi

say ""
say "=== 迁移报告（只打印键名，绝不打印值）==="
if [ -n "$SKIPPED" ]; then
  say "不存在的源（跳过）:"
  printf '%s' "$SKIPPED" | sed 's/^/  - /'
fi
if [ -n "$LEFT_BEHIND" ]; then
  say "未迁移的源（解析问题，文件原位保留）:"
  printf '%s' "$LEFT_BEHIND" | sed 's/^/  - /'
fi
if [ -n "$NOTES" ]; then
  say ""
  say "手动收尾事项:"
  printf '%s' "$NOTES" | sed 's/^/  /'
fi
say ""
say "polysim 提醒：若运行簿里有 accounts.toml 未记录的账户（例如 polysimulator，或面板上创建的账户），请手动把 id 追加进 ACCOUNT_LIST 并补 ACCOUNT_<ID>_* 键。"
say "迁移后 .env 是唯一配置源：内核在 ACCOUNT_* 生效时拒绝与遗留 toml 并存（loader fail-closed），请以备份目录为准核对后删除残留文件。"

if [ "$MOVE_FAILED" = 1 ]; then
  say ""
  say "!! 部分源文件未能退役：.env 已写入并保持一致，源文件仍在原位。请修复后重跑本脚本（幂等，不会重复写键）。" >&2
  exit 1
fi
