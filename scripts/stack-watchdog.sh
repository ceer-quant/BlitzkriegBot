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
# 两个答案。区别只有两处：内核自己不写 pidfile，所以未配置 --pidfile 时用
# `pgrep -f` 发现候选 pid，再用同一套语义（kill -0 + 命令行匹配）复核；命令行用
# `ps -ww` 读（不这样读，人在终端里跑会把长命令行的活内核误判成停机），并拒收
# 僵尸——两条都是实测踩出来的，细节写在 alive() 上方。
#
# UDS 探针：进程在 ≠ 服务在。soak-health.sh 用 `core.ping` 抓「活着但卡死」；
# 这里做更轻的一步——只 connect（与 ui_kit 的 `socket_served()` 同语义），连不上
# 即算异常，并且把两种形态在输出里分开写：
#   「进程不在」    —— 找不到内核进程
#   「进程在但 socket 不通」 —— 进程在，但 UDS 连不上（启动中 / 卡死 / 抢走了名字）
# 内核进程不在时残留的 socket 节点是**正常现象**（内核启动时先 unlink 再 bind，
# 见 ipc/server.rs），所以它只作附注，绝不当成「还活着」的证据。
#
# ── 重复内核：与状态无关的一等信号（issue #199）──────────────────────────────
# 「有两个内核在跑」不是停机的一种，是**另一种事故形态**：两个内核会写同一份
# 账本与订单库（data/positions、data/orders），造成重复下单或持仓口径错乱。
# 而它唯一的表现形态就是「看起来一切正常」——socket 通、退出码 0、日志干净：
# 把这条信息塞进停机告警的正文里（只有 down 才渲染）等于永远不看它。
# 所以它是独立信号，不依附于 up/down 判定：
#   - 复核通过的内核进程 ≥2 个 → 退出码 3（优先于 1），并写
#     $STATE_DIR/DUPLICATE_CORES 标记文件：调度器看退出码，人看日志与标记；
#   - `--status` 同样报 3，绝不显示成「一切正常」；
#   - 告警正文自带完整解释（风险 + 全部 pid + 各自的模式/socket + 止损命令），
#     单看这一条就能处置；
#   - 去抖与停机告警共用一套状态机（翻转时出声，持续期间每 REPEAT_SEC 重复一次）；
#   - 重复期间**绝不自动拉起**——多出来的那个不是「没起来」，再拉只会更多。
# 复核口径与它的代价：发现与复核都是**整条命令行上的子串匹配**（pgrep -f 的语义），所以
# 「只在命令行里提到内核路径」的进程也会命中——本机实测有 4 个 `zsh -c … cargo build …` /
# `ls -la target/release/blitzkrieg-core` 的构建 shell 命中了默认串。两条应对都不改判定：
#   1) 告警里逐个 pid 点名：argv 里既没有 --mode 也没有 --socket 的，标成「可疑」并提示
#      逐条核对（真实内核的 argv 一定同时带这两个参数，supervisor 每次都显式传）；
#   2) 要更严的部署方把 BK_CORE_PGREP 收紧成 "<路径>.*--socket"（自测第 22 组钉住这条）。
# 不把收紧做进默认值，是因为不带 --socket 手工起的真内核会被漏掉——漏报正是 #211 的原病。
#
# 去抖：只在状态翻转（up→down / down→up）时发告警；持续 down 期间最多每
# REPEAT_SEC（默认 900 = 15 分钟）重复一次。每分钟一条「内核不在跑」的刷屏和
# 一条都不发是同一个缺陷——信号都会消失。
#
# ── 备份新鲜度：第二条独立信号（issue #217）──────────────────────────────────
# 内核活着不等于数据安全。2026-09-21 实测：两个 databackup agent 从装上起
# **一次都没成功过**（macOS TCC 拒绝 launchd 拉起的外部卷访问），而 `launchctl list`
# 的退出码是 0、日志只有 94 字节——「看起来一切正常」正是它唯一的表现形态。
# 那条检查此前只存在于 `scripts/soak-health.sh`，而 soak-health 只在有人跑它时才响
# （`soak-health-loop.sh` 不跨重启、也不常驻）。本脚本是唯一**由 launchd 每 60 秒跑
# 一次**的检查，所以「距上次成功备份超过 N 小时」的告警必须也在这里有一条。
#
# 判定本身**不在本脚本里重写**：`data-backup.sh --status --quiet` 是唯一实现
# （产物年龄 + 最新一次自动尝试），这里消费它的一行判定与退出码——理由与 soak-health
# 第 8 节完全相同：两份「备份是否新鲜」迟早会给出两个答案，而错的那份恰好是没人看的
# 那份。本脚本只把它的结论接进**既有**的告警通道（同一份 watchdog.log、同一个本地
# 通知、同一套翻转去抖），并给出可编程的退出码 4。
#
# 与 up/down 的关系（刻意如此）：
#   * 它**不改变**内核存活的判定。内核在跑、备份却是三天前的 → 退出码 4，这正是
#     本条要抓的组合：栈看起来健康，数据没有保障。
#   * 内核已经停机时**不再单独发这条通知**——停机通知已经把人叫来了，备份缺失是停机的
#     后果，再刷一条只是把 4 条/小时变成 8 条/小时。但日志、BACKUP_STALE 标记与
#     退出码照旧：「不喊」不等于「不说」。
#   * 退出码优先级：3（重复内核）> 1（停机）> 4（备份不新鲜）> 0。
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
#     --no-backup-check 跳过备份新鲜度检查（没有备份调度的部署；单测/CI 用）
#     --quiet           只在异常（告警）时输出到 stdout
#     -h | --help
#
# 退出码: 0 = 唯一内核存活并在服务；1 = 内核不在跑 / socket 不可达（已告警）；
#         2 = 用法或配置错误；3 = 发现 ≥2 个内核进程（重复内核，issue #199；优先于 1）；
#         4 = 内核在跑但备份不新鲜（issue #217；3 与 1 优先于它）
#
# 环境变量（全部可选，用于部署与测试注入；与 soak-health.sh 的 seam 风格一致）:
#   BK_REPO_ROOT        仓库根（默认由脚本位置推导；内置盘副本部署时必填）
#   BK_CORE_PGREP       内核进程发现串（pgrep -f；默认 target/release/blitzkrieg-core。
#                       发现与复核都是子串匹配，只提到该路径的进程也会命中——要更严就设成
#                       "target/release/blitzkrieg-core.*--socket"，代价是漏掉不带该参数
#                       手工起的真内核）
#   BK_CORE_VERIFY_PGREP 对已发现 pid 的命令行复核串（默认同 BK_CORE_PGREP。
#                       只用于 --self-test 注入「发现了但复核不认」这一态，生产别设）
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
#   BK_BACKUP_CHECK     0 = 跳过备份新鲜度检查（默认 1；--no-backup-check 同义）
#   BK_BACKUP_STALE_HOURS / BK_BACKUP_FULL_STALE_HOURS
#                       两个 tier 的容忍上限（默认 26h / 192h；与 soak-health.sh 同样的
#                       默认值，且**导出给子进程**——容忍值住在被判定的那个脚本里）
#   BK_BACKUP_STATUS_SH 判定引擎（默认 <repo>/scripts/data-backup.sh；自测注入 fixture）
#   BK_BACKUP_DIR / BK_BACKUP_STATUS_DIR / BK_BACKUP_LOG_DIR
#                       落在子进程里的部署 seam（不设则用 data-backup.sh 自己的默认）
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
# 发现串与复核串分开：默认同一个值（发现到什么就复核什么）。分开的唯一用途是
# --self-test 能注入「pgrep 命中但复核不认」——真进程构造不出这种分歧（pgrep 与
# ps 读同一份 exec 时的 argv 快照），而这道复核门又必须被证明真的会拦（否则它就是
# 一个永远不会开火的守卫，KI-30）。
CORE_VERIFY_PGREP="${BK_CORE_VERIFY_PGREP:-$CORE_PGREP}"
ENV_FILE_CFG="${BK_ENV_FILE:-}"
POSITIONS_CFG="${BK_POSITIONS:-}"
SETTLEMENTS_CFG="${BK_SETTLEMENTS:-}"
AUTOSTART="${BK_AUTOSTART:-0}"
AUTOSTART_CMD="${BK_AUTOSTART_CMD:-}"
NOTIFY="${BK_NOTIFY:-1}"
SILENCE="${BK_SILENCE:-0}"
BACKUP_CHECK="${BK_BACKUP_CHECK:-1}"
# 容忍上限住在**被判定的脚本**里（data-backup.sh 的 --stale-hours），所以这两个值
# 只负责被导出下去。默认值与 soak-health.sh 第 8 节逐字一致：同一个问题在两处被问
# 出两个答案，比没有答案更糟。
BK_BACKUP_STALE_HOURS="${BK_BACKUP_STALE_HOURS:-26}"
BK_BACKUP_FULL_STALE_HOURS="${BK_BACKUP_FULL_STALE_HOURS:-192}"
export BK_BACKUP_STALE_HOURS BK_BACKUP_FULL_STALE_HOURS

usage() {
  sed -n 's/^# \{0,1\}//p' <<'HDR' >&2
# 用法: scripts/stack-watchdog.sh [--status|--self-test|--autostart|--no-autostart]
#        [--repeat-sec N] [--state-dir DIR] [--pidfile PATH] [--socket PATH]
#        [--no-notify] [--no-backup-check] [--quiet]
# 退出码: 0 = 唯一内核存活；1 = 内核不在跑 / socket 不可达（已告警）；
#         2 = 用法或配置错误；3 = 发现 ≥2 个内核进程（重复内核，优先于 1）；
#         4 = 内核在跑但备份不新鲜（issue #217；3 与 1 优先于它）
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
    --no-backup-check) BACKUP_CHECK=0 ;;
    --quiet)         QUIET=1 ;;
    -h|--help)       usage; exit 0 ;;
    *) echo "stack-watchdog: 未知参数: $1" >&2; usage; exit 2 ;;
  esac
  shift
done

case "$REPEAT_SEC" in
  ''|*[!0-9]*) echo "stack-watchdog: --repeat-sec 需要非负整数，得到 '$REPEAT_SEC'" >&2; exit 2 ;;
