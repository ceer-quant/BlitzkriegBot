# BlitzkriegBot

<div align="center">

<img src="assets/logo.png" alt="BlitzkriegBot" width="140"/>

### 以机构级标准设计的现代化量化交易平台

**可视化策略蓝图 · Lua 低代码开发 · 回测实盘同路径 · 风控优先 · 100% Rust**

不是又一个交易机器人——**一个从零设计的交易操作系统**。

dry 模拟出厂默认 · Polymarket / Kalshi / predict.fun / PolySimulator 官方连接器

[![CI](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/ci.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/ci.yml)
[![Security](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/security.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/security.yml)
[![Secret Scan (deep)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/secret-scan.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/secret-scan.yml)
[![Release](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/release.yml/badge.svg)](https://github.com/ceer-quant/BlitzkriegBot/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024%20edition-orange)](https://doc.rust-lang.org/edition-guide/)

<img src="assets/screenshots/panel-markets.png" alt="BlitzkriegBot 行情面板：轮盘倒计时与盘口深度" width="100%"/>

*行情页：轮盘倒计时、UP/DOWN 盘口深度、OBI 失衡、战绩卡。*

**⚠️ 预测市场交易有真实资金风险。本项目默认 dry 模拟模式，仅供学习与研究，不构成投资建议。**

</div>

---

## 为什么是 BlitzkriegBot

它不是「一个会下单的脚本」。它是按**交易操作系统**的思路从零设计的量化平台：微内核是它的内核，市场连接器与策略包是它的驱动，蓝图与面板是它的用户界面，风控是它的内核态。以下每一项都是**当前已实现并在生产使用中的能力**——不是规划。

**先回答你最关心的问题：我能多快拿到结果？**

| 你是谁 | 你能多快拿到结果 | 从哪开始 |
| --- | --- | --- |
| **交易者** | 解压 → 一条命令 → 面板出现：dry 模拟正盯着真实 Polymarket 轮盘跑，你的钱包零风险 | [60 秒上手](#60-秒上手dry-模拟零资金风险) |
| **策略开发者** | 不写 Rust：蓝图上拖一个策略出来，或抄 11 个官方 Lua 包改一改，回放验证后按账户启用 | [Blueprint](#1--blueprint拖拽出一条完整策略不写一行代码) |
| **连接器开发者** | 四个 trait 的市场契约 + 四条官方连接器当参考实现，照着接入下一个市场 | [插件架构](#8--插件架构换市场--写一个插件内核不改一行) |
| **评估者** | 每个功能被什么验证过、哪条链路还没验证，一张证据分级表摊开给你看 | [docs/FEATURES.md](./docs/FEATURES.md) |

---

### 1 · Blueprint：拖拽出一条完整策略，不写一行代码

<div align="center">
<img src="assets/screenshots/panel-blueprint.png" alt="Blueprint 可视化策略画布：节点面板、示例策略、实时 Lua 预览与编译检查" width="100%"/>
</div>

**Visual Strategy Builder。** 从左侧节点面板拖入组件、连线、点开属性面板调参——一个完整的策略工作流就搭好了：

```
数据源节点 ──▶ 条件节点 ──▶ 条件节点 ──▶ 买入/卖出/持有动作节点
              （且 / 或 逻辑组合，守卫链极性可切）
```

- **Low-Code Strategy Development**：节点面板分四组——值·输入（数据源/常量）、判断（条件）、守卫逻辑（且/或）、动作（买入/卖出/持有），覆盖从信号到执行的完整链路。
- **实时编译检查**：画布底部就是内核实时生成的 Lua 预览，「编译通过」徽章亮起才算数——你在画布上看到的，就是内核将要执行的。
- **一键成为策略包**：保存后即是一个标准 Lua 策略包，进策略页按账户启用即可交易；支持导出/导入 JSON，策略可以分享与复用。
- **手写包也能进画布**：任何手写 Lua 策略包都能被蓝图编辑器以源码模式打开继续编辑。

### 2 · Lua 策略系统：不学 Rust，也能写出能上场的策略

- **学习成本低**：一个带 `manifest.json` 的目录就是一个策略包，语言是 Lua——改完保存，内核即可加载，无需编译步骤。
- **开发速度快**：11 个官方策略包（折价套利、单腿/双腿配对、跨所套利、动量族、做市、恐慌接盘等）就在 `user_layer/strategies_lua/` 里，全部开源、当活样板抄。
- **易于测试**：策略跑在内嵌沙箱里（16MiB 内存 / 1e6 指令预算），逻辑失控不会伤到内核；配合回放与回测，先在历史行情上验证再启用。
- **易于扩展**：manifest 声明可调参数域后，参数能进面板热调、进蓝图编辑、进影子进化。

### 3 · Write Once, Trade Everywhere：回测、模拟、实盘是同一条代码路径

回测赚钱、实盘失效，多数时候不是策略错了，是**回测和实盘根本不是同一套代码**。BlitzkriegBot 从内核层面消灭这个分叉：

```
           同一套策略包 · 同一套事件系统 · 同一套风控 · 同一套出场纪律 · 同一套账本结构
                │                    │                     │
                ▼                    ▼                     ▼
            Backtest  ──────▶  Paper (dry 模拟)  ──────▶  Live
```

- 策略在回放、模拟、实盘里加载的是**同一个包、同一份字节码**，不是两套实现。
- dry 与 live 的账务路径由核心对拍门禁（`core-parity`）做双核逐字校验——这不是愿景，是 CI 里每天在跑的测试。
- 诚实条款：各能力的验证等级（实盘验证 / 离线验证 / 仅测试覆盖 / 未验证）以 [`docs/FEATURES.md`](./docs/FEATURES.md) 为权威——包括 Live 场地链路的当前分级。

### 4 · Risk First：先活下来，再谈交易

风控不是插件，是系统的承重墙。任何策略的任何一笔单，都先过同一道闸：

- **硬上限不可豁免**：持仓数、单笔名义额等硬限制写在内核里，策略声明面碰不到它。
- **九项系统限额与退出纪律**面板热编辑：止损、最小剩余时间、跟踪止盈……改完即生效，不用重启。
- **先活下来的机制是真的**：账户维熔断与自愈（良性拒单不喂计数、场地恢复自动解冻）、账本与场地报数逐窗对账、无法归属的成交回报响亮告警、场地现金异动探测器。
- **结构性只读**：`--readonly` 下进程从根上不构造下单通道——不是「不点按钮」，是根本没有那条路。

### 5 · 100% Rust：为长时间运行而建

生产路径全部是 Rust——没有 GC 停顿、没有解释器抖动、没有运行时依赖：

- **你的收益**：部署 = 解压一个目录；运行 = 一条命令；不需要装 Node、Python 或任何运行时环境。
- **长时间运行可靠性**：单二进制 + 崩溃自动重建 + 只收自己拉起进程的生命周期托管，重启机器后一条 `blitzkrieg run` 回到全部状态。
- **安全工程**：内存安全语言 + 双平台 CI + CodeQL + 深度密钥扫描 + 凭据只走环境变量。

### 6 · 毫秒级交易引擎

不是为了跑分，是为了**在行情断流的瞬间不拿着旧价格下单**：

- 引擎节拍默认 50ms、毫秒级可调；行情断流 8 秒内被盘口新鲜度保护发现并拒用陈旧盘口——宁可不开仓，不按过期数据交易。
- 面板 ↔ 内核走 Unix 域套接字 + JSON-RPC 2.0 换行分帧，本机零网络栈开销。
- 回放器按 deadline 驱动调度，历史行情回放的时间轴与实盘节拍同构——回测测到的时序就是实盘的时序。

### 7 · 机构级架构：按「跑十年」的标准设计

- **微内核**：交易核心市场无关、零策略出厂；撮合、下单、持仓、风控、对账、账本各居其位。
- **冻结的扩展 ABI**：策略 C ABI v2 的 vtable 已冻结，新能力走可选符号——老策略包不会被升级弄坏。
- **版本单事实来源**：版本号只有一处定义，tag 由发布流水线构建双平台资产，同一 tag 永不覆盖。
- **全链审计**：订单、成交、持仓、决策、场地异动全部落 JSONL 审计；出事有据可查，升级保留上一代日志。

### 8 · 插件架构：换市场 = 写一个插件，内核不改一行

交易核心不含任何交易所 SDK。四条官方连接器（**Polymarket / Kalshi / predict.fun / PolySimulator**）都实现同一份契约（`core/market_api` 的 `DataFeed` / `MarketDiscovery` / `OrderExecutor` / `MarketPlugin`）：

- 新市场 = 一个扩展 crate + feature 注册，内核零改动。
- 策略侧同理：Lua 包与 cdylib（C ABI v2）都是标准投放格式，社区策略与官方策略走同一扇门。

### 9 · 现代化界面：面板与终端双形态

- **Web 面板**：Vue 3 + shadcn 风格组件体系（reka-ui + Tailwind）+ vue-flow 画布——行情、挂单、持仓、账本实时刷新，策略/蓝图/裁决流/进化四页签，多账户切换器，首次引导内置。
- **TUI 终端面板**：`blitzkrieg tui`，同一套 ui_kit 格式化函数，服务器上没有浏览器也能操作。
- 面板默认只听本机 `127.0.0.1:51888`，跨站来源一律 403。

---

## 60 秒上手（dry 模拟，零资金风险）

**路线 A：不装任何工具（推荐）**

到 [Releases](https://github.com/ceer-quant/BlitzkriegBot/releases) 下载对应平台（Linux x86_64 / macOS aarch64）的 `tar.gz`，解压后：

```bash
cd <解压目录>
bin/blitzkrieg run
```

浏览器打开 **<http://127.0.0.1:51888/panel>** ——完事。它在盯真实 Polymarket 轮盘、按真实盘口在本地模拟成交，你的钱包没有任何风险。

**路线 B：源码构建（开发者）**

```bash
git clone https://github.com/ceer-quant/BlitzkriegBot.git
cd BlitzkriegBot && cargo build --release --workspace --locked
bash scripts/install-blitzkrieg-shim.sh   # 可选：把 blitzkrieg 装进 PATH
blitzkrieg run
```

**三条路把平台用起来**：① 策略页打开某个官方策略的开关（按账户启用）；② 蓝图页拖一个自己的策略出来保存启用；③ 让网页能下命令——`.env.example` 复制为 `.env`，设置 `BLITZKRIEG_PANEL_USER` / `BLITZKRIEG_PANEL_PASSWORD`。

<details>
<summary><b>Windows 用户：用 WSL2 安装</b>（本平台原生不支持 Windows，依赖 Unix 域套接字）</summary>

1. 管理员 PowerShell 执行 `wsl --install`，重启（老机器先确认 `wsl -l -v` 的 VERSION 是 2）。
2. Ubuntu 里装工具链：
   ```bash
   sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev git curl
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
   source "$HOME/.cargo/env"
   ```
3. **代码必须放在 WSL 文件系统里**（`~` 下），不要放 `/mnt/c/...`（9P 跨盘 I/O 慢一个数量级）：
   ```bash
   git clone https://github.com/ceer-quant/BlitzkriegBot.git && cd BlitzkriegBot
   cargo build --release --workspace --locked && bash scripts/install-blitzkrieg-shim.sh && blitzkrieg run
   ```
4. Windows 浏览器直接打开 `http://127.0.0.1:51888/panel`（WSL2 localhost 自动转发，`wsl hostname -I` 的内部 IP 不用）。

三个 WSL 特有坑：**`wsl --shutdown` 或最后一个 WSL 进程退出约 8 秒后会杀掉机器人**（长跑保持 Ubuntu 窗口开着，或用 Windows 任务计划执行 `wsl -e bash -lc "cd ~/BlitzkriegBot && blitzkrieg run"`）；重启电脑后需手动 `blitzkrieg run`；数据在 `\\wsl$\Ubuntu\home\<用户名>\BlitzkriegBot\data\` 可从 Windows 直接备份。内存紧张可在 `%UserProfile%\.wslconfig` 写 `[wsl2]` + `memory=4GB`。

</details>

## 安全默认与红线

- **dry 出厂默认**；唯一碰真钱的开关是 `DRY_RUN=false` / `--mode live`，且需真人明确授权——项目硬性规则。
- **`--readonly` 结构性只读**：压过一切 live 请求，连下单通道都不构造。
- **策略出厂全关**：内核 0 个启用策略；装包只是「已安装」，启用真值是「账户 × 策略」复合键。
- **密钥只走环境变量**：`.env` 被 git 忽略，绝不入库；安全问题走 [`SECURITY.md`](./SECURITY.md) 私下披露。
- **数据与你自己的判断归你**：本项目按「现状」提供，不构成投资建议，不对任何损失负责。

## FAQ

**Q：我要装 Node.js 吗？** 不用。生产是 100% Rust；Node 22 只跑开发门禁脚本（零 npm 依赖），只跑平台用不到。

**Q：面板打不开？** 先 `blitzkrieg stop` 再 `blitzkrieg run`（端口被旧栈占着时新栈绑不上）；地址是 `http://127.0.0.1:51888/panel`。

**Q：策略装了为什么不动？** 三道门：包在 `user_layer/strategies_lua/`（安装）→ 面板策略页按账户打开开关（启用）→ 请求了策略但一个都没加载成功时内核拒绝启动（保护）。启动日志逐包打印装载与启用状态。

**Q：数据存哪里？** 仓库根 `data/` 下：成交/订单/持仓 JSONL、`accounts.toml`、策略启用集。`blitzkrieg backup` 一键备份（每日轻量 + 周日全量）。

**Q：怎么升级？** `blitzkrieg upgrade`（构建 → 停栈 → 换装 → 起栈 → 身份校验，全自动，上一代日志轮转保留）；或 `git pull && cargo build --release --workspace --locked`。正式版本以 [Releases](https://github.com/ceer-quant/BlitzkriegBot/releases) tag 为准（当前 **0.4.1**）。

**Q：能拿真钱跑吗？** 平台实现了 live 模式，但开启需真人明确授权——项目硬性规则；各能力的验证等级以 [docs/FEATURES.md](./docs/FEATURES.md) 证据分级为准。钱包里放你亏得起的钱，本项目不对任何损失负责。

**Q：支持 Windows 吗？** 原生不支持，唯一受支持用法是 WSL2（见上方折叠节）。

**Q：怎么彻底停掉？** `blitzkrieg stop`——面板与核心一起收，重复执行无害，只碰自己 socket 上的进程族。

## 文档导航

| 文档 | 内容 |
| --- | --- |
| [docs/FEATURES.md](./docs/FEATURES.md) | 功能清单与**证据分级**（哪个功能被什么验证过） |
| [docs/rust-core/ARCHITECTURE.md](./docs/rust-core/ARCHITECTURE.md) | Rust 分层架构与扩展体系 |
| [docs/rust-core/STRATEGY_GUIDE.md](./docs/rust-core/STRATEGY_GUIDE.md) | 编写与加载策略 |
| [docs/rust-core/EXTENSION_GUIDE.md](./docs/rust-core/EXTENSION_GUIDE.md) | 新增一个市场扩展 |
| [docs/rust-core/INTERFACES.md](./docs/rust-core/INTERFACES.md) | UDS JSON-RPC 2.0 契约 |
| [docs/rust-core/ABI_V2_DESIGN.md](./docs/rust-core/ABI_V2_DESIGN.md) | 策略 C ABI v2 设计 |
| [docs/VERSIONING.md](./docs/VERSIONING.md) | 版本单事实来源与发布流水线 |
| [scripts/README.md](./scripts/README.md) | 运维脚本手册（看门狗/备份/巡检） |
| [CONTRIBUTING.md](./CONTRIBUTING.md) | 贡献流程（架构红线与门禁是硬性的） |
| [.env.example](./.env.example) | 环境变量全家福（`BK_*` / `BLITZKRIEG_*` / `POLYMARKET_*`） |

<details>
<summary><b>给开发者：架构、目录、常用旗标与门禁</b></summary>

### 工作原理

```
┌──────────────────────────────┐        spawn + 生命周期托管
│  blitzkrieg 网关（Rust）      │ ───────────────────────────────┐
│  Vue 面板 · snapshot/命令 API │                                  │
│  127.0.0.1:51888             │ ◀── UDS（$TMPDIR/*.sock）        │
└──────────────────────────────┘     JSON-RPC 2.0，'\n' 分帧       │
                                            ▲                       ▼
┌───────────────────────────────────────────┴───────────────────────────┐
│                         blitzkrieg-core（Rust）                          │
│  engine/ome · order · position · risk · exit_policy · reconcile        │
│  strategy_engine（cdylib + Lua 热加载，内核 0 策略） · shadow/evolution │
│  hedge（跨平台两腿对冲） · kline · execution_policy                    │
│  market registry（core 市场无关，无任何交易所 SDK）                      │
└────────────────────────────────────────┬───────────────────────────────┘
                                 │ 依 feature 挂接
                    ┌────────────┴────────────┐
                    │ extensions/*            │  polymarket / kalshi / predict.fun / polysimulator
                    └─────────────────────────┘
```

- 面板 ↔ 核心：本机 Unix 域套接字 + 换行分帧 JSON-RPC 2.0；IPC 契约唯一来源是 Rust serde 结构体（[`docs/rust-core/INTERFACES.md`](./docs/rust-core/INTERFACES.md)）。
- 市场抽象契约：`core/market_api` 的 `DataFeed` / `MarketDiscovery` / `OrderExecutor` / `MarketPlugin` trait——换交易所 = 写一个扩展 crate，内核不改一行。

### 目录结构

```
core/blitzkrieg_core   交易核心（市场无关）      core/market_api        市场扩展契约
extensions/*           polymarket/kalshi/predictfun/polysimulator
user_layer/            strategy_api · lua_runtime · strategies_lua（官方 11 包）· configs
ui/                    ui_kit（Web 网关）· ui_kit_panel（TUI + 单二进制启动器）· webapp（Vue）
scripts/               门禁与运维脚本（零 npm 依赖）
docs/ · dev-docs/      公开文档 · 内部文档（不入库）
```

### 配置与旋钮

- 配置优先级：**命令行参数 > `BK_*` 环境变量 > `user_layer/configs/default.toml` > 代码默认值**；启动时每个非默认值打印 `key=value (source)`；未知键逐个告警，绝不静默忽略。
- 常用旗标：`--mode dry|live` · `--readonly` · `--engine --feed-ws` · `--assets BTC,ETH,SOL,XRP` · `--round-sec 900 --min-time-left 180` · `--max-positions 2 --max-order-notional 6` · `--seed-balance 1000` · `--socket <path>` · `--enable/disable-strategy` · `--replay`。全表见 `blitzkrieg --help` 与 [`docs/rust-core/ARCHITECTURE.md`](./docs/rust-core/ARCHITECTURE.md)。
- 服务器部署：`BLITZKRIEG_PANEL_ADDR=0.0.0.0:51888`（凭据必需；跨站一律 403）。

### 本地验收门禁（提交前必须全绿）

```bash
cargo build --release --workspace --locked && cargo test --workspace --locked
node scripts/cycle-check.mjs          # 订单链端到端：挂单 → 成交 → 持仓 → 重估
node scripts/core-parity.mjs          # 核心 dry/live 双核对拍
bash scripts/secret-scan.sh           # 零依赖密钥扫描
cd ui/webapp/webui && npm run check:all   # 面板前端检查
```

### 分支与版本

`main` 仅经 PR 合入（全绿检查）；版本唯一事实来源是根 `Cargo.toml` 的 `version`（当前 **0.4.1**），tag `v<version>` 由 Release 工作流构建双平台资产，同一 tag 永不覆盖。详见 [`docs/VERSIONING.md`](./docs/VERSIONING.md)。

</details>

## 许可证

[MIT](./LICENSE) © 2026 BlitzkriegBot contributors (ceer-quant)。欢迎贡献：请先读 [`CONTRIBUTING.md`](./CONTRIBUTING.md)。

---

**再次提醒**：预测市场交易有真实资金风险。本项目按「现状」提供，仅供学习与研究——默认 dry 模拟，请勿在未经充分测试与明确授权的情况下接入真实资金。
