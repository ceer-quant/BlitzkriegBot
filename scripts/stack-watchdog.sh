#!/usr/bin/env bash
# stack-watchdog.sh — 交易内核的心跳检查与「停机」告警（issue #211 的 B 路线）。
#
# 2026-09-21 机器崩溃重启后，内核与面板都没有起来，而**没有任何告警**：只有
# `com.blitzkrieg.databackup.*` 两个备份 job 出现在 `launchctl list` 里。没人看
# 面板，就永远不知道栈已经停了。本脚本补的就是这一半：让「停机」自己喊出来。
#
# 分工（硬约定，写在最前面）：
#   本脚本的职责是**判定 + 告警**，不是拉起。
#   - 默认**绝不自动拉起内核**；
#   - 只有显式给出 `--autostart`（或 BK_AUTOSTART=1）**并且**显式给出
#     `BK_AUTOSTART_CMD` 时才会尝试拉起（两把钥匙，缺一不动）；
#   - **live 模式下永远拒绝自动拉起**，开关打开也一样。理由是既有约定：
#     live 只在用户在场时开。这条是代码里的硬门禁，不是注释里的君子协定。
#   - 本脚本**只读** `.env`：永不写 .env、永不设置或修改 DRY_RUN 的值。
#     「谁都不能代改 DRY_RUN」是项目红线，看门狗更不该是例外。
#
# 存活判定复用 `scripts/soak-resident.sh` 的 alive() 语义（pidfile + kill -0 +
# 命令行匹配），不另发明一套：同样的三件事回答同一个问题，两套判定迟早会给出
# 两个答案。区别只有一处：内核自己不写 pidfile，所以未配置 --pidfile 时用
# `pgrep -f` 发现候选 pid，再用同一套语义（kill -0 + 命令行匹配）复核。
#
# UDS 探针：进程在 ≠ 服务在。soak-health.sh 用 `core.ping` 抓「活着但卡死」；
# 这里做更轻的一步——只 connect（与 ui_kit 的 `socket_served()` 同语义），连不上
# 即算异常，并且把两种形态在输出里分开写：
#   「进程不在」    —— 找不到内核进程
#   「进程在但 socket 不通」 —— 进程在，但 UDS 连不上（启动中 / 卡死 / 抢走了名字）
# 内核进程不在时残留的 socket 节点是**正常现象**（内核启动时先 unlink 再 bind，
# 见 ipc/server.rs），所以它只作附注，绝不当成「还活着」的证据。
#
# 去抖：只在状态翻转（up→down / down→up）时发告警；持续 down 期间最多每
# REPEAT_SEC（默认 900 = 15 分钟）重复一次。每分钟一条「内核不在跑」的刷屏和
# 一条都不发是同一个缺陷——信号都会消失。
#
# 告警去哪里：本地日志（$STATE_DIR/watchdog.log，正在 down 时另有 STACK_DOWN
# 标记文件）+ macOS 本地通知（osascript `display notification`）。
# **没有任何出站网络请求**（面板只绑回环，这是本项目的既有约束）。
#
# ── launchd 的现实（决定「重启后能不能自动跑」，务必读完）────────────────────
# 本仓库位于外置卷（/Volumes/Hard Disk），macOS 拒绝 launchd 拉起的进程读写/执行
# 该卷上的文件：同目录的 databackup agent 现在就在以
#   `/bin/sh: /Volumes/.../data-backup-cli.sh: Operation not permitted`
# 失败（证据：~/Library/Logs/blitzkrieg-data-backup-light.log，2026-09-21 04:00）。
# 所以「谁来在重启后跑这个脚本」是用户的选择，不是脚本能替用户做的决定：
#   (a) 给解释器授予「完全磁盘访问权限」（安全姿态变更，**用户拍板**）；
#   (b) 把本脚本复制到内置盘运行（用 BK_REPO_ROOT 指回本仓库）——不改安全姿态。
#       该副本读不到 .env / data/（模式与敞口显示「无法判定（权限被拒）」），但
#       停机判定与告警仍然成立：pgrep 与 $TMPDIR 下的 socket 都在受保护卷之外；
#   (c) 把检出搬到内置盘。
# 三选一的具体做法、plist 里的本机路径、以及「重启后谁跑它」见 scripts/README.md 与
# README.md §3.5；本目录里附了 scripts/com.blitzkrieg.stack-watchdog.plist（StartInterval=60
# + RunAtLoad，**未安装**）。
#
# 用法:
#   scripts/stack-watchdog.sh [选项]
#     （无选项）        执行一次心跳检查（默认）
#     --status          只打印状态摘要：不告警、不写状态、不自动拉起
#     --self-test       用临时目录 fixture 自测判定/去抖/门禁（**不需要真内核**）
#     --autostart       允许在 dry 模式下自动拉起（默认关闭；还需 BK_AUTOSTART_CMD）
#     --no-autostart    显式关闭自动拉起（默认）
#     --repeat-sec N    down 持续期间重复告警的最小间隔秒数（默认 900）
#     --state-dir DIR   状态与日志目录（默认 ~/Library/Logs/blitzkrieg-stack-watchdog）
#     --pidfile PATH    内核 pidfile（可选；不给则 pgrep 发现）
#     --socket PATH     内核 UDS 路径（默认从内核 argv 推导，再退回 $TMPDIR 约定）
#     --no-notify       不发 macOS 通知（日志与 STACK_DOWN 标记照常）
#     --quiet           只在异常（告警）时输出到 stdout
#     -h | --help
#
# 退出码: 0 = 内核存活并在服务；1 = 内核不在跑 / socket 不可达（已告警）；2 = 用法或配置错误
#
# 环境变量（全部可选，用于部署与测试注入；与 soak-health.sh 的 seam 风格一致）:
#   BK_REPO_ROOT        仓库根（默认由脚本位置推导；内置盘副本部署时必填）
#   BK_CORE_PGREP       内核进程匹配串（默认 target/release/blitzkrieg-core）
#   BK_CORE_PIDFILE     内核 pidfile（默认无：内核不写 pidfile）
#   BK_SOCKET           内核 UDS 路径（默认从内核 argv 推导）
#   BK_WATCHDOG_DIR     状态/日志目录
#   BK_REPEAT_SEC       重复告警间隔（默认 900）
#   BK_ENV_FILE         .env 路径（默认 <repo>/.env，**只读**）
#   BK_POSITIONS        未平仓快照路径（默认 data/positions/positions.jsonl）
#   BK_SETTLEMENTS      结算日志路径（默认 data/orders/settlements.jsonl）
#   BK_DATA_MTIME_HINT  无存活记录时用来推断「最后已知存活」的候选文件（冒号分隔）
#   BK_AUTOSTART        1 = 允许自动拉起（默认 0；live 仍然拒绝）
#   BK_AUTOSTART_CMD    自动拉起命令（未设置则不动手；由部署方显式指定）
#   BK_NOTIFY           0 = 不发 macOS 通知（默认 1）
#   BK_SILENCE          1 = 静默（等价于存在 $STATE_DIR/silence）：只抑制通知，
#                       状态、STACK_DOWN 标记与退出码照常
#
# 运维开关：停机告警是给「没人看着」准备的。若这次停机是你自己干的（`blitzkrieg
# stop`），先 `touch $STATE_DIR/silence` 再停，起来后删掉即可——否则你会收到一条
# 完全正确的告警。
#
# 自测: scripts/stack-watchdog.sh --self-test   （CI 可直接跑，零外部依赖）