esac
case "$BACKUP_CHECK" in ''|*[!01]*) echo "stack-watchdog: --no-backup-check/BK_BACKUP_CHECK 只能是 0 或 1，得到 '$BACKUP_CHECK'" >&2; exit 2 ;; esac
[ -n "$STATE_DIR" ] || { echo "stack-watchdog: --state-dir 不能为空" >&2; exit 2; }
[ -f "$ROOT/Cargo.toml" ] || { echo "stack-watchdog: $ROOT 不是 BlitzkriegBot 仓库根（缺 Cargo.toml）" >&2; exit 2; }

LOG="$STATE_DIR/watchdog.log"
STATE_FILE="$STATE_DIR/state"
DOWN_MARKER="$STATE_DIR/STACK_DOWN"
# 重复内核（issue #199）有它自己的标记：它不是「停机」，混用 STACK_DOWN 会让看标记的
# 人和脚本都读错事故类型。
DUP_MARKER="$STATE_DIR/DUPLICATE_CORES"
# 备份不新鲜（issue #217）同样有自己的标记与状态文件。不复用 state 文件的原因很具体：
# 那里只有一对 last_alert_epoch/kind，两条独立信号共用一对去抖字段就会互相压低对方的
# 警示——内核刚恢复把 last_alert_epoch 刷新掉，备份那条就闭嘴了，而它可能已经三天没备份。
BACKUP_MARKER="$STATE_DIR/BACKUP_STALE"
BACKUP_STATE_FILE="$STATE_DIR/backup-state"

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
#
# 两处加固（都是实测踩出来的，改之前先读）：
#   1) `ps -ww`，不是 `ps`。测量结果（macOS BSD ps）：`ps -o command=` 只在**它自己的
#      stdout 是终端**时按终端宽度截断（pty 60 列 → 只有 60 列；同一个进程换成管道或
#      文件就是完整 267 字节）。本脚本的 ps 一律经 `$( )`/管道取，所以实测**不截断**，
#      `-ww` 在这里是**防御性**的：判定的正确性不该取决于调用方的 fd 布局（以后有人把
#      某处改成继承 stdout，就会踩进来），而且让人在终端里手工核对时与我们看到的
#      是同一份完整命令行。别把它写成「修掉了一个线上 bug」——实测没有那么严重。
#   2) 拒绝僵尸（state=Z）。`kill -0` 对僵尸同样成功，僵尸的 command 是 `<defunct>`：
#      不加这一条，一个已经退出、只等回收的内核会被算成「进程在但 socket 不通」
#      （down_wedged），文案把运维指向错误的处置。实测：僵尸 kill -0=yes、state=Z、
#      command=<defunct>，而 `pgrep -f` 不列僵尸（所以 pgrep 分支自然免疫）。
# 匹配方言用 grep -E，与 `pgrep -f`（ERE）一致：否则 'a+b' 这类模式在发现端匹配、
# 在复核端（BRE，+ 是字面量）不匹配，复核会把自己发现的内核判掉。
alive() {
  local pf=$1 pat=$2 pid st
  [ -f "$pf" ] || return 1
  pid=$(cat "$pf" 2>/dev/null) || return 1
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  st=$(ps -ww -o state= -p "$pid" 2>/dev/null | tr -d '[:space:]')
  [ -n "$st" ] || return 1
  case "$st" in Z*) return 1 ;; esac
  ps -ww -o command= -p "$pid" 2>/dev/null | grep -qE -- "$pat" || return 1
  return 0
}
# pidfile 里的候选 pid 是僵尸或命令行不匹配时，给人看的分类（文案要指对处置）。
# 返回：zombie / mismatch / gone
pid_reject_reason() {
  local pid=$1 st
  [ -n "$pid" ] || { printf '%s' "gone"; return 0; }
  kill -0 "$pid" 2>/dev/null || { printf '%s' "gone"; return 0; }
  st=$(ps -ww -o state= -p "$pid" 2>/dev/null | tr -d '[:space:]')
  case "$st" in Z*) printf '%s' "zombie"; return 0 ;; esac
  printf '%s' "mismatch"
}
# 同一套复核，作用于「已发现的 pid」（内核不写 pidfile，候选来自 pgrep）。
# 匹配串用 $CORE_VERIFY_PGREP（默认与发现串相同）——它存在是为了让 --self-test 能
# 注入「pgrep 命中了、复核不认」这一态：pgrep 与 ps 读的是同一份 exec 时的 argv 快照，
# 真进程构造不出这种分歧（测量记录见本文件头部），而这道门又必须被证明真的会拦。
pid_is_core() {
  local pid=$1 st
  [ -n "$pid" ] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  st=$(ps -ww -o state= -p "$pid" 2>/dev/null | tr -d '[:space:]')
  [ -n "$st" ] || return 1
  case "$st" in Z*) return 1 ;; esac
  ps -ww -o command= -p "$pid" 2>/dev/null | grep -qE -- "$CORE_VERIFY_PGREP" || return 1
  return 0
}
# 通过复核的候选 pid 列表（升序；升序是为了让最小的 pid——通常是最早启动、也是占着
# socket 的那个——排在前面，但**不保证**：多内核时占 socket 的可能是后来者，所以
# 多内核一律按 DUPLICATE 告警处理，不依赖「选中的是哪一个」）。
core_pids_verified() {
  local p
  for p in $(pgrep -f "$CORE_PGREP" 2>/dev/null); do
    pid_is_core "$p" && printf '%s\n' "$p"
  done | sort -n
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
# 从任意 argv 取 --socket（重复内核告警要逐 pid 报各自挂在哪个 socket 上：
#   `blitzkrieg stop` 只处理**同一个 socket 上**的 blitzkrieg 家族进程，
#   见 ui/ui_kit_panel/src/stop_stack.rs——不同 socket 的内核要逐个停。）
argv_socket_of() {
  local argv=$1 s
  s=""
  if [ -n "$argv" ]; then
    s=$(printf '%s\n' "$argv" | tr ' ' '\n' | grep -A1 '^--socket$' | tail -1)
  fi
  if [ -n "$s" ]; then printf '%s' "$s"; else printf '%s' "未在 argv 指定（内核默认路径）"; fi
}
# 从任意 argv 取模式（argv 是权威：--readonly 压过 --mode）
argv_mode_of() {
  local argv=$1 m
  case "$argv" in
    *--readonly*) printf '%s' "readonly"; return 0 ;;
  esac
  m=""
  if [ -n "$argv" ]; then
    m=$(printf '%s\n' "$argv" | tr ' ' '\n' | grep -A1 '^--mode$' | tail -1)
  fi
  if [ -n "$m" ]; then printf '%s' "$m"; else printf '%s' "未在 argv 指定"; fi
}
# 重复告警里逐 pid 一行（pid / argv 模式 / argv socket）。两个内核跑不同模式
# （一个 dry 一个 live）是最坏的一种，argv 逐条读出来比总结一句「模式: live」有用。
#
# 为什么还要标「可疑」：**复核是子串匹配**（pgrep -f 与 grep 都在整条命令行上找内核
# 路径），于是「命令行里只是提到这个路径」的进程也会被算进来——本机实测有 4 个 `zsh -c
# … cargo build …` / `ls -la target/release/blitzkrieg-core` 的构建 shell 命中了默认串。
# 只提到路径的进程说不出 socket 与模式（真实内核的 argv 一定同时带 --socket 与 --mode，
# supervisor 每次都显式传，见 ui_kit/src/gateway/supervisor.rs 的 to_args()），所以
# 「两者都没有」就是个可核对的判据。这里只**点名提示**、不悄悄改判定：宁可多喊一次让人
# 核对，也不收紧发现串去冒「真内核被漏掉」的险（漏报才是 issue #211 的原病）。
pids_detail_text() {
  local p argv m s
  for p in $CORE_PIDS; do # shellcheck disable=SC2086 # 有意分词：pid 以空格分隔
    argv=$(ps -ww -o command= -p "$p" 2>/dev/null || true)
    m=$(argv_mode_of "$argv")
    s=$(argv_socket_of "$argv")
    if [ "$m" = "未在 argv 指定" ] && [ "$s" = "未在 argv 指定（内核默认路径）" ]; then
      printf '  - pid %s：argv 模式 %s；socket %s\n' "$p" "$m" "$s"
      printf '      ← 可疑：argv 里既没有 --mode 也没有 --socket；可能只是命令行提到内核路径的进程（构建 shell 等），处置前逐条核对\n'
    else
      printf '  - pid %s：argv 模式 %s；socket %s\n' "$p" "$m" "$s"
    fi
  done
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
# ── 备份新鲜度（issue #217）──────────────────────────────────────────────────
# 判定引擎是 data-backup.sh --status（唯一实现），这里只负责把它的结论接进既有的
# 日志/通知/标记/退出码通道。分两个函数是因为两种模式要的东西不同：check 要「该不该
# 出声 + 标记」，status 只要一行可读的结论（且**不写任何东西**，与 --status 的只读
# 契约一致）。
#
# BK_BK_REASON 是给人看的原因（引擎自己说的那句），BK_BK_NOTE 是给 --status 的一行。
backup_state_get() {
  [ -f "$BACKUP_STATE_FILE" ] || return 1
  sed -n "s/^$1=//p" "$BACKUP_STATE_FILE" 2>/dev/null | head -1
}
backup_state_put() { # status last_alert_epoch note
  local tmp="$BACKUP_STATE_FILE.tmp.$$"
  {
    printf 'status=%s\n' "$1"
    printf 'last_alert_epoch=%s\n' "$2"
    printf 'note=%s\n' "$3"
    printf 'last_check_epoch=%s\n' "$(now_epoch)"
  } > "$tmp" 2>/dev/null && mv -f "$tmp" "$BACKUP_STATE_FILE" 2>/dev/null
  return 0
}

# 备份判定引擎：默认 <repo>/scripts/data-backup.sh，可注入（自测）。
# 解释器与 soak-health.sh 同一套写法：data-backup.sh 是 bash（set -o pipefail、
# [[ =~ ]]），用 sh 调它在 macOS 上没事、在 Debian/Ubuntu 上（sh 是 dash）会当场
# 死掉——那样「备份没在发生」这句告警就是为错误的理由响的，正是本改动要消灭的那类缺陷。
BK_BACKUP_STATUS_SH="${BK_BACKUP_STATUS_SH:-$ROOT/scripts/data-backup.sh}"
BK_BK_CODE=0        # 0 = 新鲜；其他 = 不新鲜/无法判定（取引擎的退出码，或 2 = 脚本缺失）
BK_BK_NOTE=""       # 一行判定（引擎的 `backup: ` 那行）
BK_BK_DETAIL=""     # 引擎说的原因（错误那一行），只在异常时有
backup_verdict() {
  local interp out
  BK_BK_CODE=0; BK_BK_NOTE="off"; BK_BK_DETAIL=""
  if [ "$BACKUP_CHECK" != "1" ]; then
    BK_BK_NOTE="off（--no-backup-check / BK_BACKUP_CHECK=0）"
    return 0
  fi
  if [ ! -f "$BK_BACKUP_STATUS_SH" ]; then
    # KI-30：一个改名就静默消失的检查不是「通过」，是缺陷。缺文件按异常报。
    BK_BK_CODE=2
    BK_BK_NOTE="unusable"
    BK_BK_DETAIL="备份判定脚本不存在: $BK_BACKUP_STATUS_SH"
    return 0
  fi
  interp="sh"
  command -v bash >/dev/null 2>&1 && interp="bash"
  # 引擎的 stdout 与 stderr 一起收：判定在 stdout，拒绝运行的理由在 stderr，
  # 丢掉任何一半都会让告警说不出原因。
  if out=$("$interp" "$BK_BACKUP_STATUS_SH" --status --quiet 2>&1); then
    BK_BK_CODE=0
  else
    BK_BK_CODE=$?
    [ -n "$BK_BK_CODE" ] || BK_BK_CODE=1
    [ "$BK_BK_CODE" -ne 0 ] || BK_BK_CODE=1   # 防御：退出 0 之外的怪值一律当异常
  fi
  BK_BK_NOTE=$(printf '%s\n' "$out" | sed -n 's/^backup: //p' | head -1)
  if [ -z "$BK_BK_NOTE" ]; then
    BK_BK_NOTE="unusable"
    # 没有判定行 = 引擎拒绝运行（容忍值打错、树不见了……），它把原因写在最后一行；
    # 丢了原因就等于把「一个环境变量的笔误」变成一团看不懂的红。有界截断。
    BK_BK_DETAIL=$(printf '%s\n' "$out" | grep -v '^[[:space:]]*$' | tail -1 | cut -c1-160)
    [ -n "$BK_BK_DETAIL" ] || BK_BK_DETAIL="exit $BK_BK_CODE（引擎没有输出）"
  fi
  # 判定行解析出来了就不再另取「原因」：--quiet 的契约是那一行**自带**理由
  # （`sched=FAILED(0h:Operation not permitted)`），再抄一遍首行只是噪音。
  return 0
}
backup_bad() { [ "$BK_BK_CODE" -ne 0 ]; }

# 告警正文：自带完整解释（判定 + 原因 + 两个 tier 的最新产物从哪看），单看这一条
# 就能处置——和 duplicate_body 同一个标准。
backup_body() {
  printf '[备份告警] 最后一次成功备份已超过容忍上限（issue #217）\n'
  printf '判定: %s\n' "$BK_BK_NOTE"
  printf '容忍: light %sh / full %sh（BK_BACKUP_STALE_HOURS / BK_BACKUP_FULL_STALE_HOURS）\n' \
    "$BK_BACKUP_STALE_HOURS" "$BK_BACKUP_FULL_STALE_HOURS"
  [ -n "$BK_BK_DETAIL" ] && printf '原因: %s\n' "$BK_BK_DETAIL"
  printf '为什么现在说: 内核存活与数据安全是两件事——2026-09-21 的实测是备份从装上起从未成功过，\n'
  printf '  而 launchctl list 的退出码是 0、日志只有 94 字节，「看起来一切正常」是它唯一的表现形态。\n'
  printf '诊断: blitzkrieg backup --status   （等价：bash scripts/data-backup.sh --status）\n'
  printf '两条可行路线: A 给 /bin/sh 完全磁盘访问权限后 bash scripts/data-backup-install.sh；\n'
  printf '  B 无需改权限、今天可用：scripts/data-backup-loop.sh start（不跨重启）\n'
  printf '说明见 README.md §3.6；本脚本只判定与告警，绝不代替你安装或修改任何 LaunchAgent。\n'
  printf '日志: %s\n' "$LOG"
}
# 通知正文（一行，短）：喊人时最该看到的是「多久没成功备份了」和「怎么查」。
backup_short_line() {
  printf '备份不新鲜（%s）｜容忍 light %sh/full %sh｜查: blitzkrieg backup --status' \
    "$BK_BK_NOTE" "$BK_BACKUP_STALE_HOURS" "$BK_BACKUP_FULL_STALE_HOURS"
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
  # 断言计数/失败列表走 $ST_TMP 下的文件（原因见 ck_fail 的注释），这里不再有
  # 计数的局部变量：一个子 shell 里的计数器只会骗自己。
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
  unset BK_BACKUP_CHECK BK_BACKUP_STATUS_SH BK_BACKUP_STALE_HOURS BK_BACKUP_FULL_STALE_HOURS
  unset BK_BACKUP_DIR BK_BACKUP_STATUS_DIR BK_BACKUP_LOG_DIR

  # 备份新鲜度检查默认在自测里**关掉**（第 23 组再显式打开，用 fixture 判定引擎）。
  # 原因不是「太麻烦」，而是自测的契约：它零外部依赖、绝不碰生产路径。默认打开的话，
  # 每个用例都会去跑真正的 data-backup.sh，而它读的是真备份卷与真的 ~/Library/Logs——
  # 于是一台「本来就没有备份」的开发机（以及 Linux CI，那里根本没有
  # /Volumes/Hard Disk/BlitzkriegBotBackup）会让所有 `run 0`/`run 1` 用例统统变成 4。
  export BK_BACKUP_CHECK=0

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

  # ── 备份判定引擎的 fixture（第 23 组）────────────────────────────────────────
  # 真的去跑 data-backup.sh 会把自测绑到备份卷与 ~/Library/Logs 上，所以第 23 组
  # 注入三个只有几行的替身：它们唯一要复现的是**接口**——一行 `backup: …` 判定 +
  # 退出码（0 = 新鲜，非 0 = 不新鲜）+ 出错时的原因行。这样断言的是「看门狗怎么消费
  # 判定」，而不是「data-backup.sh 判得对不对」——后者的门禁是
  # scripts/data-backup-check.mjs 第 8 节，两处不重复。
  local bk_ok="$ST_TMP/backup-ok.sh" bk_bad="$ST_TMP/backup-bad.sh" bk_gone="$ST_TMP/no-such-backup-engine.sh"
  printf '%s\n' '#!/bin/sh' \
    'echo "backup: light=ok(2h)/sched=ok(2h) full=ok(30h)/sched=ok(30h)"' \
    'exit 0' > "$bk_ok"
  printf '%s\n' '#!/bin/sh' \
    'echo "backup: light=STALE(97h)/sched=FAILED(97h:Operation not permitted) full=STALE(97h)/sched=FAILED(97h:Operation not permitted)"' \
    'echo "  - light has never produced a backup" >&2' \
    'exit 1' > "$bk_bad"

  # 每个用例都注入 fixture 路径：自测绝不读生产 data/、绝不碰真 socket、绝不访问
  # 真内核。BK_AUTOSTART_CMD 一律指向 marker 命令，所以**即使门禁逻辑写错**，自测
  # 也不可能真的拉起一个内核。
  local common="--state-dir $st --pidfile $pidfile_dead --socket $ST_TMP/absent.sock --no-notify"
  local out rc

  # ── 发现串/复核串的注入旋钮 ────────────────────────────────────────────────
  # run_pgrep  : BK_CORE_PGREP（发现串）。默认命中 fixture 内核；改成不存在的串
  #              就能造出「pgrep 未命中」，改成另一个 tag 就能只命中某个 fixture。
  # run_verify : BK_CORE_VERIFY_PGREP（复核串）。默认与发现串相同（生产语义）；
  #              注入一个永不匹配的串 = 「pgrep 命中了、复核不认」——这是唯一能
  #              构造出该分歧的办法（真进程的 pgrep 与 ps 读同一份 argv 快照），
  #              也正是「复核门真的会拦」这一断言赖以成立的手段。
  local pgrep_hit="$fake_tag" pgrep_none="$ST_TMP/no-such-core-$$"
  local run_pgrep="$fake_tag" run_verify=""

  # ── 断言计数与失败记录：走文件，不走变量 ────────────────────────────────────
  # 为什么不用 `fail=1` / `cases=$((cases+1))`：run() 的退出码断言跑在调用点的
  # `$( )` 里，那是子 shell——子 shell 里的变量改不动父 shell。变异实验抓到了这个
  # 洞：「重复内核退出码 3 → 0」的变异会印出 FAIL 行却被吞进 $( )，计数也不涨，
  # 整个自测照样报 PASS。文件是共享的，append 是原子写，任何子 shell 里的断言都
  # 记得到；FAIL 同时打到 stderr（`$( )` 只吞 stdout，所以人一定看得见）。
  # 断言计数也走文件，否则 run() 里那一条会被漏算（报出来的总数会偏小）。
  local failfile="$ST_TMP/failures" asserts="$ST_TMP/assertions"
  : > "$failfile"; : > "$asserts"
  ck_count() { printf '.\n' >> "$asserts"; }
  ck_fail() { printf '  FAIL: %s\n' "$1" >&2; printf '%s\n' "$1" >> "$failfile"; }
  ck() { # ck <描述> <期望> <实际>
    ck_count
    [ "$2" = "$3" ] || ck_fail "$1（期望 '$2'，得到 '$3'）"
  }
  ck_contains() { # ck_contains <描述> <文件> <子串>
    ck_count
    grep -q -- "$3" "$2" 2>/dev/null || ck_fail "$1（$2 里没有 '$3'）"
  }
  # grep -c 在「0 个匹配」时自己就退出 1（还会印一个 0），直接 `|| echo 0` 会得到
  # 两行 "0"——断言于是永远失败。包一层，保证只输出一个数字。
  count_matches() { # count_matches <文件> <模式>
    local n
    n=$(grep -c -- "$2" "$1" 2>/dev/null) || n=0
    [ -n "$n" ] || n=0
    printf '%s' "$n"
  }
  ck_log_contains() { # 断言写在日志里的内容（比路径更好读）
    ck_contains "$1" "$LOG" "$2"
  }
  run() { # run <期望退出码> <描述> [参数...]；结果放 out/rc，并把子进程输出再打到 stdout
    local want=$1 desc=$2; shift 2
    out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$run_pgrep" BK_CORE_VERIFY_PGREP="${run_verify:-$run_pgrep}" \
          BK_ENV_FILE="$env_dry" \
          BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 \
          BK_AUTOSTART_CMD="$fake_autostart" \
          bash "$SELF" "$@" 2>&1)
    rc=$?
    ck "$desc 退出码" "$want" "$rc"
    # 调用点普遍写成 out=$(run ...)，所以这里把它打到 stdout 让外面接住；断言计数
    # 与失败记录已经走文件，**不再依赖这个 $( ) 的返回值**。
    printf '%s\n' "$out"
  }
  live_env_run() { # 用真实 live .env fixture 跑一次（自动拉起门禁用例）
    out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$run_pgrep" BK_CORE_VERIFY_PGREP="${run_verify:-$run_pgrep}" \
          BK_ENV_FILE="$env_live" \
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
  # 这一组必须用「真的一台内核都没有」的 fixture：发现串改成永不匹配的串。否则
  # fixture 里那个内核样进程会被 pgrep 复核发现，走到「pidfile 判死但复核发现内核」
  # 的跳过分支——那是另一条门禁，用例 15/19 专门测它。
  run_pgrep="$pgrep_none"
  rm -f "$marker" "$st/watchdog.log"
  out=$(run 1 "默认关" $common --repeat-sec 0)
  ck "默认不拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  ck_log_contains "记录未开启" "自动拉起未开启"

  rm -f "$marker" "$st/watchdog.log"
  live_env_run --autostart --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_dead" --socket "$ST_TMP/absent.sock"
  ck "live 退出码仍是 1" 1 "$rc"
  ck "live 拒绝自动拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  ck_log_contains "live 拒绝写在日志里" "live 模式禁止自动拉起"

  rm -f "$marker" "$st/watchdog.log"
  out=$(run 1 "dry 拉起" --autostart --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_dead" --socket "$ST_TMP/absent.sock")
  wait_for_file "$marker"
  ck "dry + 显式开关 + 显式命令 → 拉起" "yes" "$([ -f "$marker" ] && echo yes || echo no)"
  ck_log_contains "拉起动作写进日志" "已自动拉起"

  echo "== 11. 开关打开但没有 BK_AUTOSTART_CMD → 仍不动手"
  rm -f "$marker" "$st/watchdog.log"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$run_pgrep" BK_ENV_FILE="$env_dry" \
        BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 BK_AUTOSTART_CMD="" \
        bash "$SELF" --autostart --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_dead" --socket "$ST_TMP/absent.sock" 2>&1)
  rc=$?
  ck "退出码" 1 "$rc"
  ck "未拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  ck_log_contains "写明缺命令" "未配置 BK_AUTOSTART_CMD"

  echo "== 12. --status 只读：不写日志、不告警、不自动拉起"
  run_pgrep="$pgrep_hit"
  rm -f "$st/watchdog.log" "$marker"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$run_pgrep" BK_ENV_FILE="$env_dry" \
        BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 BK_AUTOSTART_CMD="$fake_autostart" \
        bash "$SELF" --status --autostart --state-dir "$st" --pidfile "$pidfile_live" --socket "$srv_sock" 2>&1)
  rc=$?
  ck "退出码 0" 0 "$rc"
  ck "不写日志" "no" "$([ -f "$st/watchdog.log" ] && echo yes || echo no)"
  ck "不拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  printf '%s\n' "$out" > "$ST_TMP/status.out"
  ck_contains "--status 打印模式" "$ST_TMP/status.out" "模式: dry"
  ck_contains "--status 打印未平仓" "$ST_TMP/status.out" "未平仓: 2 笔"

  # ── 发现路径（不带 --pidfile）：pgrep + 复核 ────────────────────────────────
  # 这一组是「没有 pidfile 的部署」的默认路径（内核根本不写 pidfile，所以这才是
  # 主线）。缺了它，复核门（pid_is_core）就没有任何用例真的走过——把复核整个短路
  # 掉自测也照样全绿，等于没有这道门。
  echo "== 13. 不带 --pidfile：pgrep 命中 + 复核通过 + socket 通 → 0（默认部署路径）"
  rm -f "$st/watchdog.log" "$marker"
  out=$(run 0 "无 pidfile 且健康" --repeat-sec 0 --state-dir "$st" --socket "$srv_sock" --no-notify)
  printf '%s\n' "$out" > "$ST_TMP/case13.out"
  ck_contains "判定内核在跑" "$ST_TMP/case13.out" "OK"
  ck "无停机告警" "0" "$(count_matches "$st/watchdog.log" '^\[停机告警\]')"
  ck "状态=up" "up" "$(state_get status)"

  echo "== 14. 不带 --pidfile：pgrep 未命中 → 1，且写明「未发现内核进程」"
  rm -f "$st/watchdog.log"
  run_pgrep="$pgrep_none"
  out=$(run 1 "无 pidfile 且没进程" --repeat-sec 0 --state-dir "$st" --socket "$ST_TMP/absent.sock" --no-notify)
  ck_log_contains "写明 pgrep 未命中" "pgrep -f $pgrep_none 未发现内核进程"
  run_pgrep="$pgrep_hit"

  echo "== 15. 复核门真的会拦：pgrep 命中但复核不认 → 判为不在跑（变异实验的靶子）"
  rm -f "$st/watchdog.log"
  run_verify="$pgrep_none"
  out=$(run 1 "复核不认" --repeat-sec 0 --state-dir "$st" --socket "$srv_sock" --no-notify)
  run_verify=""
  ck_log_contains "写明复核不通过" "pgrep 命中的 pid"
  ck_log_contains "写明命令行不匹配" "命令行不匹配内核"

  echo "== 16. 两个内核 + socket 通（#199 最隐蔽的形态）→ 3 + 重复内核告警 + 标记 + 拒绝自动拉起"
  rm -f "$st/watchdog.log" "$marker" "$st/DUPLICATE_CORES"
  /usr/bin/tail -f "$fake_tag" &
  dup_extra=$!
  ST_PIDS="$ST_PIDS $dup_extra"
  disown 2>/dev/null || true
  sleep 0.3
  out=$(run 3 "重复内核" --autostart --repeat-sec 900 --state-dir "$st" --socket "$srv_sock" --no-notify)
  printf '%s\n' "$out" > "$ST_TMP/case16.out"
  ck "重复内核标记存在" "yes" "$([ -f "$st/DUPLICATE_CORES" ] && echo yes || echo no)"
  ck "状态=up_duplicate" "up_duplicate" "$(state_get status)"
  ck_contains "标题是重复内核告警（不是停机告警）" "$ST_TMP/case16.out" "重复内核告警"
  ck_contains "写明预期 1 个" "$ST_TMP/case16.out" "预期 1 个"
  ck_contains "写明 #199 风险" "$ST_TMP/case16.out" "issue #199"
  ck_contains "逐 pid 列出" "$ST_TMP/case16.out" "- pid "
  ck_contains "给出止损命令" "$ST_TMP/case16.out" "blitzkrieg stop"
  ck "重复内核不自动拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  ck "socket 通也照报（这就是重点）" "up_duplicate" "$(state_get status)"
  out=$(run 3 "重复内核去抖" --repeat-sec 900 --state-dir "$st" --socket "$srv_sock" --no-notify)
  ck "去抖：告警仍只有 1 条" "1" "$(grep -c '^\[重复内核告警\]' "$st/watchdog.log")"
  ck "停机告警一条都没有" "0" "$(grep -c '^\[停机告警\]' "$st/watchdog.log")"

  echo "== 17. --status 也要能发现重复内核（调度器可编程信号：退出码 3）"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$run_pgrep" BK_ENV_FILE="$env_dry" \
        BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 \
        bash "$SELF" --status --state-dir "$st" --socket "$srv_sock" 2>&1)
  rc=$?
  ck "--status 退出码 3" 3 "$rc"
  printf '%s\n' "$out" > "$ST_TMP/case17.out"
  ck_contains "--status 报 DUPLICATE" "$ST_TMP/case17.out" "DUPLICATE"
  ck_contains "--status 给出止损" "$ST_TMP/case17.out" "blitzkrieg stop"

  echo "== 18. 重复内核消失 → 0，标记清掉，且不误报「栈已恢复」"
  kill -TERM "$dup_extra" 2>/dev/null || true
  sleep 0.5
  rm -f "$st/watchdog.log"
  out=$(run 0 "重复内核解除" --repeat-sec 900 --state-dir "$st" --socket "$srv_sock" --no-notify)
  ck "标记已清掉" "no" "$([ -f "$st/DUPLICATE_CORES" ] && echo yes || echo no)"
  ck "状态回到 up" "up" "$(state_get status)"
  ck_log_contains "记了解除" "重复内核已解除"
  ck "不再报重复" "0" "$(count_matches "$st/watchdog.log" '^\[重复内核告警\]')"

  echo "== 19. pidfile 判死但 pgrep 复核发现内核在跑 → 不自动拉起（别亲手造重复内核）"
  rm -f "$marker" "$st/watchdog.log"
  out=$(run 1 "pidfile 过期" --autostart --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_dead" --socket "$ST_TMP/absent.sock")
  ck "未拉起" "no" "$([ -f "$marker" ] && echo yes || echo no)"
  ck_log_contains "说明跳过原因" "pidfile 判死但 pgrep 复核发现内核在跑"
  ck_log_contains "告警里点出 pidfile 可能过期" "pidfile 可能过期"

  echo "== 20. 僵尸进程（kill -0 会成功）→ 必须判为不在跑，文案写明僵尸"
  zpid=""
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$ST_TMP/zombie.pid" <<'PY' >/dev/null 2>&1 &
import os, sys, time
pid = os.fork()
if pid == 0:
    os._exit(0)          # 立刻退出，父进程不 wait → 僵尸
with open(sys.argv[1], "w") as f:
    f.write(str(pid))
time.sleep(120)
PY
    zombie_holder=$!
    ST_PIDS="$ST_PIDS $zombie_holder"
    disown 2>/dev/null || true
    i=0
    while [ ! -s "$ST_TMP/zombie.pid" ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
    zpid=$(cat "$ST_TMP/zombie.pid" 2>/dev/null || true)
  fi
  if [ -z "$zpid" ]; then
    echo "  SKIP: 造不出僵尸进程（没有 python3 或 fork 失败）——本条未覆盖"
  else
    zstate=$(ps -ww -o state= -p "$zpid" 2>/dev/null | tr -d '[:space:]')
    case "$zstate" in
      Z*)
        ck "僵尸的 kill -0 仍然成功（所以必须靠 state=Z 拒收）" 0 "$(kill -0 "$zpid" 2>/dev/null; echo $?)"
        rm -f "$st/watchdog.log"
        out=$(run 1 "僵尸 pidfile" --repeat-sec 0 --state-dir "$st" --pidfile "$ST_TMP/zombie.pid" --socket "$ST_TMP/absent.sock" --no-notify)
        ck_log_contains "文案写明僵尸" "僵尸进程"
        ck_log_contains "判为不在跑" "判为不在跑" ;;
      *)
        echo "  SKIP: pid $zpid 的 state='$zstate' 不是 Z（刚被回收？）——本条未覆盖" ;;
    esac
  fi

  echo "== 21. 长命令行 + pty：判定与『ps 的显示宽度』无关（冒烟，不是 ps -ww 的守卫——见注释）"
  # 诚实说明本条的射程：把 pid_is_core 里的 `ps -ww` 改回 `ps`，本条**不会**变红
  # （变异实验实测）。原因见 alive() 上方的测量：BSD ps 只在它自己的 stdout 是终端时
  # 才按宽度截断，而本脚本的 ps 一律经管道取，所以两边的结果相同。本条仍然有价值：
  # 它端到端跑的是「长 argv（tag 在 160 字符之后）+ pty 上的 --status」，能抓到
  # 复核/argv 解析整条链断掉（例如 pid_is_core 恒 false——变异 M2 下本条确实变红）。
  # 下面那句 INFO 把测量本身记录下来，免得后人再把它误当成已修好的 bug。
  long_tag="$ST_TMP/fake-blitzkrieg-core-long-$$"
  : > "$long_tag"
  pad=$(python3 -c 'print("x" * 160)')
  python3 -c 'import time; time.sleep(120)' "$pad" "$long_tag" &
  long_pid=$!
  ST_PIDS="$ST_PIDS $long_pid"
  disown 2>/dev/null || true
  sleep 0.3
  if ! ps -ww -o command= -p "$long_pid" 2>/dev/null | grep -qF -- "$long_tag"; then
    echo "  SKIP: 造不出长命令行 fixture（ps -ww 不认这个 pid）——本条未覆盖"
  else
    # 非 pty 下先确认判定本身正确（argv 里 tag 在 160 个字符之后）
    run_pgrep="$long_tag"
    out=$(run 0 "长命令行（无 pty）" --repeat-sec 0 --state-dir "$st" --socket "$srv_sock" --no-notify)
    printf '%s\n' "$out" > "$ST_TMP/case21.out"
    ck_contains "长命令行内核被认出来（复核/argv 链路可用）" "$ST_TMP/case21.out" "OK"
    script_form=""
    if command -v script >/dev/null 2>&1; then
      p=$(script -q /dev/null /bin/echo pty-probe-ok 2>/dev/null)
      case "$p" in *pty-probe-ok*) script_form=bsd ;; esac
      if [ -z "$script_form" ]; then
        p=$(script -q -c "/bin/echo pty-probe-ok" /dev/null 2>/dev/null)
        case "$p" in *pty-probe-ok*) script_form=gnu ;; esac
      fi
    fi
    case "$SELF$ST_TMP$ROOT" in
      *" "*) script_form="" ;;
    esac
    if [ -z "$script_form" ]; then
      echo "  SKIP: 没有可用的 script(1)（或路径含空格）——pty 路径未覆盖；本机应人工验证"
    else
      inner="stty cols 60 2>/dev/null; exec env BK_REPO_ROOT='$ROOT' BK_CORE_PGREP='$long_tag' BK_ENV_FILE='$env_dry' BK_POSITIONS='$pos' BK_SETTLEMENTS='$settle' BK_NOTIFY=0 bash '$SELF' --status --state-dir '$st' --socket '$srv_sock'"
      # 同一个 pty 里量一次「ps 的输出宽度」：本脚本经管道取 ps，两个数应当相等。
      # 只打印，不断言——GNU ps 本来就不截断，把它写成断言会在 Linux CI 上变成假红。
      probe="stty cols 60 2>/dev/null; echo INFO-ps=\$(ps -o command= -p $long_pid | wc -c | tr -d ' '); echo INFO-psww=\$(ps -ww -o command= -p $long_pid | wc -c | tr -d ' ')"
      if [ "$script_form" = "bsd" ]; then
        out=$(script -q /dev/null /bin/sh -c "$inner" 2>&1)
        pty_info=$(script -q /dev/null /bin/sh -c "$probe" 2>&1 | tr -d '\r')
      else
        out=$(script -q -c "/bin/sh -c '$inner'" /dev/null 2>&1)
        pty_info=$(script -q -c "/bin/sh -c '$probe'" /dev/null 2>&1 | tr -d '\r')
      fi
      # `script -q` 会把 pty 上的 EOF 字符渲染成字面量 `^D` 加两个退格，粘在**第一行**
      # 输出前面（实测 od -c: `^ D \b \b I N F O - p s = 3 9`），所以这里用 `.*` 前缀
      # 匹配、只取数字，别用 `^INFO-ps=` 锚定（否则第一行永远解析不到，打印成 `?`）。
      info_a=$(printf '%s\n' "$pty_info" | sed -n 's/.*INFO-ps=\([0-9][0-9]*\).*/\1/p' | head -1)
      info_b=$(printf '%s\n' "$pty_info" | sed -n 's/.*INFO-psww=\([0-9][0-9]*\).*/\1/p' | head -1)
      printf '  INFO: pty 下 ps 输出 %s 字节 / ps -ww %s 字节（该进程 argv 长度 >160；本脚本经管道取 ps，两者相等即未截断）\n' \
        "${info_a:-?}" "${info_b:-?}" >&2
      printf '%s\n' "$out" > "$ST_TMP/case21pty.out"
      # 注意断言串带 `（pid`：`RUNNING` 是 `NOT RUNNING` 的子串，只断言 RUNNING
      # 等于没断言（变异 M2 下这条会假绿）。
      ck_contains "pty（60 列）下仍认得出内核" "$ST_TMP/case21pty.out" "RUNNING（pid"
      ck "pty 下不误判为不在跑" "0" "$(count_matches "$ST_TMP/case21pty.out" 'NOT RUNNING')"
    fi
    run_pgrep="$pgrep_hit"
  fi

  echo "== 22. 复核是子串匹配：「只在命令行里提到内核路径」的进程（构建 shell）不能把重复内核信号喊成狼来了"
  # 本机实测：默认串 target/release/blitzkrieg-core 命中了 4 个 `zsh -c … cargo build …`
  # / `ls -la target/release/blitzkrieg-core` 的构建 shell——真机上 --status 于是报
  # 「4 个内核进程」。这条路**不改默认判定**（收紧发现串会让不带 --socket 手工起的真内核
  # 被漏掉，漏报正是 issue #211 的原病），改为：(a) 告警里逐个 pid 点名「argv 既无 --mode
  # 也无 --socket，可能只是提到路径的进程」；(b) 部署方可用 BK_CORE_PGREP 收紧成
  # "<路径>.*--socket"。本用例把这两条都钉住。
  mention_tag="$ST_TMP/mention-blitzkrieg-core-$$"
  : > "$mention_tag"
  # 只在 argv 的**后面**提到路径（模拟 `ls -la <路径>`），既没有 --mode 也没有 --socket
  python3 -c 'import time; time.sleep(120)' "$mention_tag" &
  mention_pid=$!
  ST_PIDS="$ST_PIDS $mention_pid"
  disown 2>/dev/null || true
  sleep 0.3
  if [ -z "$(ps -ww -o command= -p "$mention_pid" 2>/dev/null)" ]; then
    echo "  SKIP: 造不出「只是提到路径」的 fixture——本条未覆盖"
  else
    # (a) 松串（= 现状默认语义）：两个这样的进程 + 通着的 socket → 仍然报重复（保守），
    #     但正文必须点出可疑，人一看就知道该核对什么。
    python3 -c 'import time; time.sleep(120)' "$mention_tag" &
    mention2_pid=$!
    ST_PIDS="$ST_PIDS $mention2_pid"
    disown 2>/dev/null || true
    sleep 0.3
    run_pgrep="$mention_tag"
    out=$(run 3 "松串 + 只提到路径的 2 个进程 → 报重复（保守，缺 --socket 也数）" --repeat-sec 0 --state-dir "$st" --socket "$srv_sock" --no-notify)
    printf '%s\n' "$out" > "$ST_TMP/case22.out"
    ck_contains "正文逐条点名可疑" "$ST_TMP/case22.out" "可疑：argv 里既没有 --mode 也没有 --socket"
    ck_contains "正文写明判别口径" "$ST_TMP/case22.out" "子串匹配"
    ck_contains "正文给出收紧办法" "$ST_TMP/case22.out" "BK_CORE_PGREP"
    # (b) 收紧串（内核一定带 --socket，supervisor 每次都显式传）→ 同样的进程一个都不算。
    run_pgrep="$mention_tag.*--socket"
    out=$(run 1 "收紧串 → 不算内核（不会用构建 shell 喊重复内核）" --repeat-sec 0 --state-dir "$st" --socket "$ST_TMP/absent.sock" --no-notify)
    printf '%s\n' "$out" > "$ST_TMP/case22b.out"
    ck "收紧串下不报重复" "0" "$(count_matches "$ST_TMP/case22b.out" '重复内核')"
    ck_contains "收紧串下写明未发现内核" "$ST_TMP/case22b.out" "未发现内核进程"
    run_pgrep="$pgrep_hit"
  fi

  # ── 23. 备份新鲜度（issue #217）────────────────────────────────────────────
  # 本条要证明的是「内核在跑 + 备份陈旧」这一组合**会自己喊出来**，而且喊完之后
  # 能恢复（KI-30：一个永远红着的检查与一个永远不响的检查是同一个缺陷）。
  # 判定引擎是可注入的替身（见 fixture 段的注释），所以这些断言完全不依赖备份卷。
  echo "== 23. 备份新鲜度（#217）：内核在跑 + 备份陈旧 → 退出 4 + BACKUP_STALE 标记 + 可恢复"
  # 备份用例的 runner：内核「在跑且 socket 通」（与用例 6 同一套 fixture），判定引擎
  # 由 bk_engine 注入。要造「内核停机 + 备份陈旧」的组合就用 $common 那条路。
  local bk_engine="$bk_ok" bk_state="$st/backup-state"
  run_up() { # run_up <期望退出码> <描述> [参数...]
    local want=$1 desc=$2; shift 2
    out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" \
          BK_BACKUP_CHECK=1 BK_BACKUP_STATUS_SH="$bk_engine" \
          BK_ENV_FILE="$env_dry" BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 \
          BK_AUTOSTART_CMD="$fake_autostart" \
          bash "$SELF" --state-dir "$st" --pidfile "$pidfile_live" --socket "$srv_sock" "$@" 2>&1)
    rc=$?
    ck "$desc 退出码" "$want" "$rc"
    printf '%s\n' "$out"
  }

  # (a) 干净起点：备份新鲜 → 退出 0、没有标记、状态文件记 ok。
  rm -f "$st/watchdog.log" "$st/BACKUP_STALE" "$bk_state"
  out=$(run_up 0 "备份新鲜")
  ck "新鲜时不写标记" "no" "$([ -f "$st/BACKUP_STALE" ] && echo yes || echo no)"
  ck "新鲜时状态=ok" "ok" "$(sed -n 's/^status=//p' "$bk_state" 2>/dev/null | head -1)"

  # (b) 翻转成陈旧：退出码必须是 4（而不是 0），告警正文写进日志，标记落盘。
  bk_engine="$bk_bad"
  rm -f "$st/watchdog.log"
  out=$(run_up 4 "内核在跑 + 备份陈旧")
  printf '%s\n' "$out" > "$ST_TMP/case23b.out"
  ck_contains "告警标题" "$st/watchdog.log" "备份告警"
  ck_contains "正文含判定行" "$st/watchdog.log" "light=STALE(97h)"
  ck_contains "正文含容忍值" "$st/watchdog.log" "容忍: light 26h / full 192h"
  ck_contains "正文给出诊断命令" "$st/watchdog.log" "blitzkrieg backup --status"
  ck_contains "正文说明与内核存活无关" "$st/watchdog.log" "内核存活与数据安全是两件事"
  ck_contains "正文不提自动拉起内核（无关的话不说）" "$st/watchdog.log" "绝不代替你安装或修改任何 LaunchAgent"
  ck "BACKUP_STALE 标记存在" "yes" "$([ -f "$st/BACKUP_STALE" ] && echo yes || echo no)"
  ck "状态=bad" "bad" "$(sed -n 's/^status=//p' "$bk_state" | head -1)"
  ck_contains "stdout 也说明备份不新鲜" "$ST_TMP/case23b.out" "备份: 不新鲜"

  # (c) 去抖：同一状态再来一次不重复告警；窗口到期（--repeat-sec 0）才再喊。
  #     计数用带锚点的 `^[备份告警]`（= 每次告警的正文标题，一条一次）：正文里每行
  #     都是独立的日志行，数「出现过这个词」会把一次告警数成两次。
  out=$(run_up 4 "去抖中" )
  ck "告警仍只有 1 条" "1" "$(count_matches "$st/watchdog.log" '^\[备份告警\]')"
  ck_contains "去抖写在日志里" "$st/watchdog.log" "备份仍不新鲜"
  out=$(run_up 4 "窗口到期" --repeat-sec 0)
  ck "窗口到期后再次告警" "2" "$(count_matches "$st/watchdog.log" '^\[备份告警\]')"

  # (d) 内核停机 + 备份陈旧：退出码是 1（停机优先），本条不单独发通知但**照样记**
  #     ——「不喊」不等于「不说」，标记与状态必须还在。
  rm -f "$st/watchdog.log" "$st/BACKUP_STALE"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$run_pgrep" BK_BACKUP_CHECK=1 BK_BACKUP_STATUS_SH="$bk_bad" \
        BK_ENV_FILE="$env_dry" BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 \
        BK_AUTOSTART_CMD="$fake_autostart" \
        bash "$SELF" --repeat-sec 0 --state-dir "$st" --pidfile "$pidfile_dead" --socket "$ST_TMP/absent.sock" 2>&1)
  rc=$?
  ck "停机优先于备份" "1" "$rc"
  ck "停机时不落备份告警正文" "0" "$(count_matches "$st/watchdog.log" '^\[备份告警\]')"
  ck_contains "但写明通知已抑制" "$st/watchdog.log" "内核停机中，本条通知已抑制"
  ck "标记照旧落盘" "yes" "$([ -f "$st/BACKUP_STALE" ] && echo yes || echo no)"

  # (e) 恢复：判定回到新鲜 → 标记被清掉、日志留一笔、退出码回到 0。
  bk_engine="$bk_ok"
  rm -f "$st/watchdog.log"
  out=$(run_up 0 "恢复新鲜")
  ck "标记已清掉" "no" "$([ -f "$st/BACKUP_STALE" ] && echo yes || echo no)"
  ck_contains "恢复写进日志" "$st/watchdog.log" "备份已恢复新鲜"
  ck "状态回到 ok" "ok" "$(sed -n 's/^status=//p' "$bk_state" | head -1)"

  # (f) 判定引擎缺失是**异常**，不是「跳过」（KI-30：改名一次就静默消失的检查不是通过）。
  bk_engine="$bk_gone"
  rm -f "$st/watchdog.log"
  out=$(run_up 4 "判定脚本缺失")
  printf '%s\n' "$out" > "$ST_TMP/case23f.out"
  ck_contains "点名缺失的脚本" "$st/watchdog.log" "$bk_gone"
  ck_contains "告警正文写明 unusable" "$ST_TMP/case23f.out" "判定: unusable"

  # (g) --status 也要能编程消费：内核在跑 + 备份陈旧 → 退出 4，并打印判定与容忍值。
  bk_engine="$bk_bad"
  rm -f "$st/BACKUP_STALE"   # (f) 的 check 模式落下的标记，这组要断言 --status 不再写
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_BACKUP_CHECK=1 BK_BACKUP_STATUS_SH="$bk_bad" \
        BK_ENV_FILE="$env_dry" BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" \
        bash "$SELF" --status --state-dir "$st" --pidfile "$pidfile_live" --socket "$srv_sock" 2>&1)
  rc=$?
  ck "--status 退出码 4" 4 "$rc"
  printf '%s\n' "$out" > "$ST_TMP/case23g.out"
  ck_contains "--status 打印备份判定" "$ST_TMP/case23g.out" "备份: light=STALE(97h)"
  ck_contains "--status 打印容忍值" "$ST_TMP/case23g.out" "备份容忍: light 26h / full 192h"
  # 只读契约：--status 不落标记（那条只属于 check 模式）。
  ck "--status 不写标记" "no" "$([ -f "$st/BACKUP_STALE" ] && echo yes || echo no)"

  # (h) 关掉检查（--no-backup-check / BK_BACKUP_CHECK=0）：判定引擎再坏也不报。
  #     这是给「这台机器根本没有备份调度」的部署留的出口，不是给 CI 方便的开关。
  bk_engine="$bk_bad"
  out=$(BK_REPO_ROOT="$ROOT" BK_CORE_PGREP="$fake_tag" BK_BACKUP_CHECK=0 BK_BACKUP_STATUS_SH="$bk_bad" \
        BK_ENV_FILE="$env_dry" BK_POSITIONS="$pos" BK_SETTLEMENTS="$settle" BK_NOTIFY=0 \
        BK_AUTOSTART_CMD="$fake_autostart" \
        bash "$SELF" --state-dir "$st" --pidfile "$pidfile_live" --socket "$srv_sock" 2>&1)
  rc=$?
  ck "--no-backup-check 下退出 0" "0" "$rc"

  # 先统计再清理：failures/assertions 就在 $ST_TMP 里，st_cleanup 会把它们一起删掉。
  nfail=$(wc -l < "$failfile" 2>/dev/null | tr -d ' '); [ -n "$nfail" ] || nfail=0
  ncases=$(wc -l < "$asserts" 2>/dev/null | tr -d ' '); [ -n "$ncases" ] || ncases=0
  st_cleanup
  echo
  if [ "$nfail" -eq 0 ]; then
    echo "SELFTEST PASS（23 组用例 / $ncases 项断言）"
    return 0
  fi
  echo "SELFTEST FAIL（23 组用例 / $ncases 项断言，$nfail 项失败，见上面 FAIL 行）"
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
  else
    # 文案要指对处置：僵尸（等回收）与 pid 回收（陌生人占了号）是两码事。
    case "$(pid_reject_reason "$pid_candidate")" in
      zombie)
        PROC_DETAIL="pidfile $PIDFILE_CFG 里的 pid $pid_candidate 是僵尸进程（已退出、只等回收；state=Z）——判为不在跑" ;;
      mismatch)
        PROC_DETAIL="pidfile $PIDFILE_CFG 里的 pid $pid_candidate 活着，但命令行不匹配内核（${CORE_PGREP}）——判为不在跑" ;;
      *)
        PROC_DETAIL="pidfile $PIDFILE_CFG 里没有存活进程" ;;
    esac
  fi
