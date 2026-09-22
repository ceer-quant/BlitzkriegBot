# BlitzkriegBot

> 开源量化交易系统 —— **纯 Rust 交易核心 + Rust 面板 + 可插拔市场扩展**。
> 所有涉及资金、订单、风控与状态一致性的逻辑都在 Rust 侧；Node.js 仅在
> 本地验收门禁中作为驱动脚本使用（零依赖，仅标准库），不参与生产运行。

- **仓库**：`ceer-quant/BlitzkriegBot`（开源，MIT）
- **默认运行模式**：`dry`（DryRun 模拟）。**Live 交易默认关闭且受硬约束保护，不得在未授权下开启。**
- **工具链**：Rust（edition 2024）；Node.js `>= 22` 仅用于运行门禁与打包脚本（零 npm 依赖）

---

## 1. 它是什么

BlitzkriegBot 是一个面向**轮盘型预测市场（当前为 Polymarket 加密二元市场）**的自动化交易系统，
全部业务逻辑都在 Rust 侧：

- **Rust 核心**：确定性的撮合 / 下单 / 持仓 / 风控 / 对账状态机，单二进制，内存安全，无市场 SDK 依赖。
- **市场扩展**：核心市场无关；具体交易所通过扩展 crate 实现，并以 Cargo feature 挂接。官方默认扩展为
  **Polymarket**（CLOB 下单、WS 行情、Gamma 轮盘发现）。
- **策略层**：核心侧 0 策略（无硬编码内建策略），所有交易策略完全通过 `user_layer/strategy_api` 的 C ABI v2 规范在运行时
  动态加载（cdylib，feature `strategy-loading`），热插拔且分发时不捆绑任何策略。
- **Rust 面板**：`ui_kit_web` 监听 `127.0.0.1:51888`，托管 Vue 前端（`ui/webapp/webui/`）并直接
  提供 `/api/snapshot`、`/api/command`、`/api/plugins`、`/api/login`；`--manage` 模式下可
  拉起 / 停止核心并托管其生命周期。单二进制启动器 **`blitzkrieg`**（E12）把
  「核心 + 面板 + 生命周期托管」合成一条命令（见 §3.1）。

核心与面板之间通过本机 **Unix Domain Socket + 换行分帧的 JSON-RPC 2.0** 通信，
契约唯一来源是 Rust 的 serde 结构体；门禁驱动端 `scripts/lib/core-client.mjs`
为零依赖 bare-Node 客户端，只做传输，不做二次校验。

---

## 2. 目录结构（Cargo workspace）

```
BlitzkriegBot/
├── core/
│   ├── blitzkrieg_core/   # 交易核心二进制 blitzkrieg-core（市场无关）
│   └── market_api/        # 市场扩展契约：DataFeed / Discovery / Executor / MarketPlugin
├── extensions/
│   └── polymarket/        # 官方 Polymarket 扩展（默认 feature 挂接）
├── user_layer/
│   ├── strategy_api/      # 用户策略 trait / FFI 稳定表面
│   └── strategies/        # 示例动态策略（独立的嵌套 workspace，产出 cdylib）
├── ui/
│   ├── ui_kit/            # Rust UI 套件（bin: ui_kit_web —— 面板 HTTP 服务器）
│   ├── ui_kit_panel/      # 终端面板（bin: ui_kit_panel）+ 单二进制启动器（bin: blitzkrieg）
│   └── webapp/            # Vue 前端源码（webui/）与构建产物
├── scripts/               # 门禁与运维脚本（cycle-check / core-parity / secret-scan …；lib/ 为零依赖 IPC 驱动客户端）
├── docs/                  # 可公开文档（Rust 体系）
├── dev-docs/              # 内部开发文档（不入库，不上 GitHub）
└── Cargo.toml             # 工作区根（共享 target/，产物固定在根 target/）
```

> 生产二进制路径固定为仓库根的 `target/release/blitzkrieg-core`；移动成员 crate 不改变该路径。

### 核心模块（`core/blitzkrieg_core/src`）

| 模块 | 职责 |
| --- | --- |
| `engine` / `ome` | 自驱动引擎与订单撮合状态机 |
| `order` / `order_db` | 下单意图、挂单生命周期与落库 |
| `position` / `position_db` | 持仓、重估与恢复 |
| `risk` / `risk_context` | 下单前风控（名义额、持仓数等硬限制） |
| `exit_policy` | 出场策略（止损、最小剩余时间等） |
| `reconcile` | 与交易所成交回报对账 |
| `ledger` / `trade_db` | 成交与决策审计台账 |
| `signal` / `scanner` / `marketdata` | 信号、轮盘扫描、行情归集 |
| `strategy_engine` | 内置策略与动态策略加载器 |
| `shadow` / `shadow_evolution` | 影子回放、近失（near-miss）样本与影子进化 |
| `sim` | DryRun 模拟成交与估值 |
| `ipc` | UDS + JSON-RPC 2.0 传输与 schema |
| `extension` / `market` | 扩展上下文与市场注册/托管 |