set -uo pipefail

SELF="${BASH_SOURCE[0]}"
SCRIPT_DIR="$(cd "$(dirname "$SELF")" && pwd)"
ROOT="${BK_REPO_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"

CHECK_MODE=check
QUIET=0
REPEAT_SEC="${BK_REPEAT_SEC:-900}"
STATE_DIR="${BK_WATCHDOG_DIR:-${HOME:-/tmp}/Library/Logs/blitzkrieg-stack-watchdog}"
PIDFILE_CFG="${BK_CORE_PIDFILE:-}"
SOCKET_CFG="${BK_SOCKET:-}"
CORE_PGREP="${BK_CORE_PGREP:-target/release/blitzkrieg-core}"
ENV_FILE_CFG="${BK_ENV_FILE:-}"
POSITIONS_CFG="${BK_POSITIONS:-}"
SETTLEMENTS_CFG="${BK_SETTLEMENTS:-}"
AUTOSTART="${BK_AUTOSTART:-0}"
AUTOSTART_CMD="${BK_AUTOSTART_CMD:-}"
NOTIFY="${BK_NOTIFY:-1}"
SILENCE="${BK_SILENCE:-0}"

usage() {
  sed -n 's/^# \{0,1\}//p' <<'HDR' >&2
# 用法: scripts/stack-watchdog.sh [--status|--self-test|--autostart|--no-autostart]
#        [--repeat-sec N] [--state-dir DIR] [--pidfile PATH] [--socket PATH]
#        [--no-notify] [--quiet]
# 退出码: 0 = 内核存活；1 = 内核不在跑 / socket 不可达（已告警）；2 = 用法或配置错误
HDR
}

# ── 参数 ────────────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
  case "$1" in
    --status)        CHECK_MODE=status ;;
    --self-test)     CHECK_MODE=self-test ;;
    --autostart)     AUTOSTART=1 ;;
    --no-autostart)  AUTOSTART=0 ;;
    --repeat-sec)    REPEAT_SEC="${2:-}"; shift ;;
    --state-dir)     STATE_DIR="${2:-}"; shift ;;
    --pidfile)       PIDFILE_CFG="${2:-}"; shift ;;
    --socket)        SOCKET_CFG="${2:-}"; shift ;;
    --no-notify)     NOTIFY=0 ;;
    --quiet)         QUIET=1 ;;
    -h|--help)       usage; exit 0 ;;
    *) echo "stack-watchdog: 未知参数: $1" >&2; usage; exit 2 ;;
  esac
  shift
done

case "$REPEAT_SEC" in
  ''|*[!0-9]*) echo "stack-watchdog: --repeat-sec 需要非负整数，得到 '$REPEAT_SEC'" >&2; exit 2 ;;
esac
[ -n "$STATE_DIR" ] || { echo "stack-watchdog: --state-dir 不能为空" >&2; exit 2; }
[ -f "$ROOT/Cargo.toml" ] || { echo "stack-watchdog: $ROOT 不是 BlitzkriegBot 仓库根（缺 Cargo.toml）" >&2; exit 2; }

LOG="$STATE_DIR/watchdog.log"
STATE_FILE="$STATE_DIR/state"
DOWN_MARKER="$STATE_DIR/STACK_DOWN"

# 只有真正的一次检查（check）才需要可写的状态目录：--status 是只读的（连目录都不建），
# --self-test 只用 mktemp 的临时目录。自测/状态查询不该在用户 HOME 里留下任何东西
# （更不碰生产 data/）。
if [ "$CHECK_MODE" = "check" ]; then
  mkdir -p "$STATE_DIR" 2>/dev/null || { echo "stack-watchdog: 状态目录不可创建: $STATE_DIR" >&2; exit 2; }
  [ -w "$STATE_DIR" ] || { echo "stack-watchdog: 状态目录不可写: $STATE_DIR" >&2; exit 2; }
fi

# ── 基础工具 ────────────────────────────────────────────────────────────────
log_append() {
  printf '%s\n' "$1" >> "$LOG" 2>/dev/null || true
}
say() { # 正常状态的输出；--quiet 下闭嘴。异常输出一律走 printf，不受 --quiet 影响
  [ "$QUIET" -eq 1 ] && return 0
  printf '%s\n' "$1"
}
# 时间戳用 python3，而不是 `date -r`（BSD 专有）/ `date -d @`（GNU 专有）：本脚本
# 要在 macOS 与 Ubuntu CI 上给出同一个格式。
fmt_epoch() {
  [ -n "${1:-}" ] || { printf '%s' "未知"; return 0; }
  python3 -c 'import sys,time
try: print(time.strftime("%Y-%m-%d %H:%M:%S %z", time.localtime(float(sys.argv[1]))))
except Exception: print("未知")' "$1" 2>/dev/null || printf '%s' "未知"
}
now_epoch() { date +%s; }

# ── 存活判定（语义照抄 scripts/soak-resident.sh 的 alive()）──────────────────
# alive <pidfile> <pattern>：pidfile 在 + pid 非空 + kill -0 活着 + 命令行匹配。
# 命令行匹配不是洁癖：重启/崩溃后 pidfile 常常陈旧，而 pid 被回收时 `kill -0`
# 会把一个陌生人报成「内核在跑」。
alive() {
  local pf=$1 pat=$2 pid
  [ -f "$pf" ] || return 1
  pid=$(cat "$pf" 2>/dev/null) || return 1
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  ps -o command= -p "$pid" 2>/dev/null | grep -q "$pat" || return 1
  return 0
}
# 同一套复核，作用于「已发现的 pid」（内核不写 pidfile，候选来自 pgrep）。
pid_is_core() {
  local pid=$1
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  ps -o command= -p "$pid" 2>/dev/null | grep -q "$CORE_PGREP" || return 1
  return 0
}