else
  CORE_PID=$(pgrep -f "$CORE_PGREP" 2>/dev/null | head -1 || true)
  if [ -n "$CORE_PID" ] && ! pid_is_core "$CORE_PID"; then
    PROC_DETAIL="pgrep 命中的 pid $CORE_PID 命令行不匹配内核——判为不在跑"
    CORE_PID=""
  fi
  [ -n "$CORE_PID" ] || PROC_DETAIL="${PROC_DETAIL:-pgrep -f $CORE_PGREP 未发现内核进程}"
fi

# 2) 重复内核（issue #199）：与 up/down 无关的独立信号，只看「复核通过的内核进程有
#    几个」。候选一律来自 pgrep，且**每个候选都要过复核**（pid_is_core）——否则一个
#    命令行里恰好含这个字符串的编辑器/部署命令就会被算成内核，把「重复」误报出来。
#    多内核时 CORE_PID 取最小的 pid：这是**启发式**，只为有 argv 可读（mode/socket），
#    谁占着 socket 由内核自己决定、可能是后来者——所以结论是「有几个」，不是「哪一个」，
#    正文逐 pid 列出全部进程。
CORE_PIDS=""
for _p in $(core_pids_verified); do CORE_PIDS="$CORE_PIDS $_p"; done
if [ -n "$CORE_PID" ]; then
  case " $CORE_PIDS " in
    *" $CORE_PID "*) : ;;
    *) CORE_PIDS="$CORE_PID $CORE_PIDS" ;;
  esac