---

## 3. 快速开始

### 3.1 启动与停止（`blitzkrieg run` / `blitzkrieg stop`）

一次性安装（生成一个指向本仓库最新构建的 `blitzkrieg` 命令）：

```bash
cargo build --release --workspace --locked
bash scripts/install-blitzkrieg-shim.sh
```

之后**从任何目录**：

```bash
blitzkrieg run              # 一键启动：内核 + Web 面板 + 崩溃自愈
blitzkrieg run --tui        # 一键启动：内核 + TUI，不启动 Web
blitzkrieg run -tui         # 上面的容错短别名
blitzkrieg run --web        # 显式选择 Web 面板
blitzkrieg tui              # 直接显示 TUI（默认允许管理核心）
blitzkrieg tui --attach     # 只连接已有核心，不启动/停止核心
blitzkrieg web              # 只启动 Web 网关，不自动启动核心
blitzkrieg stop             # 一键停止：面板 + 内核一起收（重复执行无害）
nohup blitzkrieg run >> /tmp/blitzkrieg-run.log 2>&1 &   # Web 后台常驻
```

默认 Web 面板地址为 `http://127.0.0.1:51888`；`run --tui` 只占用终端，不监听 Web 端口。

要点：

- **`.env` 自动加载**：面板凭据、`HFT_*` 旋钮从当前目录的 `.env` 读入，无需
  `source`。已导出的环境变量永远优先于文件；文件值绝不打印。`.env` 已
  gitignore，字段清单见 `.env.example`。
- **`stop` 按 socket 范围工作**：只触碰挂在目标 socket 上的 blitzkrieg 家族进程——先
  SIGTERM 拥有者（由它级联收掉自己拉起的内核），再收孤儿内核；启动 stop 的 shell/
  包装器永远不会被碰。被接管的内核（面板只读的那种状态）也能由它一并停掉。
- **默认 dry 模式**，默认参数与 §3.3 的手写命令行一致（回合 900s、`--engine --feed-ws`、
  持仓/名义额上限等）；`HFT_*` 环境变量可覆盖（见 §8）。
- 面板凭据 `BLITZKRIEG_PANEL_USER` / `BLITZKRIEG_PANEL_PASSWORD` **两者都设置**时，网页
  命令动词可用；缺省时面板为纯查看（启动时打印提示）。
- `--readonly`：结构性只读——内核不构造出网桥梁，从根上禁止下单（不是逐入口拦截）。
- **服务器部署（局域网/远程访问）**：`.env` 里加一行 `BLITZKRIEG_PANEL_ADDR=0.0.0.0:51888`
  （或启动时 `--addr 0.0.0.0:51888`）即监听所有网卡。浏览器从 `http://<服务器IP>:51888`
  打开面板即可全功能使用——**同源请求天然放行**（Referer 的地址与请求 Host 一致），
  无需任何白名单；跨站来源一律 403。仅当面板被反向代理改写域名时才需要
  `--allowed-origin <url>`（可重复）或 `BLITZKRIEG_ALLOWED_ORIGINS`（逗号分隔）。
  登录凭据（`.env` 的 `BLITZKRIEG_PANEL_USER/PASSWORD`）在任何地址上都是必需的；
  请勿将面板暴露到不受信任的公网。
- 子命令面：`blitzkrieg [run|core|tui|web|stop|--help]`；`run --tui` / `run -tui` 只打开
  TUI，不启动 Web；`tui --attach` 仅监视现有核心，绝不杀死非本进程拉起的内核。
- `run` / `core` 直接吃引擎旋钮：`--round-sec`、`--min-round-age`、`--min-time-left`、
  `--max-positions`、`--min-shares`、`--max-shares` 优先于 `HFT_*` 环境变量。参数写错
  （`--tui` 与 `--web` 同给、`--manage` 搭配 `--attach`、未知 flag）会直接报错退出，
  绝不静默吞掉后按默认值起栈。
- 一次只跑一套栈：旧栈还占着 51888 / 默认 socket 时再 `run`，端口会绑不上；
  此时先 `blitzkrieg stop` 再起。
- shim 会在仓库移动后失效（路径烧死在生成物里）：重新跑一次安装脚本即可。

### 3.2 构建 Rust 核心

```bash
# 工作区全量构建（默认带 polymarket feature）
cargo build --release --workspace --locked

# 产物
ls target/release/blitzkrieg-core target/release/blitzkrieg
```

### 3.3 直接以 DryRun 跑核心

```bash
./target/release/blitzkrieg-core \
  --mode dry \
  --engine --feed-ws \
  --assets BTC,ETH,SOL,XRP \
  --round-sec 900 --min-round-age 30 --min-time-left 180 \
  --max-positions 2 --max-order-notional 6 \
  --seed-balance 1000 --tick-ms 50
```

`--mode` 取值 `dry`（默认）或 `live`。**未经明确授权不得使用 `live`。**
UDS 路径可用 `--socket` 覆盖（默认位于 `$TMPDIR` 下）。

