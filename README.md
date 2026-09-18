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
  拉起 / 停止核心并托管其生命周期。

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
│   ├── ui_kit_panel/      # 终端面板应用（bin: ui_kit_panel）
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

### 3.1 构建 Rust 核心

```bash
# 工作区全量构建（默认带 polymarket feature）
cargo build --release --workspace --locked

# 产物
ls target/release/blitzkrieg-core
```

### 3.2 直接以 DryRun 跑核心

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

### 3.3 运行面板（Web UI）

```bash
cargo build --release -p blitzkrieg-ui-kit
./target/release/ui_kit_web --addr 127.0.0.1:51888 --manage
```

浏览器打开 `http://127.0.0.1:51888/panel`。`--manage`（或环境变量
`UIKIT_MANAGE=1`）允许面板拉起 / 停止核心；面板凭据来自环境变量
`BLITZKRIEG_PANEL_USER` / `BLITZKRIEG_PANEL_PASSWORD`，登录换会话 token。

终端面板（可选）：`cargo run -p blitzkrieg-ui-panel`。

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
- `scripts/shutdown-cleanliness-check.mjs` / `parent-monitor-check.mjs` / `readonly-egress-check.mjs` / `crash-recovery-check.mjs` —— 生命周期、只读出口与崩溃恢复验收。

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
| [docs/rust-core/SHADOW_EVOLUTION.md](./docs/rust-core/SHADOW_EVOLUTION.md) | 影子进化（按策略参数 / 孪生 / 审计 / apply·rollback） |
| [CHANGELOG.md](./CHANGELOG.md) | 变更史 |

内部开发文档（门禁矩阵、决策记录、迁移日志、GitHub 治理规范等）在 `dev-docs/`，
**不入库、不上 GitHub**；在本地检出中直接阅读。

---

## 8. 配置

- 核心配置以 **CLI 参数 + 代码默认值**为准（TOML 当前不被核心读取）。
- 面板与第三方凭证通过环境变量提供，参考 [`.env.example`](./.env.example)；
  真实 `.env` 已被忽略，**切勿**提交私钥 / API Key。
- 数据目录、日志（trade/order/position）与 SQLite 库的落盘位置见 `blitzkrieg-core --help`。

---

## 9. 许可

[MIT](./LICENSE)。版权（c）2026 BlitzkriegBot contributors (ceer-quant)。
