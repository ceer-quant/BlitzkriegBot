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
#     → POLICY_BUDGET_RATIO / POLICY_MIN_BUDGET_USD / POLICY_MAX_BUDGET_USD /
#       POLICY_MIN_EQUITY_USD / POLICY_MAX_POSITIONS_PER_ASSET, global and
#       per-account (POLICY_<ID>_<TAIL>, same suffix spelling). The [[*.rules]]
#       array-of-tables is NOT parsed here — the script prints a hand-migration
#       recipe for POLICY_RULES / POLICY_<ID>_RULES instead.
#       (authority: core/blitzkrieg_core/src/execution_policy.rs)
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
#     (fail-closed, the same discipline the kernel's loaders run).
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
# [defaults] / [accounts.<id>] scalars → POLICY_* keys. The [[*.rules]]
# array-of-tables is NOT parsed — POLICY_RULES / POLICY_<ID>_RULES get a
# hand-migration recipe instead (fail-safe: rules only ever SHRINK intents,
# and silently dropping them would silently un-guard the kernel).
migrate_policy_toml() {
  local f="$CONFIG_DIR/execution_policy.toml"
  if [ ! -f "$f" ]; then
    SKIPPED="${SKIPPED}user_layer/configs/execution_policy.toml"$'\n'
    return 0
  fi
  ANY_SOURCE=1
  say "── user_layer/configs/execution_policy.toml"
  local ev="$WORKDIR/policy.events"
  scan_toml "$f" > "$ev"

  local tag p1 p2 p3 sec="" mode="none" acc_id="" acc_sfx="" rules_global=0 rules_ids=""

  # mode: none (top-level) / defaults / account / skip (unmappable id) / rules
  while IFS=$'\t' read -r tag p1 p2 p3; do
    case "$tag" in
      T)
        sec="$p1"
        case "$sec" in
          defaults)
            mode="defaults" ;;
          accounts.*)
            case "$sec" in
              *#arr)
                # [[accounts.<id>.rules]] — the id sits between accounts. and .rules
                acc_id="${sec#accounts.}"
                acc_id="${acc_id%%.rules#*}"
                if [ -n "$acc_id" ] && [ "$acc_id" != "$sec" ] && [ "$acc_id" != "${sec#accounts.}" ]; then
                  rules_ids="$rules_ids $acc_id"
                fi
                mode="rules" ;;
              *)
                acc_id="${sec#accounts.}"
                case "$acc_id" in
                  *'"'* | *"'"* | *.* | *" "*)
                    warn "[accounts.$acc_id] 的 id 含引号/点/空格——env 后缀文法无法表达，该段请手动迁移"
                    mode="skip" ;;
                  *)
                    acc_sfx="$(env_suffix "$acc_id")"
                    mode="account" ;;
                esac ;;
            esac ;;
          defaults.rules*)
            rules_global=1
            mode="rules" ;;
          *)
            mode="none" ;;
        esac ;;
      K)
        case "$mode" in
          none)
            case "$p1" in
              version)
                if [ "$p3" != "1" ]; then
                  warn "execution_policy.toml version=$p3 而本内核讲 version=1——照迁标量（该文件本身会被内核拒绝启动）"
                fi ;;
            esac ;;
          defaults)
            case "$p1" in
              budget_ratio)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_BUDGET_RATIO" "$p3"; fi ;;
              min_budget_usd)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_MIN_BUDGET_USD" "$p3"; fi ;;
              max_budget_usd)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_MAX_BUDGET_USD" "$p3"; fi ;;
              min_equity_usd)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_MIN_EQUITY_USD" "$p3"; fi ;;
              max_positions_per_asset)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_MAX_POSITIONS_PER_ASSET" "$p3"; fi ;;
            esac ;;
          account)
            case "$p1" in
              budget_ratio)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_${acc_sfx}_BUDGET_RATIO" "$p3"; fi ;;
              min_budget_usd)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_${acc_sfx}_MIN_BUDGET_USD" "$p3"; fi ;;
              max_budget_usd)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_${acc_sfx}_MAX_BUDGET_USD" "$p3"; fi ;;
              min_equity_usd)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_${acc_sfx}_MIN_EQUITY_USD" "$p3"; fi ;;
              max_positions_per_asset)
                if [ "$p2" = "s" ] || [ "$p2" = "b" ]; then emit "POLICY_${acc_sfx}_MAX_POSITIONS_PER_ASSET" "$p3"; fi ;;
            esac ;;
        esac ;;
    esac
  done < "$ev"
  if [ "$rules_global" = 1 ] || [ -n "$rules_ids" ]; then
    local per=""
    if [ -n "$rules_ids" ]; then
      per="（带 [[accounts.<id>.rules]] 的账户写 POLICY_<ID>_RULES，本文件涉及的 id:${rules_ids} ）"
    fi
    add_note "POLICY_RULES 请手动迁移：TOML 的 [[*.rules]] 数组表不由本脚本解析。打开备份里的 execution_policy.toml，把每个规则表（name/enabled/priority/when/then/reason 原样）写成 JSON 数组字符串 upsert 进 .env，例如 POLICY_RULES='[{\"name\":\"global_backstop\",\"enabled\":true,\"priority\":100,\"when\":\"open_positions >= 2\",\"then\":{\"action\":\"skip\"},\"reason\":\"…\"}]'${per}"
  fi
  if [ -n "$PENDING" ] || [ "$rules_global" = 1 ] || [ -n "$rules_ids" ]; then
    commit "$f"
  else
    warn "execution_policy.toml 没有可迁移的标量"
    abandon "$f"
  fi
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
  mkdir -p "$cand"
  chmod 700 "$cand"
  BACKUP_DIR="$cand"
}

move_to_backup() {
  local src="$1" name
  [ -f "$src" ] || return 0
  ensure_backup_dir
  name="$(basename "$src")"
  while [ -e "$BACKUP_DIR/$name" ]; do
    name="${name}.bak"
  done
  mv "$src" "$BACKUP_DIR/$name"
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

# Move the retired sources (move = backup + exit the workspace in one step).
if [ -n "$FILES_TO_MOVE" ]; then
  say "备份并退役源文件（move，不 rm）……"
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    move_to_backup "$p"
  done <<EOF
$FILES_TO_MOVE
EOF
  say "备份目录: ${BACKUP_DIR#"$ROOT"/}"
  printf '%s' "$MOVED_LIST"
fi

# Single atomic .env write, only when there is something to write.
if [ -s "$VARS_FILE" ]; then
  if [ ! -f "$ENV_FILE" ]; then
    : > "$ENV_FILE"
  fi
  merge_env_file
  read -r REPLACED APPENDED DROPPED < "$WORKDIR/merge.summary"
  say ""
  say ".env 更新完成：替换 ${REPLACED} 行，追加 ${APPENDED} 行，删除重复 ${DROPPED} 行（权限 0600；未涉及行逐字节保留）。"
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