常用开关：

| 标志 | 含义 |
| --- | --- |
| `--engine` | 启用自驱动引擎 |
| `--feed-ws` | 启用 Rust 原生行情源：Polymarket 走 REST `POST /books` 轮询，Binance 现货仍为 WS |
| `--assets` | 交易资产白名单（逗号分隔） |
| `--round-sec` / `--min-round-age` / `--min-time-left` | 轮盘周期与进场时间门限 |
| `--trend-confirm-sec` / `--trend-window-floor-ms` | 趋势确认时长 / 窗口下限 |
| `--max-positions` / `--max-order-notional` | 持仓数与单笔名义额硬上限 |
| `--seed-balance` / `--tick-ms` | DryRun 初始资金 / 引擎节拍 |
| `--no-discovery` / `--no-auto-exits` | 关闭自动发现 / 自动出场 |
| `--shadow-evolution` | 启用影子进化（相关参数 `--se-min-samples` 等） |
| `--replay` / `--replay-near-miss` | 回放行情 / 近失样本 |
| `--market-plugin` | 指定运行时市场插件 |
| `--config <path>` / `--no-config` | 指定/关闭配置文件（默认读 `user_layer/configs/default.toml`） |

#### 配置文件

`user_layer/configs/default.toml`（含同目录的 `shadow_evolution.toml`）**会被内核读取**。
生效优先级为 **命令行 > `BK_*` 环境变量 > 配置文件 > 代码默认值**，启动时会为每个
非默认值打印一行 `key=value (source)`，说明该值从哪一层来。因此上面示例里的
`--round-sec 900` 依然压过文件里的同名键。

关闭文件用 `--no-config` 或 `BK_CONFIG=none`；换一份用 `--config <path>` 或
`BK_CONFIG=<path>`。文件缺失不是错误（默认值生效）；文件写坏只告警不致命；
**文件里出现内核不认识的键会逐个告警**——不会静默忽略。

### 3.4 运行面板（Web UI）

```bash
cargo build --release -p blitzkrieg-ui-kit
./target/release/ui_kit_web --addr 127.0.0.1:51888 --manage
```

浏览器打开 `http://127.0.0.1:51888/panel`。`--manage`（或环境变量
`UIKIT_MANAGE=1`）允许面板拉起 / 停止核心；面板凭据来自环境变量
`BLITZKRIEG_PANEL_USER` / `BLITZKRIEG_PANEL_PASSWORD`，登录换会话 token。
（等价的分体式启动也可以用 `blitzkrieg core` + `blitzkrieg web --manage`。）

终端面板（可选）：`target/release/blitzkrieg tui`（或 `cargo run -p blitzkrieg-ui-panel`）。

### 3.5 重启之后：停机告警与自动拉起（issue #211）

**重启后会发生什么（当前事实，不是猜测）**：机器重启后**没有任何东西会把交易内核带回来**——
`launchctl list` 里只有 `com.blitzkrieg.databackup.*` 两个备份 job，内核与面板都得有人手动
`blitzkrieg run`。2026-09-21 的事故就是这样：内核从当日起不再运行，而**没有任何告警**。
根因不是「忘了写 agent」：本仓库在外置卷，macOS 拒绝 launchd 拉起的进程读/执行该卷
（实测 `read-volume: DENIED`、`exec-script: DENIED (exit=126)`），连 databackup 自己当初
都在以 `/bin/sh: .../data-backup-cli.sh: Operation not permitted` 失败（#217，已按 §3.6 修：
失败不再静默 + 内置盘 launcher + 备份新鲜度巡检）。所以这里把两半分开：

- **B 路线（已实现，安全的那一半）**：`scripts/stack-watchdog.sh`——判定 + 告警，**不拉起**。
- **A 路线（只有骨架，未启用）**：`scripts/com.blitzkrieg.stack-autostart.plist.disabled`。
  **安装它 = 无人值守地自动拉起内核，`DRY_RUN=false`（live）时也一样**——这与
  「live 只在用户在场时开」的约定冲突，所以是**用户决定、需要明确批准**，默认不安装。

**装 watchdog（B 路线）**

```bash
# 1) 先只读地看一眼：不告警、不落状态、不拉起（内核在跑则打印 RUNNING）
bash scripts/stack-watchdog.sh --status

# 2) 手工跑一次真检查：内核不在跑 → 退出 1，并落日志 + STACK_DOWN 标记 + 本地通知
bash scripts/stack-watchdog.sh

# 3) 装成「登录/重启后立刻一次 + 之后每 60 秒一次」（StartInterval=60, RunAtLoad）
cp scripts/com.blitzkrieg.stack-watchdog.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.blitzkrieg.stack-watchdog.plist
launchctl list | grep blitzkrieg     # 应当看到 com.blitzkrieg.stack-watchdog
```