# ── socket 路径 ─────────────────────────────────────────────────────────────
# 与内核 socket_path_for() 一致：${TMPDIR:-/tmp}/blitzkrieg-core-$USER.sock。
default_socket() {
  local p="${TMPDIR:-/tmp}/blitzkrieg-core-${USER:-user}.sock"
  printf '%s' "$p" | sed 's|//|/|g'
}
# 从内核 argv 取 --socket：argv 才是权威（部署可以用 --socket 覆盖；在这里再抄一份
# socket 命名契约，就会在覆盖的那一刻开始说谎）。
argv_socket() {
  [ -n "${CORE_ARGV:-}" ] || return 1
  printf '%s\n' "$CORE_ARGV" | tr ' ' '\n' | grep -A1 '^--socket$' | tail -1
}
# connect-only 探针，与 ui_kit 的 `socket_served()` 同语义。打印 ok 或失败原因。
socket_probe() {
  local path=$1
  [ -n "$path" ] || { printf '%s' "路径未知"; return 0; }
  [ -e "$path" ] || { printf '%s' "节点不存在"; return 0; }
  python3 - "$path" <<'PY' 2>/dev/null || printf '%s' "探针失败"
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(3)
try:
    s.connect(sys.argv[1])
    print("ok")
except PermissionError:
    print("权限被拒")
except OSError as e:
    print(type(e).__name__)
finally:
    s.close()
PY
}

# ── 模式（dry / live）：只读 .env，绝不写 ──────────────────────────────────
# 优先级与内核一致：argv(--mode) > 环境变量(DRY_RUN) > .env > 默认 dry。
# `--readonly` 单列：它不是 dry/live，而是「结构性禁止出网下单」，在内核里压过
# --mode live；告警文案必须分开说，否则运维会以为 live 自己在跑。
MODE_VALUE="未知"
MODE_SOURCE="未判定"
read_mode() {
  local env_file=$1 m v
  if [ -n "${CORE_ARGV:-}" ]; then
    case "$CORE_ARGV" in
      *--readonly*) MODE_VALUE="readonly"; MODE_SOURCE="内核 argv 的 --readonly"; return 0 ;;
    esac
    m=$(printf '%s\n' "$CORE_ARGV" | tr ' ' '\n' | grep -A1 '^--mode$' | tail -1)
    if [ -n "$m" ]; then
      MODE_VALUE="$m"; MODE_SOURCE="内核 argv 的 --mode"; return 0
    fi
  fi
  if [ -n "${DRY_RUN:-}" ]; then
    case "$DRY_RUN" in
      false|False|FALSE|0|no) MODE_VALUE="live" ;;
      *) MODE_VALUE="dry" ;;
    esac
    MODE_SOURCE="环境变量 DRY_RUN=$DRY_RUN"
    return 0
  fi
  if [ -f "$env_file" ]; then
    # 只取 DRY_RUN 这一行的值；.env 里还有凭证，任何值都不打印。
    v=$(sed -n 's/^[[:space:]]*export[[:space:]]\{1,\}//; s/^[[:space:]]*DRY_RUN[[:space:]]*=[[:space:]]*//p' "$env_file" 2>/dev/null | head -1)
    v=$(printf '%s' "$v" | tr -d '"\047' | tr -d '[:space:]')
    if [ -n "$v" ]; then
      case "$v" in
        false|False|FALSE|0|no) MODE_VALUE="live" ;;
        *) MODE_VALUE="dry" ;;
      esac
      MODE_SOURCE="${env_file}（只读）"
    else
      MODE_VALUE="dry"; MODE_SOURCE="$env_file 未配置 DRY_RUN（内核默认 dry）"
    fi
    return 0
  fi
  MODE_VALUE="无法判定"; MODE_SOURCE="$env_file 不存在且环境变量 DRY_RUN 未设置"
}

# ── 风险面：未平仓 / 未赎回应收 ─────────────────────────────────────────────
# 输出 `OK|<文案>` 或 `UNKNOWN|<原因>`。读不到就说读不到（「无法判定」），绝不假装
# 0——0 会让运维以为没有敞口，而那是事故里最贵的一句话。
positions_summary() {
  local f=$1
  [ -n "$f" ] || { printf '%s' "UNKNOWN|未配置路径"; return 0; }
  [ -e "$f" ] || { printf 'UNKNOWN|%s 不存在' "$f"; return 0; }
  python3 - "$f" <<'PY' 2>/dev/null || printf '%s' "UNKNOWN|解析失败"
import json, sys
try:
    n, cost, brief = 0, 0.0, []
    for line in open(sys.argv[1], errors="replace"):
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except Exception:
            continue
        n += 1
        try:
            cost += float(r.get("costUsd") or 0)
        except Exception:
            pass
        if len(brief) < 3:
            brief.append("%s %s %s 股" % (r.get("asset") or "?", r.get("direction") or "?", r.get("shares") or "?"))
    extra = "" if n <= len(brief) else "；+%d 笔" % (n - len(brief))
    tail = ("（" + "；".join(brief) + extra + "）") if brief else ""
    print("OK|%d 笔，成本合计 $%.2f%s" % (n, cost, tail))
except PermissionError:
    print("UNKNOWN|权限被拒（launchd/TCC 读不到外置卷；见 scripts/README.md）")
except Exception as e:
    print("UNKNOWN|读取失败: %s" % type(e).__name__)
PY
}
# 结算日志是 append-only 的 kind 事件流（settlement / closed / redeemed）：
# 未赎回 = 出现过 claimId 的 settlement − 已 redeemed 的 claimId（同一文件重放）。
settlements_summary() {
  local f=$1
  [ -n "$f" ] || { printf '%s' "UNKNOWN|未配置路径"; return 0; }
  [ -e "$f" ] || { printf 'UNKNOWN|%s 不存在（dry 且从未结算过时属正常）' "$f"; return 0; }
  python3 - "$f" <<'PY' 2>/dev/null || printf '%s' "UNKNOWN|解析失败"
import json, sys
try:
    claims = {}
    for line in open(sys.argv[1], errors="replace"):
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except Exception:
            continue
        kind = r.get("kind")
        if kind == "settlement":
            rec = r.get("record") or {}
            cid = rec.get("claimId")
            if cid:
                claims[cid] = float(rec.get("payoutUsd") or 0) + claims.get(cid, 0.0)
        elif kind == "redeemed":
            claims.pop(r.get("claimId"), None)
    print("OK|%d 笔，应收合计 $%.2f" % (len(claims), sum(claims.values())))
except PermissionError:
    print("UNKNOWN|权限被拒（launchd/TCC 读不到外置卷；见 scripts/README.md）")
except Exception as e:
    print("UNKNOWN|读取失败: %s" % type(e).__name__)
PY
}
render_frag() { # OK|x → x ；UNKNOWN|y → 无法判定（y）
  case "$1" in
    OK\|*) printf '%s' "${1#OK|}" ;;
    UNKNOWN\|*) printf '无法判定（%s）' "${1#UNKNOWN|}" ;;
    *) printf '%s' "$1" ;;
  esac
}