fi
CORE_COUNT=0
if [ -n "${CORE_PIDS# }" ]; then
  CORE_PIDS=$(printf '%s\n' $CORE_PIDS | sort -n | uniq | tr '\n' ' ' | sed 's/[[:space:]]*$//') # shellcheck disable=SC2086 # 有意分词
  CORE_COUNT=$(printf '%s\n' $CORE_PIDS | wc -l | tr -d ' ')
else
  CORE_PIDS=""
fi
PIDS_CSV=$(printf '%s' "$CORE_PIDS" | tr ' ' ',')

# pidfile 判死、而 pgrep 复核发现有内核在跑：配置与事实的分歧，必须说出来——一个
# 过期的 pidfile 会让这个看门狗长期误报停机，而人只会以为「告警又抽风了」。
# 注意 `${VAR}；` 的花括号：bash 3.2 会把紧跟变量名的多字节 UTF-8 字符吃进变量名
# （`$PROC_DETAIL；但…` → `PROC_DETAIL\xef: unbound variable`），而 set -u 下这是
# 直接退出。本文件里「变量名后面直接跟中文标点」的地方一律用 ${} 包起来。
if [ -n "$PIDFILE_CFG" ] && [ -z "$CORE_PID" ] && [ -n "$CORE_PIDS" ]; then
  PROC_DETAIL="${PROC_DETAIL}；但 pgrep 复核发现 ${CORE_COUNT} 个内核进程在跑（pid ${PIDS_CSV}）——pidfile 可能过期"