退出码：`0` 唯一内核存活并在服务；`1` 内核不在跑 / socket 不可达（已告警）；`2` 用法或配置错误；
`3` **发现 ≥2 个内核进程（重复内核）**——见下面「重复内核」一节，它优先于 `1`。
（plist 里有两处烧死的本机路径——脚本路径与 `StandardOutPath`：检出不在
`/Volumes/Hard Disk/BlitzkriegBot`、或用户名不是 `fancer`，就改这两行。）

日志：`$STATE_DIR/watchdog.log`（默认 `~/Library/Logs/blitzkrieg-stack-watchdog/`，与 plist 的
stdout 同一份）；内核正在 down 时另有 `$STATE_DIR/STACK_DOWN` 标记，恢复后自动删除。
告警正文包含：当前模式（dry / live / readonly，来源一并写明）、**未平仓与未赎回应收**
（读 `data/` 的真实数字；读不到就写「无法判定」，绝不假装 0）、最后已知存活时间、可直接粘贴的
恢复命令。判定复用 `scripts/soak-resident.sh` 的 `alive()` 语义（pidfile + `kill -0` + 命令行匹配），
再加一个 UDS connect 探针，因此「进程不在」与「进程在但 socket 不通」在输出里是两句话。
命令行一律用 `ps -ww` 读、并**拒收僵尸**（`kill -0` 对僵尸是成功的，`state=Z` 时必须另外判死；
实测：BSD `ps` 只在自己的 stdout 是 tty 时才按终端宽度截断，脚本永远把 `ps` 接进管道，
所以 `-ww` 是防御性写法而不是现场 bug 的修复——但它保证有人手工在窄终端里复现时结论一致）。

**重复内核是独立的一等信号（issue #199）**

「有两个内核在跑」不是停机的一种，是**另一种事故形态**：两个内核会写同一份账本与订单库
（`data/positions`、`data/orders`），造成重复下单或持仓口径错乱。而它唯一的表现形态就是
**看起来一切正常**——socket 通、日志干净。所以判定与处置都不依附于 up/down：

- **退出码 `3`**（优先于 `1`），`--status` 与正常检查都给 `3`：调度器/CI 能编程发现，
  绝不显示成「一切正常」；`--status` 一行 `内核: DUPLICATE（N 个内核进程在跑，预期 1 个；pid …）`。
- 落 `$STATE_DIR/DUPLICATE_CORES` 标记文件；解除后自动删除，并记一行、发一条
  「重复内核已解除」（不会误报成「栈已恢复」——栈本来就没停）。
- **告警正文自包含**：风险（#199）、**全部 pid**、每个 pid 各自的 argv 模式与 socket、
  可直接粘贴的止损命令 `cd "<repo>" && blitzkrieg stop`、未平仓与未赎回应收、最后已知存活。
- 去抖与停机告警共用同一套状态机（翻转时出声，持续期间每 `--repeat-sec` 重复一次）。
- **重复期间绝不自动拉起**：多出来的那个不是「没起来」，再拉只会更多。同理，pidfile 判死
  但 `pgrep` 复核发现内核在跑时也不拉起（会变成重复内核），改为提示核对 pidfile。
- 复核口径：先 `pgrep -f` 发现候选，再逐个用 `kill -0` + `ps -ww` 命令行匹配复核，只数
  复核通过的；`pgrep -f | head -1` 取哪个 pid 是**不确定的**（内核之间没有主次之分），
  所以单一 pid 只用于显示与推导 socket，计数与告警都以复核后的完整列表为准。
- **复核是子串匹配，所以它会喊多**（实测）：默认串 `target/release/blitzkrieg-core` 命中了
  4 个 `zsh -c … cargo build …` / `ls -la target/release/blitzkrieg-core` 的构建 shell，
  `--status` 于是报「4 个内核进程」。判定**故意不收紧**（收紧成「必须带 `--socket`」会让
  手工起的不带该参数的真内核被漏掉——漏报才是 #211 的原病），改为两条：告警里逐个 pid
  点名，**argv 里既没有 `--mode` 也没有 `--socket` 的标「可疑」**并提示逐条核对（真内核
  的 argv 一定同时带这两个参数，`supervisor.rs` 的 `to_args()` 每次都显式传）；要更严的
  部署方把 `BK_CORE_PGREP` 改成 `target/release/blitzkrieg-core.*--socket` 即可（自测第
  22 组把这两条都钉住：松串仍报重复但点名可疑，收紧串则一个都不算）。

**外置卷的 TCC 会让第 3 步跑不起来**（错误会出现在
`~/Library/Logs/blitzkrieg-stack-watchdog.log`）。三选一，细节见 `scripts/README.md`：
(a) 给解释器（`/bin/sh`）授予完全磁盘访问权限——安全姿态变更，**你拍板**；
(b) 把脚本复制到内置盘，用 `BK_REPO_ROOT` 指回本仓库——停机判定与告警仍然成立（`pgrep` 与
`$TMPDIR` 下的 socket 都在受保护卷之外），只有模式 / 未平仓会显示「无法判定（权限被拒）」；
(c) 把检出搬到内置盘。

**DRY_RUN 与自动拉起的关系**（一句话：**默认不拉起，live 永不自动拉起**）