# ── 状态文件 ────────────────────────────────────────────────────────────────
state_get() {
  local key=$1
  [ -f "$STATE_FILE" ] || return 1
  sed -n "s/^${key}=//p" "$STATE_FILE" 2>/dev/null | head -1
}
write_state() { # status last_up last_alert kind
  local tmp="$STATE_FILE.tmp.$$"
  {
    printf 'status=%s\n' "$1"
    printf 'last_up_epoch=%s\n' "$2"
    printf 'last_alert_epoch=%s\n' "$3"
    printf 'last_alert_kind=%s\n' "$4"
    printf 'last_check_epoch=%s\n' "$(now_epoch)"
  } > "$tmp" 2>/dev/null && mv -f "$tmp" "$STATE_FILE" 2>/dev/null
  return 0
}
# 无存活记录时（重启后第一次跑）用数据落盘时间兜底：比「未知」有用得多，而且是
# 标注了来源的推断，不是假装看门狗当时在场。
data_mtime_hint() {
  python3 - "$@" <<'PY' 2>/dev/null || true
import os, sys
best = None
for p in sys.argv[1:]:
    try:
        m = os.path.getmtime(p)
    except OSError:
        continue
    if best is None or m > best[0]:
        best = (m, p)
if best:
    print("%d %s" % (int(best[0]), best[1]))
PY
}

# ── 通知（本地；无出站请求）─────────────────────────────────────────────────
# AppleScript 字符串里 `\` 与 `"` 必须转义：一个带引号的恢复命令就能让通知在语法
# 错误里静默消失——偏偏是最需要它出声的时候。
notify_local() {
  local title=$1 body=$2 esc ttl
  if [ "$NOTIFY" != "1" ]; then
    log_append "通知: 已禁用（BK_NOTIFY=0 / --no-notify），仅落日志与 STACK_DOWN 标记"
    return 0
  fi
  if ! command -v osascript >/dev/null 2>&1; then
    log_append "通知: osascript 不可用（非 macOS？），仅落日志"
    return 0
  fi
  esc=$(printf '%s' "$body" | sed 's/\\/\\\\/g; s/"/\\"/g')
  ttl=$(printf '%s' "$title" | sed 's/\\/\\\\/g; s/"/\\"/g')
  if osascript -e "display notification \"$esc\" with title \"$ttl\" sound name \"Basso\"" >/dev/null 2>&1; then
    log_append "通知: 已发送（macOS 本地通知）"
  else
    # 常见原因：launchd 会话没有 Automation 授权 / 没有 GUI 会话。告警不能因此消失，
    # 所以这里只记一笔——日志与 STACK_DOWN 标记照旧。
    log_append "通知: 发送失败（缺 Automation 授权或无 GUI 会话？）——告警仍在日志与 STACK_DOWN 标记里"
  fi
}

# ── 自测（fixture 驱动；绝不接触真内核 / 真 socket / 生产 data/）────────────
ST_TMP=""
ST_PIDS=""
st_cleanup() {
  local p
  # 显式 st_cleanup + EXIT trap 会各调一次，所以这里必须容忍「已经被清过」。
  for p in ${ST_PIDS:-}; do kill -TERM "$p" 2>/dev/null || true; done
  if [ -n "${ST_TMP:-}" ]; then rm -rf "$ST_TMP" 2>/dev/null; fi
  ST_TMP=""
  ST_PIDS=""
  return 0
}

