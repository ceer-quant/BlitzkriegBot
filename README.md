# BlitzkriegBot

> 面向轮盘型预测市场（当前为 Polymarket 加密二元市场）的**纯 Rust 自动化交易系统**。

<div align="center">

[![CI](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/ci.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/ci.yml)
[![Security](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/security.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/security.yml)
[![Secret Scan (deep)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/secret-scan.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/secret-scan.yml)
[![Release](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/release.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024%20edition-orange)](https://doc.rust-lang.org/edition-guide/)

**⚠️ 免责声明：预测市场交易有真实资金风险。本项目是 MIT 开源软件，仅供学习与研究，不构成投资建议，作者不对任何损失负责。**

</div>

---

## 目录

- [30 秒读懂它](#30-秒读懂它)
- [5 分钟上手](#5-分钟上手)
- [安全默认与风险](#安全默认与风险)
- [功能一览](#功能一览)
- [常见问题 FAQ](#常见问题-faq)
- [术语表](#术语表)
- [故障排查](#故障排查)
- [开发者手册](#开发者手册)
- [文档导航](#文档导航)
- [许可证与贡献](#许可证与贡献)

---

## 30 秒读懂它

BlitzkriegBot 是一个**自己盯行情、自己下模拟单**的机器人：它盯着 Polymarket 上「UP/DOWN」这类
15 分钟一局的加密二元市场，按可插拔的策略自动下单。它**默认全部用模拟资金**（dry 模式）——
你可以把它当成一个零风险的行情实验台。

这套系统有四层，各管一件事：

```
┌────────────────────────────────────────────────────────────┐
│ 用户层：你的策略（Lua 策略包，默认安装即关闭）              │
│   user_layer/strategies_lua/ 里放一个带 manifest.json 的   │
│   目录 = 安装一个策略；启用与否由你在面板里决定             │
├────────────────────────────────────────────────────────────┤
│ 交易核心 blitzkrieg-core（市场无关）：                     │
│   撮合 · 下单 · 持仓 · 风控 · 对账 · 账本，内核 0 策略     │
├────────────────────────────────────────────────────────────┤
│ 市场扩展：extensions/polymarket（唯一官方扩展）            │
│   CLOB 下单 · WS 行情 · Gamma 轮盘发现                     │
├────────────────────────────────────────────────────────────┤
│ 面板 blitzkrieg（单二进制启动器）：                        │
│   Web 面板（127.0.0.1:51888）· TUI · run/stop 生命周期     │
└────────────────────────────────────────────────────────────┘
```

- **交易核心与面板**之间用本机 Unix 域套接字 + 换行分帧的 JSON-RPC 2.0 通信。
- **核心与任何交易所之间**只隔着一层扩展契约（`core/market_api`），换交易所 = 写一个扩展
  crate，核心不改一行。
- Node.js（`>= 22`）**只用于本地验收门禁脚本**（零 npm 依赖），不参与生产运行。

## 5 分钟上手

前提：一台能装 [Rust](https://rustup.rs) 的电脑（macOS / Linux；CI 只测 `ubuntu-latest`，
release 产物只发 Linux x86_64 与 macOS aarch64）。**不需要** Node.js——除非你要跑开发门禁。

### 第 1 步：拿到代码并构建（约几分钟，看网速）

```bash
git clone https://github.com/ceer-quant/BlitzkriegBot.git
cd BlitzkriegBot
cargo build --release --workspace --locked
```

你会看到：Cargo 下载并编译几百个 crate，最后几行是 `Finished ... in ...`。
构建产物在 `target/release/` 下，关键是 `blitzkrieg-core`（交易核心）和 `blitzkrieg`（面板/启动器）。

### 第 2 步：安装 `blitzkrieg` 命令（一次性，可选但推荐）

```bash
bash scripts/install-blitzkrieg-shim.sh
```

你会看到：一个 `blitzkrieg` 命令被装进 PATH（默认 `~/.local/bin`），以后**从任何目录**都能用。
它只是个指路牌，永远指向本仓库最新构建；仓库挪了位置就重新跑一次这个脚本。

### 第 3 步：一键启动（默认就是 dry 模拟模式）

```bash
blitzkrieg run
```

你会看到：终端打印启动信息（含当前的运行模式），随后面板在本机 `127.0.0.1:51888` 起来。

**没有 `blitzkrieg` 命令也行**，分两条命令手工起同样的东西：

```bash
./target/release/blitzkrieg-core --mode dry --engine --feed-ws \
  --assets BTC,ETH,SOL,XRP --round-sec 900 --min-round-age 30 --min-time-left 180 \
  --max-positions 2 --max-order-notional 6 --seed-balance 1000 --tick-ms 50
./target/release/ui_kit_web --addr 127.0.0.1:51888 --manage
```

### 第 4 步：打开面板看它干活

浏览器打开 **`http://127.0.0.1:51888/panel`**。

你会看到：一个 Vue 面板。默认安装（未设置面板账号密码）时是**纯查看**：轮次列表、行情快照、
挂单/持仓/账本都在实时刷新，但没有下单按钮权限。想要「用网页下命令」（启动/停止/开关策略），
复制 `.env.example` 为 `.env`，把 `BLITZKRIEG_PANEL_USER` 和 `BLITZKRIEG_PANEL_PASSWORD`
**两个都设上**，重启 `blitzkrieg run`，登录即可。

### 第 5 步（可选）：需要命令行操作面板？

| 想做的事 | 命令 |
| --- | --- |
| 停掉整套（面板 + 核心，重复执行无害） | `blitzkrieg stop` |
| 只开终端面板（TUI），不开网页 | `blitzkrieg run --tui` |
| 后台常驻 | `nohup blitzkrieg run >> /tmp/blitzkrieg-run.log 2>&1 &` |
| 只起 Web 网关，不自动拉核心 | `blitzkrieg web` |
| 结构性只读（连下单通道都不构造） | `blitzkrieg run --readonly` |

默认参数与旋钮的完整清单见[开发者手册](#开发者手册)；`.env` 环境变量全家福见
[`.env.example`](./.env.example)。

## 安全默认与风险

**先说风险：这是交易机器人。即使全部模拟，习惯上的每个决定在真实资金下都有代价。**

- **dry 模拟是出厂默认**：`--mode` 的默认值是 `dry`（本地模拟成交）；`.env` 里
  `DRY_RUN=true` 是默认且是最安全的一行。**唯一能碰到真钱的开关是把它改成 `false`**
  （或显式 `--mode live`）——项目的硬性规则是 **Live 保持关闭，除非有真人明确授权**。
- **`--readonly` 是结构性的**：给定它，进程从根上不构造下单通道——这不是逐入口拦截，
  是根本没有那条路。它压过一切 live 请求（`--mode live` 搭配 `--readonly` 不报错，
  但只会得到只读）。
- **策略默认全部关闭**：内核出厂 **0 个启用策略**。Lua 策略包放进目录只是「安装」，
  每个都以关闭状态启动；启用与否由持久化状态文件 `data/strategy-state.json` 和显式旗标
  `--enable-strategy` / `--disable-strategy`（显式关闭优先）决定。策略通过 C ABI v2
  在运行时加载，**分发时不捆绑任何策略**。
- **面板只听本机**：默认 `127.0.0.1:51888`，只有这台电脑能访问。要局域网访问需主动把
  `BLITZKRIEG_PANEL_ADDR` 改成 `0.0.0.0:51888`，且**登录凭据在任何地址上都是必需的**，
  跨站来源一律 403。请勿将面板暴露到不受信任的公网。
- **密钥只走环境变量**：`.env` 已被 git 忽略；`POLYMARKET_PRIVATE_KEY` 等凭据绝不入库。
  发现安全问题请走 [`SECURITY.md`](./SECURITY.md) 的私下披露流程。
- **代码红线**（对贡献者）：禁止未授权开启 Live；禁止提交真实凭证；禁止未经备份删除文件、
  未验证合入主分支。

## 功能一览

> 每项功能的完成度与证据分级（实盘验证（dry）/ 离线验证 / 仅测试覆盖 / 未验证 / 规划）
> 以 [`docs/FEATURES.md`](./docs/FEATURES.md) 为权威——**整条 Live（真实下单）链路属于
> 未验证，全程 DRY**。

| 功能 | 说明 |
| --- | --- |
| 轮盘发现（Gamma） | 自动发现 Polymarket 的 15 分钟 UP/DOWN 轮盘市场 |
| 行情 | Rust 原生源：Polymarket REST `POST /books` 轮询 + Binance 现货 WS |
| 自驱动引擎 | 发现 → 扫描 → 信号 → 下单 → 出场，一条流水线自动跑 |
| DryRun 模拟成交 | 本地撮合、估值、赎回，零资金风险 |
| 下单/持仓/对账 | 挂单生命周期、持仓重估、与交易所成交回报对账 |
| 风控硬限制 | 持仓数、单笔名义额等硬上限，策略不可豁免 |
| 出场策略 | 止损、最小剩余时间等自动出场 |
| 策略系统 | Lua 策略包（官方 spread_arb 等 9 个）+ cdylib（C ABI v2，vtable 已冻结）热加载 |
| 影子进化 | 影子回放、近失样本、按策略参数进化（apply · rollback，生产默认关闭） |
| 回放与回测 | `--replay` 行情回放、近失样本回放、面板内回测 |
| Web 面板 | Vue 前端：快照 / 命令 / 插件 / 登录 API，可托管核心生命周期 |
| TUI 终端面板 | `blitzkrieg tui` / `run --tui` |
| 一键启停 | `blitzkrieg run` / `stop`，崩溃自动重建，只收自己拉起的进程 |
| 看门狗与备份 | `scripts/stack-watchdog.sh`（停机告警，不自动拉起）、数据备份与新鲜度巡检 |
| 多市场架构 | 核心市场无关，新增交易所 = 写一个扩展 crate + feature 注册 |

## 常见问题 FAQ

**Q1：面板打不开（127.0.0.1:51888 没反应）？**
按顺序查：① 核心与面板起了吗——`blitzkrieg run` 是否还在跑、终端有没有报错；② 是不是起了
两套栈——旧栈还占着 51888 / 默认 socket 时再 `run`，端口绑不上，先 `blitzkrieg stop` 再起；
③ 浏览器地址是不是 `http://127.0.0.1:51888/panel`（本机访问别用 `0.0.0.0` 当网址）。

**Q2：端口被占用怎么办？**
`blitzkrieg stop`（重复执行无害）停掉旧栈；或者换个端口：`.env` 里设
`BLITZKRIEG_PANEL_ADDR=127.0.0.1:51889`。

**Q3：我的策略为什么没生效？**
三道门逐一检查：① 包在 `user_layer/strategies_lua/`（cdylib 在 `user_layer/strategies/`）
里吗——放进去只是**安装**；② 启用了吗——看 `data/strategy-state.json`，或启动时
`--enable-strategy <name>`（内核出厂 0 启用策略）；③ 请求了策略但一个都没加载成功时内核
**拒绝启动**（exit 1），除非 `--allow-zero-strategies`。启动日志会打印每个策略的装载与
启用状态。

**Q4：怎么退出 / 停止？**
`blitzkrieg stop`——面板与核心一起收，重复执行无害；它只碰挂在目标 socket 上的
blitzkrieg 家族进程，你启动它的 shell 永远不会被碰。

**Q5：数据都存在哪里？**
仓库根的 `data/` 下：成交台账 `data/trades/trades.jsonl`、订单 `data/orders/orders.jsonl`、
持仓快照 `data/positions/positions.jsonl`、策略启用状态 `data/strategy-state.json`；
市场数据事件归档默认也在 `data/`（`--event-archive-max-mb` 控制体积，`0` 不限）。
完整清单跑 `./target/release/blitzkrieg-core --help` 看。

**Q6：怎么升级到新版本？**
拉最新代码重新构建即可：`git pull && cargo build --release --workspace --locked`。
正式版本以根 `Cargo.toml` 的 `version`（当前 **0.3.0**）为准，tag 形如 `v0.3.0`，由
Release 工作流构建发布；发布与版本规则见 [`docs/VERSIONING.md`](./docs/VERSIONING.md)。

**Q7：支持 Windows 吗？**
不支持。CI 只在 `ubuntu-latest` 上测试，Release 只发布 Linux x86_64 与 macOS aarch64
两个平台的产物（x86_64 macOS 已在 0.3.0 放弃）。Windows 没有构建目标也没有测试覆盖。
另注意：本项目依赖 Unix 域套接字，即便能在 Windows 上编译，核心与面板的通信机制也不可用。

**Q8：Node.js 是干什么的？我要装吗？**
只是**本地验收门禁脚本**的运行环境（CI 里钉的是 Node 22，全部零 npm 依赖），不参与生产
运行——生产是 100% Rust。只跑机器人不用装；要跑 `node scripts/*.mjs` 门禁或前端检查
才需要。

**Q9：可以拿真钱跑吗？**
技术上存在 live 模式，但**整条 Live 链路未经验证**（[`docs/FEATURES.md`](./docs/FEATURES.md)
的证据分级里它属于「未验证」），且项目规则要求真人明确授权才能开启。如果你仍决定探索，
先问自己三个问题：钱包里有你亏得起的钱吗？——本项目不构成投资建议，不对任何损失负责。

## 术语表

| 术语 | 一句话解释 |
| --- | --- |
| **dry / live** | dry = 本地模拟成交，不动真钱（出厂默认）；live = 订单发往真实交易所，碰真钱 |
| **readonly** | 结构性只读：进程根本不构造下单通道，比「不点下单按钮」硬一个数量级 |
| **maker / taker** | maker 把单挂在订单簿上等人来成交；taker 直接吃掉别人的挂单。挂单策略通常是 maker |
| **预测市场 / 轮盘市场** | 对「某事件会发生吗」下注的市场；本项目跑的是 Polymarket 上 15 分钟一局的 UP/DOWN 加密二元市场，像轮盘一样循环开新局 |
| **negRisk** | Polymarket 上互斥事件组的一种合约结构（一组问题的资金共享一个结算篮子），发现与赎回逻辑要处理它 |
| **condition** | 一个二元市场的结算标识（condition ID），Polymarket 用它定位一个「问题」的链上合约 |
| **CLOB** | 中央限价订单簿（Central Limit Order Book），Polymarket 的撮合方式 |
| **Gamma** | Polymarket 的市场发现 API，轮盘市场列表从这里来 |
| **cdylib 策略** | 编译成动态库（`.dylib`/`.so`）的策略，内核运行时通过 C ABI v2 加载，vtable 已冻结 |
| **Lua 策略包** | 目录里带 `manifest.json` + `strategy.lua` 的策略（如官方 spread_arb），内核内嵌 Lua 运行时加载 |
| **策略状态（strategy-state）** | `data/strategy-state.json`：记录「哪些策略被启用了」的持久文件，重启后仍生效 |
| **影子进化（shadow evolution）** | 让策略变体在影子中并行试跑、按冻结语料证据优胜劣汰的机制（生产默认关闭） |
| **近失样本（near-miss）** | 「差一点就成交/触发」的行情记录，用于复盘策略错过了什么 |
| **dry 模拟成交** | 内核按真实盘口在本地模拟撮合与赎回，账本结构与 live 一致，但不动真钱 |
| **UDS** | Unix 域套接字，同一台电脑上两个进程间的高效通信管道（核心 ↔ 面板用这个） |

## 故障排查

症状 → 依次检查：

1. **`blitzkrieg run` 起不来，报参数错误** — 参数写错会直接退出（绝不静默吞掉按默认值起栈）：
   `--tui` 与 `--web` 不能同时给；`--manage` 不能搭配 `--attach`；未知 flag 会报错。看错误行
   修命令即可。
2. **面板显示「纯查看」，按钮都是灰的** — 面板凭据 `BLITZKRIEG_PANEL_USER` /
   `BLITZKRIEG_PANEL_PASSWORD` 没有同时设置。两个都写进 `.env` 并重启。
3. **策略装了但没在跑** — 见 FAQ Q3 的三道门（安装 → 启用 → 启动拒绝保护）。
4. **跑着跑着不动了 / 行情不动** — 检查盘口新鲜度保护：行情源断流超时（默认 8 秒）后引擎
   拒绝按过期盘口定价，自动停开新仓；看终端与面板日志确认行情源状态。检查网络与 Polymarket
   可达性（只读探针：`blitzkrieg net-check`）。
5. **重复内核（两个 blitzkrieg-core 进程）** — 两个内核会写同一份账本，必须立刻处理：
   `cd <仓库根> && blitzkrieg stop` 全收掉再起一套。`scripts/stack-watchdog.sh` 能自动发现
   这种状态（退出码 3）。
6. **机器重启后什么都没起来** — 这是**现状设计**：重启后没有任何东西自动拉起内核，需要手动
   `blitzkrieg run`。停机告警（不自动拉起）可装 `scripts/stack-watchdog.sh`；macOS 外置卷上
   的权限（TCC）限制与备份调度细节见 [`scripts/README.md`](./scripts/README.md)。

## 开发者手册

> 本节保留原有 README 的全部深度技术内容；更细的门禁矩阵与决策记录见 `dev-docs/`
> （内部文档，不入库，不上 GitHub，在本地检出中阅读）。

### 工作原理（架构图）

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
│  strategy_engine（cdylib + Lua 热加载，内核 0 策略） · shadow/shadow_evolution │
│  market registry（core 市场无关，无任何交易所 SDK）                      │
└────────────────────────────────────────┬───────────────────────────────┘
                                 │ 依 feature 挂接
                    ┌────────────┴────────────┐
                    │ extensions/polymarket   │  CLOB 下单 · WS 行情 · Gamma 轮盘
                    └─────────────────────────┘
```

- **面板 → 核心**：`{jsonrpc:"2.0",id,method,params}`
- **核心 → 面板（响应）**：`{jsonrpc:"2.0",id,result|error}`
- **核心 → 面板（事件）**：`{jsonrpc:"2.0",method:"core.event",params:{kind,...}}`

图中「spawn + 生命周期托管」既可以是 `blitzkrieg run` 的一体化托管（launcher 为父进程，
崩溃自动重建），也可以是 `ui_kit_web --manage` / `blitzkrieg web --manage` 的分体式托管；
两者共用同一套 `Supervisor`，且都遵守「只收自己拉起的内核」的克制语义。

IPC 契约唯一来源是 Rust 的 serde 结构体；门禁驱动端 `scripts/lib/core-client.mjs` 为零依赖
bare-Node 客户端，只做传输，不做二次校验，**不参与生产运行**（只被本地验收门禁用来以编程
方式驱动一个临时核心做端到端校验）。市场抽象契约见 `core/market_api`：`DataFeed`、
`MarketDiscovery`、`OrderExecutor`、`MarketPlugin`、`MarketHost` 等 trait。

### 目录结构（Cargo workspace）

```
BlitzkriegBot/
├── core/
│   ├── blitzkrieg_core/   # 交易核心二进制 blitzkrieg-core（市场无关）
│   └── market_api/        # 市场扩展契约：DataFeed / Discovery / Executor / MarketPlugin
├── extensions/
│   └── polymarket/        # 官方 Polymarket 扩展（默认 feature 挂接）
├── user_layer/
│   ├── strategy_api/      # 用户策略 trait / FFI 稳定表面
│   ├── strategy_logic/    # 策略算法（与内核共享的纯逻辑，不含任何启用策略）
│   ├── lua_runtime/       # 内嵌 Lua 运行时（Lua 策略包宿主）
│   ├── strategies_lua/    # Lua 策略包（官方 spread_arb 等 9 个；带 manifest.json）
│   ├── parity_strategy/   # C ABI v2 参考实现（独立的嵌套 workspace，产出 cdylib）
│   ├── strategies/        # cdylib 策略投放点：内核扫描这里，当前为空（只有 README）
│   └── configs/           # 内核会读取的运行时配置（default.toml 等）
├── ui/
│   ├── ui_kit/            # Rust UI 套件（bin: ui_kit_web —— 面板 HTTP 服务器）
│   ├── ui_kit_panel/      # 终端面板（bin: ui_kit_panel）+ 单二进制启动器（bin: blitzkrieg）
│   └── webapp/            # Vue 前端源码（webui/）与构建产物
├── scripts/               # 门禁与运维脚本（cycle-check / core-parity / secret-scan …；lib/ 为零依赖 IPC 驱动客户端）
├── docs/                  # 可公开文档（Rust 体系）
├── dev-docs/              # 内部开发文档（不入库，不上 GitHub；本地检出中阅读）
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
| `strategy_engine` | 动态策略加载器（dlopen C ABI v2 + Lua 包；内核不含策略） |
| `shadow` / `shadow_evolution` | 影子回放、近失（near-miss）样本与影子进化 |
| `sim` | DryRun 模拟成交与估值 |
| `ipc` | UDS + JSON-RPC 2.0 传输与 schema |
| `extension` / `market` | 扩展上下文与市场注册/托管 |

### 配置体系

- **核心配置会被实际读取**（KI-11 / D-1）：`user_layer/configs/default.toml`（含同目录的
  `shadow_evolution.toml`）在启动时解析，优先级 **命令行参数 > `BK_*` 环境变量 > 配置文件 >
  代码默认值**；启动时为每个非默认值打印一行 `key=value (source)`，说明该值从哪一层来。
- 关闭配置文件用 `--no-config` 或 `BK_CONFIG=none`；换一份用 `--config <path>` 或
  `BK_CONFIG=<path>`。文件缺失不是错误（默认值生效）；文件写坏只告警不致命；
  **文件里出现内核不认识的键会逐个告警**——不会静默忽略。
- **面板与第三方凭证通过环境变量提供**，参考 [`.env.example`](./.env.example)（分三族：
  `BK_*` 内核参数、`BLITZKRIEG_*` 面板设置、`POLYMARKET_*` 市场扩展凭据）；真实 `.env`
  已被忽略，**切勿**提交私钥 / API Key；建议 `chmod 600 .env`。
- `.env` 由启动器**自动加载**（无需 `source`）；已导出的环境变量永远优先于文件；文件值
  绝不打印。
- `run` / `core` 直接吃引擎旋钮：`--round-sec`、`--min-round-age`、`--min-time-left`、
  `--max-positions`、`--min-shares`、`--max-shares` 优先于 `HFT_*` 环境变量。
- 数据目录、日志（trade/order/position）与账本落盘位置见 `blitzkrieg-core --help`。

### 常用启动形态与旋钮（速查）

```bash
blitzkrieg [run|core|tui|web|net-check|version|update|stop|--help]
blitzkrieg run              # 一体化：内核 + Web 面板 + 崩溃自愈（默认 web）
blitzkrieg run --tui        # 只打开 TUI，不启动 Web（-tui 为容错短别名）
blitzkrieg run --web        # 显式选择 Web 面板
blitzkrieg tui --attach     # 只连接已有核心，不启动/停止核心
blitzkrieg web              # 只启动 Web 网关，不自动启动核心
blitzkrieg stop             # 停止同一 socket 上的整套栈（含孤儿内核）
```

核心直跑（DryRun）与常用开关：

```bash
./target/release/blitzkrieg-core \
  --mode dry \
  --engine --feed-ws \
  --assets BTC,ETH,SOL,XRP \
  --round-sec 900 --min-round-age 30 --min-time-left 180 \
  --max-positions 2 --max-order-notional 6 \
  --seed-balance 1000 --tick-ms 50
```

| 标志 | 含义 |
| --- | --- |
| `--mode <dry\|live>` | dry 模拟 / live 实盘（**默认 dry；live 需真人授权，见「安全默认与风险」**） |
| `--readonly` | 结构性只读：不构造出网桥梁，从根上禁止下单（压过 `--mode live` 与 `DRY_RUN=false`） |
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
| `--strategy-dir` / `--lua-strategy-dir` | cdylib / Lua 包扫描目录（`none` = 不加载） |
| `--enable-strategy` / `--disable-strategy` | 启动时开/关某策略（重复旗标；显式关闭优先） |
| `--socket <path>` | UDS 路径（默认 `$TMPDIR/blitzkrieg-core-$USER.sock`） |

### 服务器部署（局域网 / 远程访问）

`.env` 里加一行 `BLITZKRIEG_PANEL_ADDR=0.0.0.0:51888`（或启动时 `--addr 0.0.0.0:51888`）
即监听所有网卡；浏览器从 `http://<服务器IP>:51888` 打开面板即可全功能使用——**同源请求
天然放行**（Referer 的地址与请求 Host 一致），无需任何白名单；跨站来源一律 403。仅当面板
被反向代理改写域名时才需要 `--allowed-origin <url>`（可重复）或
`BLITZKRIEG_ALLOWED_ORIGINS`（逗号分隔）。**登录凭据在任何地址上都是必需的；请勿将面板
暴露到不受信任的公网。**

### 运维脚本（看门狗 / 停机告警 / 备份）

完整用法、TCC 权限与 LaunchAgent 细节见 [`scripts/README.md`](./scripts/README.md)，要点：

- `scripts/stack-watchdog.sh` — 内核心跳检查与**停机告警**（默认**只告警不拉起**；自动拉起
  需 `--autostart` + `BK_AUTOSTART_CMD` 两把钥匙齐备且模式为 dry，live 永不自动拉起）。
  能发现**重复内核**（独立事故形态，退出码 3）与备份陈旧（退出码 4）。
- `scripts/data-backup.sh --status` — 「备份到底有没有在发生」一行判定（产物年龄 + 最近一次
  自动尝试；没有新鲜备份就退出 1）。
- `scripts/data-backup-loop.sh start` — 常驻循环备份（每天 04:00 light、周日 04:30 full）；
  不跨重启。LaunchAgent + 完全磁盘访问路线见 `scripts/data-backup-install.sh` 与
  [`scripts/templates/README.md`](./scripts/templates/README.md)。
- `scripts/soak-health.sh` — 长跑健康巡检（面板/核心/采样/账本/备份新鲜度一行判定）。

### 开发与门禁

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
- `scripts/dry-observe.mjs` —— DryRun 观察（`--strategy <name>` 指定要加载并启用的 cdylib；内核自己不注册任何策略）。
- `scripts/shutdown-cleanliness-check.mjs` / `parent-monitor-check.mjs` / `readonly-egress-check.mjs` / `crash-recovery-check.mjs` / `gateway-signal-stop-check.mjs` —— 生命周期、只读出口、崩溃恢复与网关信号收尾验收。
- `scripts/unified-launcher-check.mjs` —— 单二进制 `blitzkrieg` 一体化启动验收（子命令 / 托管 / 优雅退出 / `--readonly` 穿透）。
- `scripts/strategy-devcheck.mjs` —— 策略开发链（模板 → 编译 → 加载 → 启用 → 出信号），自己造探针 crate。
- `scripts/soak-health.sh` —— 长跑健康巡检。
- `scripts/data-backup.sh --status` —— 备份判定（见上）。

> **禁止**在未通过上述验证时提交到 `main`；完整门禁矩阵见 `dev-docs/DEVELOPMENT.md`（内部）。

### 分支模型与协作

- 长期分支：`main`（发布分支，按约定仅经 PR 合入）、`develop`（集成分支）。
- 短期分支：`feat/*`、`fix/*`、`chore/*`、`release/*`。
- 所有合入走 Pull Request + 全绿检查。
- **远端名为 `ceer`**（`origin` 已移除）。
- **版本与 tag**：版本号的唯一事实来源是根 `Cargo.toml` 的 `[workspace.package].version`
  （`scripts/version-guard.mjs` 守卫）；tag 形态 `v<version>`，只落在 `main` 或 `release/*`
  线上，由 Release 工作流构建并发布（同一 tag 永不覆盖，一次发布 = 一批字节）。发布顺序、
  rc 路径与守卫细节见 [`docs/VERSIONING.md`](./docs/VERSIONING.md) §8.2。

### 策略状态与进入条件

内核**出厂零策略、零默认启用**：策略来自 cdylib 投放点（`user_layer/strategies/`）与 Lua
包目录（`user_layer/strategies_lua/`），全部以「关闭」启动；真正决定启用与否的只有操作员的
持久意图 `data/strategy-state.json` 与显式旗标 `--enable-strategy <name>` /
`--disable-strategy <name>`（显式关闭优先）。这条规则的原文在
`user_layer/configs/default.toml` 顶部，本节是**唯一的「某策略何时才允许进入默认启用清单」
的记录处**——`dev-docs/` 不入库，不能作为引用来源。

#### 当前状态：cdylib 策略目录为空；官方策略已转 Lua

内核曾经自带 5 个 cdylib 策略（`spread_arb` / `trend_follow` / `mean_reversion` / `pair_arb` /
`dog`）。它们已全部删除，**连同它们的默认启用清单条目**：`user_layer/strategies/` 现在只留
`README.md`（说明这个投放点的用途），目录里没有任何 `.dylib`。算法本身仍在
`user_layer/strategy_logic/`，参考实现是 `user_layer/parity_strategy/`；删除的动机、每个策略
的实测结论与随之退役的门禁见 [`CHANGELOG.md`](./CHANGELOG.md)。

官方 `spread_arb` 现在是 **Lua 策略**：`user_layer/strategies_lua/spread_arb/` 是 Rust 参考
实现的**逐位精确移植**（移植前在冻结语料的经济门禁上复现了全部 4 个窗口的基线记录）。
`user_layer/strategies_lua/` 现有 9 个策略包（`spread_arb` / `flash_arb` / `hot_side_momentum` /
`kline_probe` / `lua_momentum` / `mad_dog` / `market_maker` / `oracle_ruler` /
`pair_discount_arb`）——它们是**已安装、默认全部关闭**的，启用与否见上文。

因此本节当前**没有任何 cdylib 条目**。要新增策略时，先在这里写下它的进入条件，再让它进入
任何默认启用清单。

#### 进入条件（对任何未来策略的通用规则）

> **冻结语料 holdout：PF ≥ 1.5 且净利润 > 0，并在两个互不重叠的窗口上同时成立。**

这条规则的来源是 `pair_arb`：它曾被作为研究工具保留并禁止实盘启用，因为实测最佳配置的 PF
仅 **0.659**（胜率可达 85%，但逆向选择吃掉全部价差），且「PF 上限是退出策略产物」这个归因
后来被撤回——上限由算术与逆向选择独立支撑。同样的判据适用于任何新策略：**在把它写进任何
默认启用清单之前，先用冻结语料在两个不重叠的窗口上证明它满足上述条件。**

两条配套约束：

- **#262 的 WR/PF 口径（caliber）只统计有可比基线的策略。** 一个没有 holdout 基线的策略不
  进入度量口径，纳入只会污染它。
- **缺口（只记录，不补）：** 仓库里**没有**「默认启用清单」的自动核对门禁——即没有脚本会在
  某个策略被写进默认启用列表时报警。当前靠代码评审 + 本节的人工核对，`data/strategy-state.json`
  仍是唯一权威。若将来新增此类门禁，本节的条件应成为它的断言来源。

### 安全红线（不可逾越）

- 禁止在未授权下启用 **Live** 交易。
- 禁止修改真实凭证、私钥、API Key；机密一律走环境变量 / secret，绝不入库。
- 禁止删除未经备份的文件；禁止在未验证时提交主分支；禁止「顺手」改动业务逻辑。
- 发现漏洞请走 [`SECURITY.md`](./SECURITY.md) 的私下披露流程，勿在公开 Issue 粘贴机密。

## 文档导航

| 文档 | 内容 |
| --- | --- |
| [docs/FEATURES.md](./docs/FEATURES.md) | **功能清单与完成度**（证据分级：实盘验证 / 离线验证 / 仅测试覆盖 / 未验证 / 规划） |
| [docs/rust-core/ARCHITECTURE.md](./docs/rust-core/ARCHITECTURE.md) | Rust 分层架构与扩展体系 |
| [docs/rust-core/STRATEGY_GUIDE.md](./docs/rust-core/STRATEGY_GUIDE.md) | 如何编写与加载策略 |
| [docs/rust-core/EXTENSION_GUIDE.md](./docs/rust-core/EXTENSION_GUIDE.md) | 如何新增一个市场扩展 |
| [docs/rust-core/ABI_V2_DESIGN.md](./docs/rust-core/ABI_V2_DESIGN.md) | 策略 C ABI v2 设计（vtable 已冻结，新能力走可选符号） |
| [docs/rust-core/INTERFACES.md](./docs/rust-core/INTERFACES.md) | UDS JSON-RPC 2.0 方法/事件契约与版本变更记录 |
| [docs/rust-core/SHADOW_EVOLUTION.md](./docs/rust-core/SHADOW_EVOLUTION.md) | 影子进化（按策略参数 / 孪生 / 审计 / apply·rollback） |
| [docs/VERSIONING.md](./docs/VERSIONING.md) | 版本管理体系：单事实来源、`system.version` 契约、更新机制、发布流水线与分支模型 |
| [docs/MARKET_REGIME.md](./docs/MARKET_REGIME.md) / [docs/STRATEGY_EVOLUTION.md](./docs/STRATEGY_EVOLUTION.md) / [docs/CAPACITY_AND_EQUITY.md](./docs/CAPACITY_AND_EQUITY.md) | 市场状态 / 策略进化 / 容量与公平性专题 |
| [docs/DEV_V0_3.md](./docs/DEV_V0_3.md) | v0.3 开发记录 |
| [scripts/README.md](./scripts/README.md) | 运维脚本手册（看门狗 / 备份 / 健康巡检 / TCC） |
| [CONTRIBUTING.md](./CONTRIBUTING.md) | 如何贡献：架构约束、门禁、PR 流程 |
| [CHANGELOG.md](./CHANGELOG.md) | 变更史 |
| [SECURITY.md](./SECURITY.md) | 安全问题私下披露流程 |
| [.env.example](./.env.example) | 环境变量全家福（三族：`BK_*` / `BLITZKRIEG_*` / `POLYMARKET_*`） |

内部开发文档（门禁矩阵、决策记录、迁移日志、GitHub 治理规范等）在 `dev-docs/`，
**不入库、不上 GitHub**；在本地检出中直接阅读。

## 许可证与贡献

本项目以 [MIT](./LICENSE) 许可证开源（版权 (c) 2026 BlitzkriegBot contributors (ceer-quant)）。

欢迎贡献：请先读 [`CONTRIBUTING.md`](./CONTRIBUTING.md)（架构红线与门禁是硬性的），
所有改动走 Pull Request + 全绿检查合入。

---

**再次提醒**：预测市场交易有真实资金风险。本项目按「现状」提供，仅供学习与研究——
默认 dry 模拟模式，请勿在未经充分测试与明确授权的情况下接入真实资金。