| 条件 | watchdog 会不会拉起内核 |
| --- | --- |
| 默认（没有 `--autostart`，也没有 `BK_AUTOSTART_CMD`） | 不会，只告警 |
| `--autostart`（或 `BK_AUTOSTART=1`）但**没有** `BK_AUTOSTART_CMD` | 不会（两把钥匙缺一不动） |
| 两把钥匙都有 + 模式 dry | 会：执行 `BK_AUTOSTART_CMD`，输出记入 `autostart.log` |
| 两把钥匙都有 + 模式 live | **不会**：代码里的硬门禁，开关打开也一样 |
| 两把钥匙都有 + `--readonly` 或模式无法判定 | 不会（只允许 dry） |
| 进程在但 socket 不通 | 不会（再拉一个会有两个内核抢同一个 socket，交人工） |
| 发现 ≥2 个内核进程（重复内核，退出码 `3`） | **不会**：多出来的那个不是「没起来」，再拉只会更多；先 `blitzkrieg stop` |
| pidfile 判死、但 `pgrep` 复核发现内核在跑 | 不会（拉起会变成重复内核；先核对 pidfile） |

- watchdog **只读** `.env`（只取 `DRY_RUN` 一行用于显示），**永不写 `.env`、永不改 `DRY_RUN` 的值**。
- 计划内停机（自己 `blitzkrieg stop`）先 `touch ~/Library/Logs/blitzkrieg-stack-watchdog/silence`，
  起来后删掉：静默只压「喊人」（不通知、不落告警正文），状态、`STACK_DOWN` 标记与退出码照常。
- 去抖：只在状态翻转时出声；持续停机期间最多每 15 分钟（`--repeat-sec`）重复一次，不刷屏。
- 自测（CI 可直接跑：fixture 驱动，**不需要真内核**，不碰生产 `data/`；当前
  `23 组用例 / 129 项断言`）：`bash scripts/stack-watchdog.sh --self-test`
  覆盖用法错误、活着/死了的 pidfile、socket 不通、去抖与静默、自动拉起的两把钥匙与
  live 硬门禁、`--status` 只读、**无 `--pidfile` 的 pgrep 发现分支**（命中 / 无命中 /
  两个命中 / 复核不认 / pidfile 过期）、**「只在命令行里提到内核路径」的构建 shell
  不该把重复信号喊成狼来了**、僵尸进程与长命令行两个实测回归，以及**备份新鲜度**
  （第 23 组：新鲜时安静、变陈旧时告警 + `BACKUP_STALE` 标记 + 退出码 `4`、去抖、
  内核停机时抑制通知但保留标记、恢复后清除、判定器缺失时报 `unusable` 而不是当作通过）。

### 3.6 自动备份：TCC 拦住了什么、现在怎么跑（issue #217）

**现象（实测，2026-09-21 发现）**：`com.blitzkrieg.databackup.light/full` 两个 LaunchAgent
从装上起**一次都没成功过**，却看不出任何异常——日志只有 94 字节
一行 `/bin/sh: /Volumes/Hard Disk/BlitzkriegBot/scripts/data-backup-cli.sh: Operation not permitted`，
而 `--status` 一查：**零个自动备份**。缺陷不是「TCC 拒绝」（那是部署选择），而是**拒绝是静默的**：
issue 当时读到的 `launchctl list` 退出码是 `0`，2026-09-23 复查同一行是 `126`——**两种读数都没人消费**，
没有状态文件、没有告警、`--status` 那一行也不存在。

**根因**：本仓库在外置卷，macOS TCC 拒绝 launchd 拉起的进程读**和**执行该卷上的任何东西
（探针 agent 实测：`ls` 仓库、`head` 仓库内文件、exec 脚本，全部 EPERM / exit 126）。
所以把 launcher 挪到内置盘是**必要但不充分**的：脚本本身还在被拒的卷上。
**「plist 存在」「`launchctl list` 有条目」都不等于备份在跑**——唯一算数的是
`--status` 的一行 `ok`（或产物的 `MANIFEST.sha256` 校验通过）。

issue #217 列了三条路线，按「当前外置卷布局下是否真的生效」排序：

| 路线 | 说明 | 状态 |
| --- | --- | --- |
| **1. 内置盘 launcher + 完全磁盘访问（D-33）** | launcher 放内置盘，/bin/sh 授 FDA。**唯一在当前布局下真正生效的**，跨重启 | 已就绪（模板 + 安装器），**授权是用户手工步骤，尚未做** |
| **2. 把检出搬到内置盘** | 最彻底：同时解掉 #211 看门狗的同类问题 | 未做（部署决定，属用户） |
| **3. 放弃 launchd，由内核自己触发备份** | 内核是你手工启动的、已有 TCC 授权；失败走 `emit_error` 可见 | 未做（需要动内核代码，本次范围外） |

本仓库另外提供了一条**今天就能用、不需要任何权限变更**的替代路线（B 路线，下面），
它不在 issue 的三条里，但同样是「有产物 + 有可见信号」的正规做法。