run_self_test() {
  local fail=0 cases=0
  ST_TMP=$(mktemp -d "${TMPDIR:-/tmp}/bk-watchdog-selftest.XXXXXX") || { echo "SELFTEST FAIL: mktemp 失败" >&2; return 1; }
  trap st_cleanup EXIT

  local st="$ST_TMP/state" pos="$ST_TMP/positions.jsonl" settle="$ST_TMP/settlements.jsonl"
  mkdir -p "$st"
  # 断言用的「期望」都从同一个 state 文件里读，所以父进程的这几个路径必须指向 fixture，
  # 不能留在默认的 ~/Library/Logs/...（否则断言读的是生产状态文件，既不准也越界）。
  STATE_DIR="$st"; LOG="$st/watchdog.log"; STATE_FILE="$st/state"; DOWN_MARKER="$st/STACK_DOWN"
  # 自测必须与外界的注入环境隔离：外面若导出了 DRY_RUN=false / BK_*，用例就会
  # 判成另一种模式——最坏情况下 live 门禁用例会被环境本身绕过。
  unset DRY_RUN
  unset BK_REPO_ROOT BK_CORE_PGREP BK_CORE_PIDFILE BK_SOCKET BK_WATCHDOG_DIR BK_REPEAT_SEC
  unset BK_ENV_FILE BK_POSITIONS BK_SETTLEMENTS BK_DATA_MTIME_HINT
  unset BK_AUTOSTART BK_AUTOSTART_CMD BK_NOTIFY BK_SILENCE

  # fixture「内核样」进程。不能 `cp /bin/sleep`：macOS 拒绝执行平台二进制的复制品
  # （`Killed: 9`），复制出来的 fixture 一秒都活不了。改成 exec 一个真程序，并把
  # 「内核样」的唯一路径当参数传给它——这样 `ps -o command=` 与 `pgrep -f` 都能匹配到
  # 一个不可能与真内核撞车的字符串，而命令行匹配本身正是判定的一部分。
  local fake_tag="$ST_TMP/fake-blitzkrieg-core-$$"
  : > "$fake_tag"
  /usr/bin/tail -f "$fake_tag" &
  local fake_pid=$!
  ST_PIDS="$fake_pid"
  disown 2>/dev/null || true
  sleep 0.3

  # fixture UDS 监听器：只有真的 listen，connect 探针才会返回 ok。
  local srv_sock="$ST_TMP/fake.sock" srv_pid
  python3 - "$srv_sock" <<'PY' >/dev/null 2>&1 &
import os, socket, sys
p = sys.argv[1]
try:
    os.unlink(p)
except OSError:
    pass
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.bind(p)
s.listen(16)
while True:
    try:
        c, _ = s.accept()
    except Exception:
        break
    c.close()
PY
  srv_pid=$!
  ST_PIDS="$ST_PIDS $srv_pid"
  disown 2>/dev/null || true
  local i=0
  while [ ! -e "$srv_sock" ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done

  # 数据 fixture：2 笔未平仓；结算 2 笔中 1 笔已赎回（必须被扣掉，剩 1 笔未赎回）。
  {
    printf '%s\n' '{"id":"p1","asset":"BTC","direction":"long","shares":10,"costUsd":6.5}'
    printf '%s\n' '{"id":"p2","asset":"ETH","direction":"short","shares":4,"costUsd":2.5}'
  } > "$pos"
  {
    printf '%s\n' '{"kind":"settlement","atMs":1,"record":{"claimId":"c1","payoutUsd":12.5}}'
    printf '%s\n' '{"kind":"settlement","atMs":2,"record":{"claimId":"c2","payoutUsd":3.25}}'
    printf '%s\n' '{"kind":"redeemed","atMs":3,"claimId":"c2","txHash":"dry-simulated"}'
  } > "$settle"

  local pidfile_dead="$ST_TMP/dead.pid" pidfile_live="$ST_TMP/live.pid" pidfile_recycled="$ST_TMP/recycled.pid"
  printf '%s\n' "999999" > "$pidfile_dead"
  printf '%s\n' "$fake_pid" > "$pidfile_live"
  printf '%s\n' "$$" > "$pidfile_recycled"

  local env_dry="$ST_TMP/env-dry" env_live="$ST_TMP/env-live" marker="$ST_TMP/autostart-ran"
  printf '%s\n' 'DRY_RUN=true' > "$env_dry"
  printf '%s\n' 'DRY_RUN=false' > "$env_live"
  local fake_autostart="touch $marker"

  # 每个用例都注入 fixture 路径：自测绝不读生产 data/、绝不碰真 socket、绝不访问
  # 真内核。BK_AUTOSTART_CMD 一律指向 marker 命令，所以**即使门禁逻辑写错**，自测
  # 也不可能真的拉起一个内核。
  local common="--state-dir $st --pidfile $pidfile_dead --socket $ST_TMP/absent.sock --no-notify"
  local out rc

  ck() { # ck <描述> <期望> <实际>
    cases=$((cases + 1))
    [ "$2" = "$3" ] || { echo "  FAIL: $1（期望 '$2'，得到 '$3'）"; fail=1; }
  }
  ck_contains() { # ck_contains <描述> <文件> <子串>
    cases=$((cases + 1))
    grep -q -- "$3" "$2" 2>/dev/null || { echo "  FAIL: $1（$2 里没有 '$3'）"; fail=1; }
  }
  run() { # run <期望退出码> <描述> [参数...]
    local want=$1 desc=$2; shift 2
    out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_ENV_FILE="$env_dry" \
          BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 \
          BK_AUTOSTART_CMD="$fake_autostart" \
          bash "$SELF" "$@" 2>&1)
    rc=$?
    ck "$desc 退出码" "$want" "$rc"
    printf '%s\n' "$out"
  }
  live_env_run() { # 用真实 live .env fixture 跑一次（自动拉起门禁用例）
    out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_ENV_FILE="$env_live" \
          BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 \
          BK_AUTOSTART_CMD="$fake_autostart" \
          bash "$SELF" "$@" 2>&1)
    rc=$?
  }
  wait_for_file() { # 自动拉起是后台动作，给它一点时间落盘
    local f=$1 n=0
    while [ ! -f "$f" ] && [ "$n" -lt 40 ]; do sleep 0.1; n=$((n + 1)); done
  }

  echo "== 1. 用法错误必须报 2（不能把用法错当成内核状态）"
  out=$(bash "$SELF" --bogus 2>&1); rc=$?
  ck "未知参数退出码" 2 "$rc"

  echo "== 2. 进程不在（陈旧 pidfile + 无 socket）：退出 1 + 停机告警 + STACK_DOWN 标记"
  rm -f "$st/watchdog.log" "$marker"
  out=$(run 1 "停机" $common)
  ck_contains "告警标题" "$st/watchdog.log" "停机告警"
  ck_contains "含模式" "$st/watchdog.log" "模式: dry"
  ck_contains "含未平仓" "$st/watchdog.log" "未平仓: 2 笔"
  ck_contains "含未赎回" "$st/watchdog.log" "未赎回应收: 1 笔"
  ck_contains "含最后已知存活" "$st/watchdog.log" "最后已知存活:"
  ck_contains "含恢复命令" "$st/watchdog.log" "恢复命令:"
  ck_contains "判定写明进程不在" "$st/watchdog.log" "进程不在"
  ck "down 标记文件存在" "yes" "$([ -f "$st/STACK_DOWN" ] && echo yes || echo no)"
  ck "状态=down" "down" "$(state_get status)"

  echo "== 3. 去抖：持续 down、窗口未到 → 不重复告警"
  out=$(run 1 "去抖" $common)
  ck "告警仍只有 1 条" "1" "$(grep -c '^\[停机告警\]' "$st/watchdog.log")"

  echo "== 4. 去抖：窗口到期（--repeat-sec 0）→ 允许再响一次"
  out=$(run 1 "窗口到期" $common --repeat-sec 0)
  ck "告警变成 2 条" "2" "$(grep -c '^\[停机告警\]' "$st/watchdog.log")"

  echo "== 5. 进程在但 socket 不通：退出 1，且与「进程不在」区分"
  rm -f "$st/watchdog.log"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_ENV_FILE="$env_dry" \
        BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 BK_AUTOSTART_CMD="$fake_autostart" \
        bash "$SELF" --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_live" --socket "$ST_TMP/absent.sock" 2>&1)
  rc=$?
  ck "退出码" 1 "$rc"
  ck_contains "写明 socket 不通" "$st/watchdog.log" "socket 不通"
  ck_contains "写明进程其实在" "$st/watchdog.log" "进程在"

  echo "== 6. 进程在 + socket 通：退出 0 + 恢复日志 + 清掉 STACK_DOWN 标记"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_ENV_FILE="$env_dry" \
        BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 BK_AUTOSTART_CMD="$fake_autostart" \
        bash "$SELF" --state-dir "$st" --pidfile "$pidfile_live" --socket "$srv_sock" 2>&1)
  rc=$?
  ck "退出码" 0 "$rc"
  ck "状态=up" "up" "$(state_get status)"
  ck_contains "恢复日志" "$st/watchdog.log" "栈已恢复"
  ck "down 标记已移除" "no" "$([ -f "$st/STACK_DOWN" ] && echo yes || echo no)"

  echo "== 7. pidfile 的 pid 活着但命令行不是内核（pid 回收）→ 必须判为不在跑"
  rm -f "$st/watchdog.log"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_ENV_FILE="$env_dry" \
        BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 BK_AUTOSTART_CMD="$fake_autostart" \
        bash "$SELF" --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_recycled" --socket "$ST_TMP/absent.sock" 2>&1)
  rc=$?
  ck "退出码" 1 "$rc"
  ck_contains "写明命令行不匹配" "$st/watchdog.log" "命令行不匹配"

  echo "== 8. 读不到就说读不到：缺失的持仓/结算文件 → 无法判定（不是 0）"
  rm -f "$st/watchdog.log"
  BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_ENV_FILE="$env_dry" \
    BK_POSITIONS="$ST_TMP/nope.jsonl" BK_SETTLEMENTS="$ST_TMP/nope.jsonl" BK_NOTIFY=0 \
    BK_AUTOSTART_CMD="$fake_autostart" \
    bash "$SELF" --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_dead" --socket "$ST_TMP/absent.sock" >/dev/null 2>&1
  ck_contains "未平仓=无法判定" "$st/watchdog.log" "未平仓: 无法判定"
  ck_contains "未赎回=无法判定" "$st/watchdog.log" "未赎回应收: 无法判定"

  echo "== 9. 静默标记：状态/退出码照旧，但不通知、不落告警正文"
  rm -f "$st/watchdog.log"
  printf '%s\n' "停机已确认" > "$st/silence"
  out=$(run 1 "静默" $common --repeat-sec 0)
  ck "无停机告警正文" "0" "$(grep -c '^\[停机告警\]' "$st/watchdog.log")"
  ck "但日志仍留一行记录" "1" "$(grep -c '停机告警（' "$st/watchdog.log")"
  ck_contains "记录已静默" "$st/watchdog.log" "已静默"
  ck "STACK_DOWN 标记照旧" "yes" "$([ -f "$st/STACK_DOWN" ] && echo yes || echo no)"
  rm -f "$st/silence"

  echo "== 10. 自动拉起门禁：默认关 / live 永不 / dry+两把钥匙才动"
  rm -f "$marker" "$st/watchdog.log"
  out=$(run 1 "默认关" $common --repeat-sec 0)
  ck "默认不拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  ck_contains "记录未开启" "$st/watchdog.log" "自动拉起未开启"

  rm -f "$marker" "$st/watchdog.log"
  live_env_run --autostart --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_dead" --socket "$ST_TMP/absent.sock"
  ck "live 退出码仍是 1" 1 "$rc"
  ck "live 拒绝自动拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  ck_contains "live 拒绝写在日志里" "$st/watchdog.log" "live 模式禁止自动拉起"

  rm -f "$marker" "$st/watchdog.log"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_ENV_FILE="$env_dry" \
        BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 BK_AUTOSTART_CMD="$fake_autostart" \
        bash "$SELF" --autostart --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_dead" --socket "$ST_TMP/absent.sock" 2>&1)
  rc=$?
  wait_for_file "$marker"
  ck "dry + 显式开关 + 显式命令 → 拉起" "yes" "$([ -f "$marker" ] && echo yes || echo no)"
  ck_contains "拉起动作写进日志" "$st/watchdog.log" "已自动拉起"

  echo "== 11. 开关打开但没有 BK_AUTOSTART_CMD → 仍不动手"
  rm -f "$marker" "$st/watchdog.log"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_ENV_FILE="$env_dry" \
        BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 BK_AUTOSTART_CMD="" \
        bash "$SELF" --autostart --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_dead" --socket "$ST_TMP/absent.sock" 2>&1)
  rc=$?
  ck "退出码" 1 "$rc"
  ck "未拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  ck_contains "写明缺命令" "$st/watchdog.log" "未配置 BK_AUTOSTART_CMD"

  echo "== 12. --status 只读：不写日志、不告警、不自动拉起"
  rm -f "$st/watchdog.log" "$marker"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_ENV_FILE="$env_dry" \
        BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 BK_AUTOSTART_CMD="$fake_autostart" \
        bash "$SELF" --status --autostart --state-dir "$st" --pidfile "$pidfile_live" --socket "$srv_sock" 2>&1)
  rc=$?
  ck "退出码 0" 0 "$rc"
  ck "不写日志" "no" "$([ -f "$st/watchdog.log" ] && echo yes || echo no)"
  ck "不拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  printf '%s\n' "$out" > "$ST_TMP/status.out"
  ck_contains "--status 打印模式" "$ST_TMP/status.out" "模式: dry"
  ck_contains "--status 打印未平仓" "$ST_TMP/status.out" "未平仓: 2 笔"

  st_cleanup
  echo
  if [ "$fail" -eq 0 ]; then
    echo "SELFTEST PASS（12 组用例 / $cases 项断言）"
    return 0
  fi
  echo "SELFTEST FAIL（12 组用例 / $cases 项断言，见上面 FAIL 行）"
  return 1
}