fi

# 3) argv（socket 与 mode 的权威来源，进程不在时为空）。多内核时读的是 CORE_PID
#    那个（最小 pid）；其余内核各自的模式/socket 在重复告警正文里逐个列出。
CORE_ARGV=""
[ -n "$CORE_PID" ] && CORE_ARGV=$(ps -ww -o command= -p "$CORE_PID" 2>/dev/null)

# 4) 两个证据：进程 + socket
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
  LAST_UP="$NOW"
fi
# 重复内核优先于 up/down：它不是「栈的第三种状态」，而是另一件事（#199 的第二个
# 内核在写同一份账本/订单库）。只要复核通过的内核 ≥2 个就报它——**包括 socket 通、
# 一切都「正常」的那种最有欺骗性的形态**。
if [ "$CORE_COUNT" -ge 2 ]; then
  NEW_STATUS=up_duplicate
elif [ "$STACK_UP" -eq 1 ]; then
  NEW_STATUS=up
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
# 止损命令（只对重复内核有意义）：`blitzkrieg stop` 按 socket 路径匹配 blitzkrieg
# 家族进程（ui/ui_kit_panel/src/stop_stack.rs），跨 socket 的内核要逐个停。
STOP_RECOVERY="cd \"$ROOT\" && blitzkrieg stop"
AUTOSTART_NOTE="（默认不会自动拉起内核；live 模式下永不自动拉起——见 README.md §3.5）"