**A 路线（= issue 路线 1）：LaunchAgent + 完全磁盘访问**（重启/登出后仍然有效）

```bash
bash scripts/data-backup-install.sh              # 安装 + 自检探针（仍被拒时非零退出）
bash scripts/data-backup-install.sh --no-verify  # 跳过探针
bash scripts/data-backup-install.sh --uninstall  # 卸载 agent 与内置盘 launcher
```

安装器会把一个**内置盘 launcher** 写到
`~/Library/Application Support/blitzkrieg/data-backup-launch.sh`，plist 的
`ProgramArguments` 指向它（`/bin/sh <launcher> <light|full>`）——因为一个连自己脚本都读不到的
`/bin/sh` 没有能力报告任何事。launcher 会**真的读一次**仓库，被拒时做三件事：写一条
`result=fail` 的尝试记录（内置盘状态文件，`--status` 与看门狗都读它）、打出一行带日期的
`FAIL: … (Operation not permitted)`、以 **126** 退出（不再静默）。可读时 `exec` 真正的
`data-backup-cli.sh … --attempt-source launchd`。日志仍在
`~/Library/Logs/blitzkrieg-data-backup-<tier>.log`。

launcher 与两个 plist 都是**仓库里的模板**（`scripts/templates/`，安装器渲染后落盘），
因此「将要跑的东西」在 code review 里可读、可 diff；手工安装与占位符说明见
[`scripts/templates/README.md`](scripts/templates/README.md)。

**剩下的一步只能你做，我无法代做**（安全姿态变更，D-33）：
1. 系统设置 → 隐私与安全性 → **完全磁盘访问权限**；
2. 点 **+**，按 ⌘⇧G 输入 `/bin/sh`（launchd 实际启动的解释器，即 `ProgramArguments[0]`），
   加进去并打开开关；
3. 重新跑 `bash scripts/data-backup-install.sh`（会重新探针），或等下一个 04:00 后跑
   `bash scripts/data-backup.sh --status`；
4. 只有探针 `PASS` / 状态行为 `ok` 才算生效——「已安装」不是证据。

**B 路线：常驻循环**（今天就能用，**不需要任何权限变更**，但**不跨重启/登出**）

```bash
scripts/data-backup-loop.sh start          # 每天 04:00 light，周日 04:30 full
scripts/data-backup-loop.sh start --once   # 立刻跑一次 light 后退出
scripts/data-backup-loop.sh status|stop
```

`nohup` 从你自己的会话里拉起的进程**继承该会话的 TCC 权限**，与
`scripts/soak-resident.sh` 同一套语义；它写与 A 路线相同的日志和尝试记录，所以 `--status`
读哪条路线都一样。**重启后要重新 `start`**——这正是下面这个检查存在的理由。

**判定「备份到底有没有在发生」**（两条路线共用）

```bash
bash scripts/data-backup.sh --status    # 一行；没有新鲜备份就退出 1
blitzkrieg backup --status              # 同一件事，走 shim
```

`--status` 分开判两件事：磁盘上最新的**产物**（按 tier 的年龄，并把
`ABSENT`（卷不在）/ `NOTDIR` / `DENIED`（读不到）/ `NONE`（从未产出）分成四种原因），
以及最新一次**自动**尝试（launchd/loop 日志最后一行按失败签名归类 + 每次运行都会写的
`scripts/lib/backup-attempt.sh` 记录，后者还带**产物路径与体量**：只有时间戳的话，
一份 0 字节的「成功」和一个真正的备份在记录里长得一样）。`source=cli` 的记录**故意不算调度证据**：
手工跑成功一次绝不能让调度看起来健康——只判产物的检查会重现事故里那种「假的安心」。

这个检查接进了**两条**告警通道，不再依赖「有人正好在跑健康巡检」：

- `scripts/soak-health.sh` 的既有告警通道（`backup=` 子状态）——终端里人工巡检时看它；
- **`scripts/stack-watchdog.sh`（#211 的看门狗；装了 `scripts/com.blitzkrieg.stack-watchdog.plist`
  就每 60 秒跑一次，本机目前**未装**——它同样受 TCC 限制，安装前请先读它自己的说明）**：距上次成功备份
  超过 N 小时（`light` 默认 26h、`full` 默认 192h，用 `BK_BACKUP_STALE_HOURS` /
  `BK_BACKUP_FULL_STALE_HOURS` 调，`--no-backup-check` 整条关掉）
  就告警，并落一个 `BACKUP_STALE` 标记、以**退出码 `4`** 结束；恢复后清除标记并记一行
  「备份已恢复新鲜」。两个退出码的优先级是刻意的：重复内核（`3`）> 内核停机（`1`）> 备份陈旧（`4`）；
  内核停机期间**不喊人**（通知被抑制）但标记与退出码照写——「后端全停了」和「后端在跑但备份没跑」
  是两件事，看门狗不把它们混成一条消息。判定逻辑不在这里重新实现：看门狗调用
  `data-backup.sh --status --quiet` 并读它那一行，两个实现会分歧，一个实现不会。