if [ "$CHECK_MODE" = "self-test" ]; then
  run_self_test
  exit $?
fi

# ── 一次检查 ────────────────────────────────────────────────────────────────
ENV_FILE="${ENV_FILE_CFG:-$ROOT/.env}"
POSITIONS_FILE="${POSITIONS_CFG:-$ROOT/data/positions/positions.jsonl}"
SETTLEMENTS_FILE="${SETTLEMENTS_CFG:-$ROOT/data/orders/settlements.jsonl}"
MTIME_HINTS="${BK_DATA_MTIME_HINT:-$POSITIONS_FILE:$ROOT/data/orders/orders.jsonl:$ROOT/data/trades/trades.jsonl:$ROOT/data/soak/soak.jsonl}"

# 1) 候选 pid：配了 pidfile 就按 soak-resident 的三段语义判；否则 pgrep 发现 + 复核。
CORE_PID=""
PROC_DETAIL=""
if [ -n "$PIDFILE_CFG" ]; then
  pid_candidate=$(cat "$PIDFILE_CFG" 2>/dev/null || true)
  if alive "$PIDFILE_CFG" "$CORE_PGREP"; then
    CORE_PID="$pid_candidate"
  elif [ -n "$pid_candidate" ] && kill -0 "$pid_candidate" 2>/dev/null; then
    PROC_DETAIL="pidfile $PIDFILE_CFG 里的 pid $pid_candidate 活着，但命令行不匹配内核（${CORE_PGREP}）——判为不在跑"
  else
    PROC_DETAIL="pidfile $PIDFILE_CFG 里没有存活进程"
  fi