alert_body() {
  printf '[停机告警] BlitzkriegBot 交易内核不在运行\n'
  if [ "$NEW_STATUS" = "down_wedged" ]; then
    printf '状态: 进程在（pid %s，来源 %s）但 socket 不通：%s（探针: %s）\n' "$CORE_PID" "$([ -n "$SOCKET_CFG" ] && echo "--socket 参数" || echo "内核 argv")" "$CORE_SOCKET" "$SOCK_RESULT"
  else
    printf '状态: 进程不在（%s）%s\n' "${PROC_DETAIL:-未发现内核进程}" "$SOCK_NODE_STALE"
  fi
  printf '模式: %s（来源：%s）\n' "$MODE_VALUE" "$MODE_SOURCE"
  printf '未平仓: %s\n' "$POS_TEXT"
  printf '未赎回应收: %s\n' "$SETTLE_TEXT"
  printf '最后已知存活: %s\n' "$(last_alive_text)"
  printf '恢复命令: %s\n' "$RECOVERY"
  printf '日志: %s\n' "$LOG"
  printf '%s\n' "$AUTOSTART_NOTE"
}

# 重复内核告警正文：**自带完整解释**——它不再寄生在停机告警里，可能单独出现在日志、
# 通知或 --status 输出里，所以风险、全部 pid、各自的模式与 socket、止损命令都要写全，
# 看到这一条的人不需要再去翻别的上下文。
duplicate_body() {
  printf '[重复内核告警] 发现 %s 个 BlitzkriegBot 内核进程（预期 1 个）\n' "$CORE_COUNT"
  printf '风险: 第二个内核会写同一份账本与订单库（data/positions、data/orders），造成重复下单或持仓口径错乱（issue #199）；socket 通、日志干净、退出码 0 —— 这正是它唯一的表现形态\n'
  printf '内核进程（命令行复核通过）:\n'
  pids_detail_text
  printf '判别口径: 命令行里出现「%s」即算命中（子串匹配，pgrep -f 的语义）——只提到该路径的进程也会被算进来；要更严就把 BK_CORE_PGREP 收紧成 "%s.*--socket"（内核一定带这个参数）\n' "$CORE_VERIFY_PGREP" "$CORE_VERIFY_PGREP"
  printf 'socket: %s %s\n' "$CORE_SOCKET" "$SOCK_STATE"
  printf '模式: %s（来源：%s；多内核时以 pid %s 的 argv 为准，其余见上）\n' "$MODE_VALUE" "$MODE_SOURCE" "${CORE_PID:-${PIDS_CSV}}"
  printf '未平仓: %s\n' "$POS_TEXT"
  printf '未赎回应收: %s\n' "$SETTLE_TEXT"
  printf '最后已知存活: %s\n' "$(last_alive_text)"
  printf '止损命令: %s   # 停完用本脚本 --status 确认回到 0\n' "$STOP_RECOVERY"
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
# 重复内核的通知正文，独立一份：喊人时最该看到的是「几个内核」和「先停」。
dup_short_line() {
  printf '发现 %s 个内核进程（pid %s）｜模式 %s｜未平仓 %s｜止损: blitzkrieg stop' \
    "$CORE_COUNT" "$PIDS_CSV" "$MODE_VALUE" "$(render_frag "$POS_FRAG")"
}

# ── --status：只读一次，不告警、不落状态、不自动拉起 ────────────────────────
if [ "$CHECK_MODE" = "status" ]; then
  if [ "$CORE_COUNT" -ge 2 ]; then
    # 重复内核在 --status 里也必须是可编程信号（退出码 3），不能显示成「一切正常」：
    # 调度器/人如果只看这一行，看到的必须是与事故一致的结论。
    printf '内核: DUPLICATE（%s 个内核进程在跑，预期 1 个；pid %s）\n' "$CORE_COUNT" "$PIDS_CSV"
    printf '风险: 第二个内核会写同一份账本/订单库（issue #199）\n'
    printf '判别口径: 命令行里出现「%s」即算命中（子串匹配）——只在命令行里提到该路径的进程也会被算进来，处置前逐条核对 argv\n' "$CORE_VERIFY_PGREP"
    printf '止损命令: %s\n' "$STOP_RECOVERY"
  else
    printf '内核: %s\n' "$([ "$STACK_UP" -eq 1 ] && echo "RUNNING（pid ${CORE_PID:-?}）" || echo "NOT RUNNING")"
  fi
  printf '进程: %s %s\n' "$PROC_STATE" "${PROC_DETAIL:-}"
  printf 'socket: %s %s\n' "$CORE_SOCKET" "$SOCK_STATE"
  printf '模式: %s（%s）\n' "$MODE_VALUE" "$MODE_SOURCE"
  printf '未平仓: %s\n' "$POS_TEXT"
  printf '未赎回应收: %s\n' "$SETTLE_TEXT"
  printf '最后已知存活: %s\n' "$(last_alive_text)"
  printf '状态目录: %s\n' "$STATE_DIR"
  # 备份新鲜度（issue #217）：--status 只报告，不写标记、不落状态、不通知，
  # 与这个模式「只读一次」的契约一致。判定引擎能容忍自己写不了东西。
  backup_verdict
  printf '备份: %s\n' "$BK_BK_NOTE"
  [ -n "$BK_BK_DETAIL" ] && printf '备份原因: %s\n' "$BK_BK_DETAIL"
  printf '备份容忍: light %sh / full %sh（BK_BACKUP_STALE_HOURS / BK_BACKUP_FULL_STALE_HOURS）\n' \
    "$BK_BACKUP_STALE_HOURS" "$BK_BACKUP_FULL_STALE_HOURS"
  printf '恢复命令: %s\n' "$RECOVERY"
  # 退出码优先级与 check 模式同一套：重复内核 > 停机 > 备份不新鲜 > 正常。
  [ "$CORE_COUNT" -ge 2 ] && exit 3
  [ "$STACK_UP" -eq 1 ] && { backup_bad && exit 4; exit 0; }
  exit 1
fi

# ── 状态机与告警 ────────────────────────────────────────────────────────────
SILENCED=0
if [ "$SILENCE" = "1" ] || [ -f "$STATE_DIR/silence" ]; then SILENCED=1; fi

# 备份新鲜度（issue #217）：与 up/down 无关的第二条独立信号。内核活着、备份却停在
# 三天前，是本条要抓的组合——「栈看起来健康，数据没有保障」。判定委托给
# data-backup.sh --status（唯一实现），这里只做三件事：出声、落标记、定退出码。
# 去抖用自己的一对字段（$BACKUP_STATE_FILE），理由写在标记路径的定义处。
backup_verdict
BK_BAD=0
backup_bad && BK_BAD=1
if [ "$BACKUP_CHECK" = "1" ]; then
  # 标记文件反映「最近一次取到的判定」。检查被关掉时不写也不清——那种情况下我们
  # 没有判定，写是谎报、清也是谎报；`--status` 会把 off 说出来。
  if [ "$BK_BAD" -eq 1 ]; then : > "$BACKUP_MARKER" 2>/dev/null || true
  else rm -f "$BACKUP_MARKER" 2>/dev/null || true
  fi
fi
if [ "$BK_BAD" -eq 1 ]; then
  BK_PREV=$(backup_state_get status || true)
  BK_LAST_ALERT=$(backup_state_get last_alert_epoch || true)
  BK_ALERT=0; BK_REASON=""
  if [ "$BK_PREV" != "bad" ]; then
    BK_ALERT=1; BK_REASON="状态翻转（${BK_PREV:-无记录} → 备份不新鲜）"
  elif [ -z "$BK_LAST_ALERT" ]; then
    BK_ALERT=1; BK_REASON="无告警记录"
  elif [ $((NOW - BK_LAST_ALERT)) -ge "$REPEAT_SEC" ]; then
    BK_ALERT=1; BK_REASON="距上次告警 $((NOW - BK_LAST_ALERT))s ≥ ${REPEAT_SEC}s"
  else
    BK_REASON="去抖中（距上次告警 $((NOW - BK_LAST_ALERT))s < ${REPEAT_SEC}s）"
  fi
  if [ "$BK_ALERT" -ne 1 ]; then
    log_append "$(fmt_epoch "$NOW") 备份仍不新鲜（${BK_REASON}）: ${BK_BK_NOTE}"
    say "BACKUP 备份仍不新鲜（${BK_REASON}）: ${BK_BK_NOTE}"
    backup_state_put bad "${BK_LAST_ALERT:-0}" "$BK_BK_NOTE"
  elif [ "$NEW_STATUS" = "down" ] || [ "$NEW_STATUS" = "down_wedged" ]; then
    # 内核停机期间不单独发这条通知：停机通知已经把人叫来了，备份缺失是它的后果，
    # 再刷一条只会把 15 分钟的 4 条变成 8 条。日志与 BACKUP_STALE 标记照旧，退出码
    # 也仍是 1（停机优先）——「不喊」不等于「不说」。**不消费告警窗口**：内核恢复后
    # 若备份仍不新鲜，这条要在第一时间（而不是等下一个 REPEAT_SEC）说出口。
    log_append "$(fmt_epoch "$NOW") 备份不新鲜（${BK_REASON}）——内核停机中，本条通知已抑制（先修停机）: ${BK_BK_NOTE}"
    say "BACKUP 备份不新鲜（内核停机中，通知已抑制）: ${BK_BK_NOTE}"
    backup_state_put bad "${BK_LAST_ALERT:-0}" "$BK_BK_NOTE"
  elif [ "$SILENCED" -eq 1 ]; then
    log_append "$(fmt_epoch "$NOW") 备份告警（${BK_REASON}）——已静默（$STATE_DIR/silence）：不通知、不落告警正文；BACKUP_STALE 标记与退出码照常"
    printf '%s\n' "BACKUP 备份不新鲜（已静默：不通知，仅落日志与 BACKUP_STALE 标记）: ${BK_BK_NOTE}"
    backup_state_put bad "$NOW" "$BK_BK_NOTE"
  else
    {
      printf '%s 备份告警（%s）\n' "$(fmt_epoch "$NOW")" "$BK_REASON"
      backup_body
    } >> "$LOG" 2>/dev/null || true
    printf '%s\n' "$(backup_body)"
    notify_local "BlitzkriegBot: 备份不新鲜（issue #217）" "$(backup_short_line)"
    backup_state_put bad "$NOW" "$BK_BK_NOTE"
  fi
else
  # 恢复：只在从 bad 翻回来时记一笔，避免每分钟一行「备份正常」把日志淹掉。
  if [ "$(backup_state_get status || true)" = "bad" ]; then
    log_append "$(fmt_epoch "$NOW") 备份已恢复新鲜: ${BK_BK_NOTE}"
    say "OK 备份已恢复新鲜（${BK_BK_NOTE}）"
  fi
  backup_state_put "$([ "$BACKUP_CHECK" = "1" ] && echo ok || echo off)" 0 "$BK_BK_NOTE"
fi

# 重复内核的标记由「当前事实」决定：不是重复就删掉（是不是刚解除，各分支自己记日志）。
[ "$NEW_STATUS" != "up_duplicate" ] && rm -f "$DUP_MARKER" 2>/dev/null

if [ "$NEW_STATUS" = "up" ]; then
  write_state up "$LAST_UP" "${LAST_ALERT:-0}" "$(state_get last_alert_kind || true)"
  if [ "$PREV_STATUS" != "up" ]; then
    rm -f "$DOWN_MARKER" 2>/dev/null || true
    if [ "$PREV_STATUS" = "up_duplicate" ]; then
      # 从「两个内核」回到「一个内核」：这不是「栈恢复」——栈根本没停过，别误导。
      log_append "$(fmt_epoch "$NOW") 重复内核已解除: 现在只有 1 个内核进程（pid ${CORE_PID:-?}）在 $CORE_SOCKET 上服务（模式 ${MODE_VALUE}）"
      say "OK 重复内核已解除（pid ${CORE_PID:-?}，${CORE_SOCKET}）模式 ${MODE_VALUE}"
      if [ "$SILENCED" -eq 1 ]; then
        log_append "通知: 静默中，恢复通知已抑制"
      else
        notify_local "BlitzkriegBot: 重复内核已解除" "只剩 1 个内核（pid ${CORE_PID:-?}）｜模式 ${MODE_VALUE}｜未平仓 $(printf '%s' "$POS_TEXT" | head -c 60)"
      fi
    else
      log_append "$(fmt_epoch "$NOW") 栈已恢复: 内核 pid ${CORE_PID:-?} 在 $CORE_SOCKET 上服务（模式 ${MODE_VALUE}）"
      say "OK 内核存活（pid ${CORE_PID:-?}，${CORE_SOCKET}）模式 ${MODE_VALUE}"
      if [ "$PREV_STATUS" = "down" ] || [ "$PREV_STATUS" = "down_wedged" ]; then
        if [ "$SILENCED" -eq 1 ]; then
          log_append "通知: 静默中，恢复通知已抑制"
        else
          notify_local "BlitzkriegBot: 交易内核已恢复" "pid ${CORE_PID:-?}｜模式 ${MODE_VALUE}｜未平仓 $(printf '%s' "$POS_TEXT" | head -c 60)"
        fi
      fi
    fi
  else
    say "OK 内核存活（pid ${CORE_PID:-?}，${CORE_SOCKET}）"
  fi
  say "   模式 ${MODE_VALUE}｜未平仓 ${POS_TEXT}｜未赎回 ${SETTLE_TEXT}"
  # 内核在跑但备份不新鲜（issue #217）：退出码 4，而且**不是 0**。这正是「栈健康、
  # 数据没保障」唯一能被调度器看见的形状——把备份缺失折进「一切正常」就等于回到事故当天。
  if [ "$BK_BAD" -eq 1 ]; then
    say "   备份: 不新鲜（${BK_BK_NOTE}）——详见上方告警；查: blitzkrieg backup --status"
    exit 4
  fi
  exit 0
fi

# ── 重复内核（#199）：与 up/down 无关的一等信号，退出码 3 ────────────────────
if [ "$NEW_STATUS" = "up_duplicate" ]; then
  : > "$DUP_MARKER" 2>/dev/null || true
  ALERT=0
  REASON=""
  if [ "$PREV_STATUS" != "up_duplicate" ]; then
    ALERT=1
    REASON="状态翻转（${PREV_STATUS:-无记录} → 重复内核）"
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
      log_append "$(fmt_epoch "$NOW") 重复内核告警（${REASON}）——已静默（$STATE_DIR/silence）：不通知、不落告警正文；DUPLICATE_CORES 标记与退出码 3 照常"
      printf '%s\n' "DUPLICATE 发现 $CORE_COUNT 个内核进程（已静默：不通知，仅落日志与 DUPLICATE_CORES 标记；退出码仍为 3）"
    else
      {
        printf '%s 重复内核告警（%s）\n' "$(fmt_epoch "$NOW")" "$REASON"
        duplicate_body
      } >> "$LOG" 2>/dev/null || true
      printf '%s\n' "$(duplicate_body)"
      notify_local "BlitzkriegBot: 发现 $CORE_COUNT 个内核进程（重复内核）" "$(dup_short_line)"
    fi
    write_state up_duplicate "${LAST_UP:-}" "$NOW" up_duplicate
  else
    log_append "$(fmt_epoch "$NOW") 仍有 $CORE_COUNT 个内核进程（${REASON}）"
    say "DUPLICATE 仍有 $CORE_COUNT 个内核进程（${REASON}）"
    write_state up_duplicate "${LAST_UP:-}" "${LAST_ALERT:-0}" "$(state_get last_alert_kind || true)"
  fi
  # 自动拉起在这里是**反转**的处置：重复内核不是「没起来」，再拉一个只会更多。
  if [ "$AUTOSTART" != "1" ]; then
    log_append "自动拉起未开启（默认）：只告警，不动手"
  else
    log_append "自动拉起: 跳过——已发现 $CORE_COUNT 个内核进程（重复内核不是「没起来」，再拉只会更多；先 blitzkrieg stop）"
  fi
  exit 3
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
elif [ -n "$CORE_PIDS" ]; then
  # pidfile 判死、pgrep 却复核出内核在跑：这时拉起就是亲手造一个重复内核（#199）。
  log_append "自动拉起: 跳过——pidfile 判死但 pgrep 复核发现内核在跑（pid ${PIDS_CSV}）；拉起会变成重复内核，先核对 pidfile"
elif [ "$ALERT" -ne 1 ]; then
  log_append "自动拉起: 跳过——本次不在告警窗口内（避免在去抖窗口里反复拉起）"
else
  log_append "自动拉起: 已自动拉起（dry）: $AUTOSTART_CMD"
  nohup sh -c "$AUTOSTART_CMD" >> "$STATE_DIR/autostart.log" 2>&1 &
  disown 2>/dev/null || true
  say "已自动拉起（dry）: $AUTOSTART_CMD"
fi

exit 1