---

## 4. 工作原理

```
┌──────────────────────────────┐        spawn + 生命周期托管
│  ui_kit_web（Rust 面板）      │ ───────────────────────────────┐
│  Vue 前端 · snapshot/命令 API │                                  │
│  127.0.0.1:51888             │ ◀── UDS（$TMPDIR/*.sock）        │
└──────────────────────────────┘     JSON-RPC 2.0，'\n' 分帧       │
                                            ▲                       ▼
┌───────────────────────────────────────────┴───────────────────────────┐
│                         blitzkrieg-core（Rust）                          │
│  engine/ome · order · position · risk · exit_policy · reconcile        │
│  strategy_engine（内置 + cdylib 热加载） · shadow/shadow_evolution      │
│  market registry（core 市场无关，无任何交易所 SDK）                      │
└───────────────────────────────┬─────────────────────────────────────────┘
                                 │ 依 feature 挂接
                    ┌────────────┴────────────┐
                    │ extensions/polymarket   │  CLOB 下单 · WS 行情 · Gamma 轮盘
                    └─────────────────────────┘
```

- **面板 → 核心**：`{jsonrpc:"2.0",id,method,params}`
- **核心 → 面板（响应）**：`{jsonrpc:"2.0",id,result|error}`
- **核心 → 面板（事件）**：`{jsonrpc:"2.0",method:"core.event",params:{kind,...}}`

图中「spawn + 生命周期托管」既可以是 `blitzkrieg run` 的一体化托管（launcher 为父进程，
崩溃自动重建，见 §3.1），也可以是 `ui_kit_web --manage` / `blitzkrieg web --manage` 的
分体式托管；两者共用同一套 `Supervisor`，且都遵守「只收自己拉起的内核」的克制语义。

市场抽象契约见 `core/market_api`：`DataFeed`、`MarketDiscovery`、`OrderExecutor`、
`MarketPlugin`、`MarketHost` 等 trait；新增交易所 = 写一个扩展 crate 并用 feature 注册，核心不改一行。

验收门禁的驱动端是 `scripts/lib/core-client.mjs`（零依赖 bare-Node IPC 客户端），
**不参与生产运行**：它只被本地验收门禁用来以编程方式驱动一个临时核心做端到端校验。

---

## 5. 开发与门禁

提交前**必须**在仓库根跑完全部门禁：

```bash
# Rust
cargo build --release --workspace --locked
cargo test --workspace --locked            # 核心 + 各 crate 单测

# DryRun 订单链端到端：挂单 → 成交 → 持仓 → 重估（自起临时 core，隔离 socket）
node scripts/cycle-check.mjs

# 核心行为 / 账目一致性对拍（自起临时 core，dry+live 双核对拍）
node scripts/core-parity.mjs
node scripts/account-parity.mjs

# 零依赖密钥扫描（工作树；--history 扫全历史）
bash scripts/secret-scan.sh
```

面板前端（`ui/webapp/webui/`）另有一套 bare-Node 检查：

```bash
cd ui/webapp/webui && npm run check:all
```

其他常用门禁脚本（`node scripts/<name>.mjs` 直跑，无 npm 别名）：

- `scripts/core-adopt-check.mjs` —— 多客户端竞争与 adopt 语义。
- `scripts/dry-observe.mjs` —— DryRun 观察。
- `scripts/shutdown-cleanliness-check.mjs` / `parent-monitor-check.mjs` / `readonly-egress-check.mjs` / `crash-recovery-check.mjs` / `gateway-signal-stop-check.mjs` —— 生命周期、只读出口、崩溃恢复与网关信号收尾验收。
- `scripts/unified-launcher-check.mjs` —— 单二进制 `blitzkrieg` 一体化启动验收（子命令 / 托管 / 优雅退出 / `--readonly` 穿透）。
- `scripts/strategy-gate-check.mjs` —— 策略门禁豁免（声明兑现 + D-31 剩余时间下限双向断言）。
- `scripts/soak-health.sh` —— 长跑健康巡检（面板/核心/采样/账本/备份新鲜度一行判定；常驻配对见 `scripts/README.md`）。
- `scripts/data-backup.sh --status` —— 「备份到底有没有在发生」一行判定（产物年龄 + 最近一次自动尝试；没有新鲜备份就退出 1）。
  调度与 TCC 见 §3.6；三条路线（LaunchAgent+FDA / 常驻循环 / 手工）都写同一份日志与尝试记录，所以判定不受路线影响。

> **禁止**在未通过上述验证时提交到 `main`；完整门禁矩阵见 `dev-docs/DEVELOPMENT.md`（内部）。

---

## 6. 分支模型与协作

- 长期分支：`main`（发布分支，按约定仅经 PR 合入）、`develop`（集成分支）。
- 短期分支：`feat/*`、`fix/*`、`chore/*`、`release/*`。
- 所有合入走 Pull Request + 全绿检查。
- **远端名为 `ceer`**（`origin` 已移除）。