else
  CORE_PID=$(pgrep -f "$CORE_PGREP" 2>/dev/null | head -1 || true)
  if [ -n "$CORE_PID" ] && ! pid_is_core "$CORE_PID"; then
    PROC_DETAIL="pgrep 命中的 pid $CORE_PID 命令行不匹配内核——判为不在跑"
    CORE_PID=""
  fi
  [ -n "$CORE_PID" ] || PROC_DETAIL="${PROC_DETAIL:-pgrep -f $CORE_PGREP 未发现内核进程}"
fi
CORE_COUNT=0
[ -n "$CORE_PID" ] && CORE_COUNT=$(pgrep -f "$CORE_PGREP" 2>/dev/null | wc -l | tr -d ' ')

# 2) argv（socket 与 mode 的权威来源，进程不在时为空）
CORE_ARGV=""
[ -n "$CORE_PID" ] && CORE_ARGV=$(ps -o command= -p "$CORE_PID" 2>/dev/null)

# 3) 两个证据：进程 + socket
PROC_STATE="不在"
[ -n "$CORE_PID" ] && PROC_STATE="在（pid ${CORE_PID}）"
CORE_SOCKET="${SOCKET_CFG:-}"
[ -n "$CORE_SOCKET" ] || CORE_SOCKET=$(argv_socket || true)
[ -n "$CORE_SOCKET" ] || CORE_SOCKET=$(default_socket)
SOCK_RESULT=$(socket_probe "$CORE_SOCKET")
SOCK_STATE="不通（${SOCK_RESULT}）"
[ "$SOCK_RESULT" = "ok" ] && SOCK_STATE="通"
SOCK_NODE_STALE=""
if [ -z "$CORE_PID" ] && [ -e "$CORE_SOCKET" ]; then
  SOCK_NODE_STALE="；残留 socket 节点 ${CORE_SOCKET}（下次启动会自动清理，不代表还活着）"
fi
STACK_UP=0
[ -n "$CORE_PID" ] && [ "$SOCK_RESULT" = "ok" ] && STACK_UP=1

read_mode "$ENV_FILE"
POS_FRAG=$(positions_summary "$POSITIONS_FILE")
SETTLE_FRAG=$(settlements_summary "$SETTLEMENTS_FILE")
POS_TEXT=$(render_frag "$POS_FRAG")
SETTLE_TEXT=$(render_frag "$SETTLE_FRAG")

PREV_STATUS=$(state_get status || true)
LAST_UP=$(state_get last_up_epoch || true)
LAST_ALERT=$(state_get last_alert_epoch || true)
NOW=$(now_epoch)

if [ "$STACK_UP" -eq 1 ]; then
  NEW_STATUS=up
  LAST_UP="$NOW"
elif [ -n "$CORE_PID" ]; then
  NEW_STATUS=down_wedged
else
  NEW_STATUS=down
fi

# 「最后已知存活」：看门狗自己的记录优先（它真的看见过），否则退回数据落盘时间
# （推断，并标明是推断）。
last_alive_text() {
  local hint line
  if [ -n "${LAST_UP:-}" ]; then
    printf '%s（看门狗上一次确认存活）' "$(fmt_epoch "$LAST_UP")"
    return 0
  fi
  hint=$(printf '%s' "$MTIME_HINTS" | tr ':' ' ')
  # shellcheck disable=SC2086 # 有意分词：候选路径以空格分隔传给 python
  line=$(data_mtime_hint $hint)
  if [ -n "$line" ]; then
    printf '%s（由数据落盘时间推断：%s；看门狗状态文件里还没有存活记录）' "$(fmt_epoch "${line%% *}")" "${line#* }"
    return 0
  fi
  printf '%s' "未知（状态文件无记录，数据文件也没有可读的落盘时间）"
}

RECOVERY="cd \"$ROOT\" && blitzkrieg run"
AUTOSTART_NOTE="（默认不会自动拉起内核；live 模式下永不自动拉起——见 README.md §3.5）"

alert_body() {
  printf '[停机告警] BlitzkriegBot 交易内核不在运行\n'
  if [ "$NEW_STATUS" = "down_wedged" ]; then
    printf '状态: 进程在（pid %s，来源 %s）但 socket 不通：%s（探针: %s）\n' "$CORE_PID" "$([ -n "$SOCKET_CFG" ] && echo "--socket 参数" || echo "内核 argv")" "$CORE_SOCKET" "$SOCK_RESULT"
  else
    printf '状态: 进程不在（%s）%s\n' "${PROC_DETAIL:-未发现内核进程}" "$SOCK_NODE_STALE"
  fi
  [ "${CORE_COUNT:-0}" -gt 1 ] && printf '注意: 发现 %s 个内核进程（预期 1 个）\n' "$CORE_COUNT"
  printf '模式: %s（来源：%s）\n' "$MODE_VALUE" "$MODE_SOURCE"
  printf '未平仓: %s\n' "$POS_TEXT"
  printf '未赎回应收: %s\n' "$SETTLE_TEXT"
  printf '最后已知存活: %s\n' "$(last_alive_text)"
  printf '恢复命令: %s\n' "$RECOVERY"
  printf '日志: %s\n' "$LOG"
  printf '%s\n' "$AUTOSTART_NOTE"
}

# 通知正文（一行，短）：复用 render_frag，别在这里再抄一遍 OK|/UNKNOWN| 的解析。
# 为什么绕一层函数：**bash 3.2（macOS 自带）不能把 `case` 直接写在 $( ) 里**——它会
# 在 case 模式的 `)` 处把命令替换提前截断，报 `syntax error near unexpected token
# \`newline'`，而且 bash -n 检查不出来，只在运行时炸、替换结果为空。函数体里的 case
# 按普通函数解析，不受这个坑影响。
short_line() {
  printf '模式 %s｜未平仓 %s｜未赎回 %s｜恢复: blitzkrieg run' \
    "$MODE_VALUE" "$(render_frag "$POS_FRAG")" "$(render_frag "$SETTLE_FRAG")"
}