### 不可逾越的安全红线

- 禁止在未授权下启用 **Live** 交易。
- 禁止修改真实凭证、私钥、API Key；机密一律走环境变量 / secret，绝不入库。
- 禁止删除未经备份的文件；禁止在未验证时提交主分支；禁止「顺手」改动业务逻辑。
- 发现漏洞请走 [`SECURITY.md`](./SECURITY.md) 的私下披露流程，勿在公开 Issue 粘贴机密。

---

## 7. 文档

| 文档 | 内容 |
| --- | --- |
| [docs/FEATURES.md](./docs/FEATURES.md) | **功能清单与完成度**（证据分级：实盘验证 / 离线验证 / 仅测试覆盖 / 未验证 / 规划） |
| [docs/rust-core/ARCHITECTURE.md](./docs/rust-core/ARCHITECTURE.md) | Rust 分层架构与扩展体系 |
| [docs/rust-core/STRATEGY_GUIDE.md](./docs/rust-core/STRATEGY_GUIDE.md) | 如何编写与加载策略 |
| [docs/rust-core/EXTENSION_GUIDE.md](./docs/rust-core/EXTENSION_GUIDE.md) | 如何新增一个市场扩展 |
| [docs/rust-core/ABI_V2_DESIGN.md](./docs/rust-core/ABI_V2_DESIGN.md) | 策略 C ABI v2 设计（vtable 已冻结，新能力走可选符号） |
| [docs/rust-core/INTERFACES.md](./docs/rust-core/INTERFACES.md) | UDS JSON-RPC 2.0 方法/事件契约与版本变更记录 |
| [docs/rust-core/SHADOW_EVOLUTION.md](./docs/rust-core/SHADOW_EVOLUTION.md) | 影子进化（按策略参数 / 孪生 / 审计 / apply·rollback） |
| [CHANGELOG.md](./CHANGELOG.md) | 变更史 |

内部开发文档（门禁矩阵、决策记录、迁移日志、GitHub 治理规范等）在 `dev-docs/`，
**不入库、不上 GitHub**；在本地检出中直接阅读。

---

## 8. 配置

- 核心配置会被**实际读取**（KI-11 / D-1）：`user_layer/configs/default.toml`（含同目录的
  `shadow_evolution.toml`）在启动时解析，优先级 **CLI 参数 > `BK_*` 环境变量 > 配置文件 >
  代码默认值**；每个非默认值打印 `key=value (source)`，未识别的键逐个告警。细节见 §3.3
  的「配置文件」小节。
- 面板与第三方凭证通过环境变量提供，参考 [`.env.example`](./.env.example)；
  真实 `.env` 已被忽略，**切勿**提交私钥 / API Key。
- 数据目录、日志（trade/order/position）与 SQLite 库的落盘位置见 `blitzkrieg-core --help`。

---

## 9. 策略状态与进入条件

内核**出厂零策略、零默认启用**：策略来自策略目录下的 cdylib，全部以「关闭」启动；真正决定
启用与否的只有操作员的持久意图 `data/strategy-state.json` 与显式旗标
`--enable-strategy <name>` / `--disable-strategy <name>`（显式关闭优先）。这条规则的原文在
`user_layer/configs/default.toml` 顶部（`[strategy]` 段已于 PR-B 移除），本节是**唯一的
「某策略何时才允许进入默认启用清单」的记录处**——`dev-docs/` 不入库，不能作为引用来源。

### pair_arb — 实验性，禁止上线（`special/no-live`）

`pair_arb`（完整集配对套利）**不在任何默认启用清单里**，并且**禁止实盘启用**，直到它满足
下面的进入条件：

> **冻结语料 holdout：PF ≥ 1.5 且净利润 > 0，并在两个互不重叠的窗口上同时成立。**

当前实测结论是**不满足**：最佳配置的 PF 仅 **0.659**（胜率可达 85%，但逆向选择吃掉全部价差），
详见 `user_layer/strategies/pair_arb/pair_arb_strategy.rs` 文件头的四行配置表与「入场规则」
一节（`1 − (up_bid + down_bid) ≥ min_edge`）。该策略作为**研究工具**保留：会注册、可被
`--enable-strategy pair_arb` 显式打开，但永不自动启用。

两条配套约束：

- **#262 的 WR/PF 口径（caliber）不包含 pair_arb。** 度量进化目标时把 pair_arb 排除在外，
  理由与进入条件相同：它没有可比的 PF 基线，纳入只会污染口径。
- **缺口（只记录，不补）：** 仓库里**没有**「默认启用清单」的自动核对门禁——即没有脚本会在
  某个策略被写进默认启用列表时报警。当前靠代码评审 + 本节的人工核对，`data/strategy-state.json`
  仍是唯一权威。若将来新增此类门禁，本节的条件应成为它的断言来源。

---

## 10. 许可

[MIT](./LICENSE)。版权（c）2026 BlitzkriegBot contributors (ceer-quant)。