# ── --status：只读一次，不告警、不落状态、不自动拉起 ────────────────────────
if [ "$CHECK_MODE" = "status" ]; then
  printf '内核: %s\n' "$([ "$STACK_UP" -eq 1 ] && echo "RUNNING（pid ${CORE_PID:-?}）" || echo "NOT RUNNING")"
  printf '进程: %s %s\n' "$PROC_STATE" "${PROC_DETAIL:-}"
  printf 'socket: %s %s\n' "$CORE_SOCKET" "$SOCK_STATE"
  printf '模式: %s（%s）\n' "$MODE_VALUE" "$MODE_SOURCE"
  printf '未平仓: %s\n' "$POS_TEXT"
  printf '未赎回应收: %s\n' "$SETTLE_TEXT"
  printf '最后已知存活: %s\n' "$(last_alive_text)"
  printf '状态目录: %s\n' "$STATE_DIR"
  printf '恢复命令: %s\n' "$RECOVERY"
  [ "$STACK_UP" -eq 1 ] && exit 0
  exit 1
fi

# ── 状态机与告警 ────────────────────────────────────────────────────────────
SILENCED=0
if [ "$SILENCE" = "1" ] || [ -f "$STATE_DIR/silence" ]; then SILENCED=1; fi

if [ "$STACK_UP" -eq 1 ]; then
  write_state up "$LAST_UP" "${LAST_ALERT:-0}" "$(state_get last_alert_kind || true)"
  if [ "$PREV_STATUS" != "up" ]; then
    log_append "$(fmt_epoch "$NOW") 栈已恢复: 内核 pid ${CORE_PID:-?} 在 $CORE_SOCKET 上服务（模式 ${MODE_VALUE}）"
    rm -f "$DOWN_MARKER" 2>/dev/null || true
    say "OK 内核存活（pid ${CORE_PID:-?}，${CORE_SOCKET}）模式 ${MODE_VALUE}"
    if [ "$PREV_STATUS" = "down" ] || [ "$PREV_STATUS" = "down_wedged" ]; then
      if [ "$SILENCED" -eq 1 ]; then
        log_append "通知: 静默中，恢复通知已抑制"
      else
        notify_local "BlitzkriegBot: 交易内核已恢复" "pid ${CORE_PID:-?}｜模式 ${MODE_VALUE}｜未平仓 $(printf '%s' "$POS_TEXT" | head -c 60)"
      fi
    fi
  else
    say "OK 内核存活（pid ${CORE_PID:-?}，${CORE_SOCKET}）"
  fi
  say "   模式 ${MODE_VALUE}｜未平仓 ${POS_TEXT}｜未赎回 ${SETTLE_TEXT}"
  exit 0
fi

# down：只在状态翻转或窗口到期时出声
ALERT=0
REASON=""
if [ "$PREV_STATUS" != "down" ] && [ "$PREV_STATUS" != "down_wedged" ]; then
  ALERT=1
  REASON="状态翻转（${PREV_STATUS:-无记录} → ${NEW_STATUS}）"
elif [ -z "$LAST_ALERT" ]; then
  ALERT=1
  REASON="无告警记录"
elif [ $((NOW - LAST_ALERT)) -ge "$REPEAT_SEC" ]; then
  ALERT=1
  REASON="距上次告警 $((NOW - LAST_ALERT))s ≥ ${REPEAT_SEC}s"
else
  REASON="去抖中（距上次告警 $((NOW - LAST_ALERT))s < ${REPEAT_SEC}s）"
fi

if [ "$ALERT" -eq 1 ]; then
  if [ "$SILENCED" -eq 1 ]; then
    # 静默压的是「喊人」，不是「记录」：日志留一行，告警正文与通知都不发。
    log_append "$(fmt_epoch "$NOW") 停机告警（${REASON}）——已静默（$STATE_DIR/silence）：不通知、不落告警正文；STACK_DOWN 标记与退出码照常"
    printf '%s\n' "DOWN 内核不在跑（已静默：不通知，仅落日志与 STACK_DOWN 标记）"
  else
    {
      printf '%s 停机告警（%s）\n' "$(fmt_epoch "$NOW")" "$REASON"
      alert_body
    } >> "$LOG" 2>/dev/null || true
    # 异常输出不受 --quiet 影响：安静模式不该连告警一起安静掉
    printf '%s\n' "$(alert_body)"
    notify_local "BlitzkriegBot: 交易内核不在运行" "$(short_line)"
  fi
  : > "$DOWN_MARKER" 2>/dev/null || true
  write_state "$NEW_STATUS" "${LAST_UP:-}" "$NOW" "$NEW_STATUS"
else
  log_append "$(fmt_epoch "$NOW") 仍停机（${REASON}）"
  if [ "$SILENCED" -eq 1 ]; then
    say "DOWN 内核不在跑（已静默；${REASON}）"
  else
    say "DOWN 内核不在跑（${REASON}）"
  fi
  write_state "$NEW_STATUS" "${LAST_UP:-}" "${LAST_ALERT:-0}" "$(state_get last_alert_kind || true)"
fi

# ── 自动拉起：默认关；live 永不；命令必须显式给出 ────────────────────────────
if [ "$AUTOSTART" != "1" ]; then
  log_append "自动拉起未开启（默认）：只告警，不动手"
elif [ "$SILENCED" -eq 1 ]; then
  log_append "自动拉起: 跳过——已静默（你自己知道它停着）"
elif [ "$MODE_VALUE" = "live" ]; then
  log_append "自动拉起: 拒绝——live 模式禁止自动拉起（既有约定：live 只在用户在场时开）；开关打开也不改变这条硬门禁"
elif [ "$MODE_VALUE" != "dry" ]; then
  log_append "自动拉起: 拒绝——当前模式 '$MODE_VALUE' 不是 dry（只有 dry 允许自动拉起）"
elif [ -z "$AUTOSTART_CMD" ]; then
  log_append "自动拉起: 跳过——未配置 BK_AUTOSTART_CMD（脚本不替部署方决定启动什么）"
elif [ "$NEW_STATUS" = "down_wedged" ]; then
  log_append "自动拉起: 跳过——进程在但 socket 不通；再拉一个会造成两个内核抢同一个 socket，交人工处理"
elif [ "$ALERT" -ne 1 ]; then
  log_append "自动拉起: 跳过——本次不在告警窗口内（避免在去抖窗口里反复拉起）"
else
  log_append "自动拉起: 已自动拉起（dry）: $AUTOSTART_CMD"
  nohup sh -c "$AUTOSTART_CMD" >> "$STATE_DIR/autostart.log" 2>&1 &
  disown 2>/dev/null || true
  say "已自动拉起（dry）: $AUTOSTART_CMD"
fi

exit 1
