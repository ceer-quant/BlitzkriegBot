# BlitzkriegBot

> 私有量化交易系统 —— **Rust 交易核心 + Node.js 外壳 + 可插拔市场扩展**。
> 所有涉及资金、订单、风控与状态一致性的逻辑都在 Rust 侧；Node 只负责 UI、参数编排与人类可读日志。

- **仓库**：`ceer-quant/BlitzkriegBot`（**private**）
- **默认运行模式**：`dry`（DryRun 模拟）。**Live 交易默认关闭且受硬约束保护，不得在未授权下开启。**
- **工具链**：Rust（edition 2024）+ Node.js `>= 22`

---

## 1. 它是什么

BlitzkriegBot 是一个面向**轮盘型预测市场（当前为 Polymarket 加密二元市场）**的自动化交易系统，
采用严格的「**硬核在 Rust，外壳在 Node**」分层：

- **Rust 核心**：确定性的撮合 / 下单 / 持仓 / 风控 / 对账状态机，单二进制，内存安全，无市场 SDK 依赖。
- **市场扩展**：核心市场无关；具体交易所通过扩展 crate 实现，并以 Cargo feature 挂接。官方默认扩展为
  **Polymarket**（CLOB 下单、WS 行情、Gamma 轮盘发现）。
- **策略层**：核心内置已验证策略，并支持通过 `user_layer/strategy_api` 的 trait ABI 在运行时
  加载用户动态库（cdylib，feature `strategy-loading`），热插拔而无需改核心。
- **Node 外壳**：负责拉起并托管核心进程、提供本地 HTTP/WebSocket 网关与 UI、做参数编排和日志展示；
  **不持有私钥、不直接调用 CLOB、不维护订单状态机、不做资金计算、不吞掉原始错误**。

核心与外壳之间通过本机 **Unix Domain Socket + 换行分帧的 JSON-RPC 2.0** 通信，
契约唯一来源是 Rust 的 serde 结构体，Node 侧用 zod 镜像校验。

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
│   ├── ui_kit/            # Rust UI 组件库（bins: ui_kit_web / ui_kit_app）
│   └── ui_kit_panel/     # 面板应用（bin: ui_kit_panel）
├── src/                   # Node.js 外壳（网关、UI、核心托管客户端）
├── scripts/               # 门禁与运维脚本（cycle-check / core-parity / secret-scan …）
├── docs/                  # 架构、接口、策略与运维文档
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
./target/release/blitzkrieg-core --help
```

只构建核心：

```bash
npm run core:build        # = cargo build --release --manifest-path core/blitzkrieg_core/Cargo.toml
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

### 3.3 运行 Node 外壳（网关 + UI）

```bash
npm install                 # 需 Node >= 22
npm run build
npm start                   # node dist/index.js，本地网关与 UI
```

开发模式：`npm run dev`（tsx 热重载）。Node 会在需要时按既定搜索路径自动拉起
`target/release/blitzkrieg-core`，并通过 UDS 托管其生命周期。

### 3.4 UI

Rust UI 套件位于 `ui/`：Web（`ui_kit_web`）、桌面应用骨架（`ui_kit_app`）
与终端面板（`ui_kit_panel`，ratatui）。

```bash
cargo run -p blitzkrieg-ui-panel
```

---

## 4. 工作原理

```
┌──────────────────────────────┐        spawn + 生命周期托管
│  Node.js 外壳（src/）         │ ───────────────────────────────┐
│  UI · 参数编排 · 人类日志     │                                  │
│  本地 HTTP / WebSocket 网关   │ ◀── UDS（$TMPDIR/*.sock）        │
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

- **Node → Rust**：`{jsonrpc:"2.0",id,method,params}`
- **Rust → Node（响应）**：`{jsonrpc:"2.0",id,result|error}`
- **Rust → Node（事件）**：`{jsonrpc:"2.0",method:"core.event",params:{kind,...}}`

市场抽象契约见 `core/market_api`：`DataFeed`、`MarketDiscovery`、`OrderExecutor`、
`MarketPlugin`、`MarketHost` 等 trait；新增交易所 = 写一个扩展 crate 并用 feature 注册，核心不改一行。

---

## 5. 开发与门禁

提交前**必须**在仓库根跑完全部门禁：

```bash
# Rust
cargo build --release --workspace --locked
cargo test --workspace --locked            # 核心 + 各 crate 单测

# Node / TS
npm run typecheck
npm test
npm run build

# DryRun 订单链端到端：挂单 → 成交 → 持仓 → 重估（自起临时 core，隔离 socket）
node scripts/cycle-check.mjs

# 零依赖密钥扫描（工作树；--history 扫全历史）
bash scripts/secret-scan.sh
```

面板前端（`ui/webapp/webui/`）另有一套 bare-Node 检查：

```bash
cd ui/webapp/webui && npm run check:all
```

其他常用校验脚本：

- `npm run core:parity` —— Node 与 Rust 核心行为一致性比对。
- `npm run core:observe` —— DryRun 观察。
- `cargo build --release --workspace` 后 `ui/` 网关可发现根 `target/` 下的核心。

> **禁止**在未通过上述验证时提交到 `main`；`cargo fmt`/`clippy` 与 `npm audit`
> 当前在 CI 中为**建议性**（历史债，见 `docs/KNOWN_ISSUES.md` KI-13 / KI-14），
> 不阻塞但每次运行都会报告。
>
> ⚠️ **CI 只覆盖上面的一部分。** 仓库里还有 **22 个可运行的验证门禁**（13 个有 npm 别名，
> 如 `core:strategy-*`、`core:trend-follow`、`core:mean-reversion`、`strategy:devcheck`、
> `scale:plugins`、`scale:feed`、`ui:plugin-*`、`tui:check`；另 9 个只能直接 `node scripts/…` 跑，
> 如 `cycle-check.mjs`、`order-recovery-check.mjs`、`backtest-check.mjs`）
> **没有接进 CI，必须人工在本地跑**。这正是 [`docs/KNOWN_ISSUES.md`](./docs/KNOWN_ISSUES.md)
> KI-9 记录的当前最大回归保护缺口；完整门禁矩阵见
> [`docs/DEVELOPMENT.md`](./docs/DEVELOPMENT.md) §3。

---

## 6. 分支模型与协作

- 长期分支：`main`（发布分支，按约定仅经 PR 合入）、`develop`（集成分支）。
- 短期分支：`feat/*`、`fix/*`、`chore/*`、`release/*`（历史上亦用 `feature/*`，等价）。
- 所有合入走 Pull Request + 全绿检查；Issue/PR 模板与标签体系已内置。
- **远端名为 `ceer`**（`origin` 已移除）；`gh` CLI 未安装，Issue/PR 操作走 curl + REST API。
- 完整流程（分支 → 提交 → PR → CI → squash 合并 → 清分支）见
  [`docs/GITHUB_GOVERNANCE.md`](./docs/GITHUB_GOVERNANCE.md)。

> ⚠️ **分支保护实际上没有服务端强制。** 本仓库为 private + GitHub Free，
> 对分支保护/规则集的 API 调用返回 `403 Upgrade to GitHub Pro`，原生 Secret Scanning /
> Push Protection 亦不可用。「仅经 PR 合入」**目前只是流程约定**——见
> [`docs/KNOWN_ISSUES.md`](./docs/KNOWN_ISSUES.md) KI-17。

### 不可逾越的安全红线

- 禁止在未授权下启用 **Live** 交易。
- 禁止修改真实凭证、私钥、API Key；机密一律走环境变量 / secret，绝不入库。
- 禁止删除未经备份的文件；禁止在未验证时提交主分支；禁止「顺手」改动业务逻辑。
- 发现漏洞请走 [`SECURITY.md`](./SECURITY.md) 的私下披露流程，勿在公开 Issue 粘贴机密。
- **不得声称本项目已通过安全审计**（原因见 [`docs/KNOWN_ISSUES.md`](./docs/KNOWN_ISSUES.md)）。

---

## 7. 文档

### 7.1 先读这五份（权威）

| 文档 | 内容 |
| --- | --- |
| [HANDOFF.md](./HANDOFF.md) | **项目交接入口**：当前状态、阅读序、跑起来、待办 |
| [docs/FEATURES.md](./docs/FEATURES.md) | **功能文档与完成度**（证据等级：实盘验证 / 离线验证 / 仅测试覆盖 / 未验证 / 部分 / 规划） |
| [docs/KNOWN_ISSUES.md](./docs/KNOWN_ISSUES.md) | **已知但未修复的问题**：严重度、证据（文件:行）、影响面、处理顺序 |
| [docs/DEVELOPMENT.md](./docs/DEVELOPMENT.md) | **开发规范**：目录职责、门禁矩阵、不可触碰的边界、工具链陷阱 |
| [docs/GITHUB_GOVERNANCE.md](./docs/GITHUB_GOVERNANCE.md) | **GitHub 使用规范与身份信息**：分支、提交、Issue/PR、合并流程、REST 配方 |

> 这五份是**当前权威**。`docs/` 下的其余文档多为迁移前产物，冲突时以上述五份为准
> （完整文档地图与分级见 [docs/DEVELOPMENT.md](./docs/DEVELOPMENT.md) §11）。

### 7.2 架构与指南

| 文档 | 内容 |
| --- | --- |
| [docs/RUST_CORE.md](./docs/RUST_CORE.md) | Rust 核心架构、进程/IPC 契约、P0 状态与目录改名对照 |
| [docs/blitzkrieg/ARCHITECTURE.md](./docs/blitzkrieg/ARCHITECTURE.md) | 分层架构与扩展体系 |
| [docs/blitzkrieg/STRATEGY_GUIDE.md](./docs/blitzkrieg/STRATEGY_GUIDE.md) | 如何编写与加载策略 |
| [docs/blitzkrieg/EXTENSION_GUIDE.md](./docs/blitzkrieg/EXTENSION_GUIDE.md) | 如何新增一个市场扩展 |
| [docs/blitzkrieg/ABI_V2_DESIGN.md](./docs/blitzkrieg/ABI_V2_DESIGN.md) | 策略 C ABI v2 设计（vtable 已冻结，新能力走可选符号） |
| [docs/blitzkrieg/SHADOW_EVOLUTION.md](./docs/blitzkrieg/SHADOW_EVOLUTION.md) | 影子进化（按策略参数 / 孪生 / 审计 / apply·rollback） |
| [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md) | 系统总体设计与数据流 |
| [docs/QUICK_START.md](./docs/QUICK_START.md) | 更完整的上手指南 |
| [docs/TRADING.md](./docs/TRADING.md) | 交易执行、机器人与风控 |
| [docs/DEPLOYMENT.md](./docs/DEPLOYMENT.md) | 环境变量、容器与部署 |
| [docs/ROADMAP_INSTITUTIONAL.md](./docs/ROADMAP_INSTITUTIONAL.md) | 机构化路线图 |

### 7.3 流程、决策与历史

| 文档 | 内容 |
| --- | --- |
| [CONTRIBUTING.md](./CONTRIBUTING.md) | 贡献流程入口 |
| [docs/AI_WORKFLOW.md](./docs/AI_WORKFLOW.md) | 角色、硬约束、分支与门禁规范（人机协作红线） |
| [docs/DECISIONS_PENDING.md](./docs/DECISIONS_PENDING.md) | 待决策事项与**用户裁决**（D-1…D-19） |
| [docs/ROADMAP_V0_1.md](./docs/ROADMAP_V0_1.md) | 0.1 里程碑 E1–E7 的验收与进度 |
| [docs/blitzkrieg/MIGRATION_LOG.md](./docs/blitzkrieg/MIGRATION_LOG.md) | **权威变更史**（§1–§48）：已修复缺陷的来龙去脉 |
| [docs/reports/](./docs/reports/) | 专题报告（回放/留出段证据、治理报告等） |


---

## 8. 配置

- 核心配置以 **CLI 参数 + 代码默认值**为准（TOML 当前不被核心读取，详见 D-1）。
- Node 外壳与第三方凭证通过环境变量提供，参考 [`.env.example`](./.env.example)；
  真实 `.env` 已被忽略，**切勿**提交私钥 / API Key。
- 数据目录、日志（trade/order/position）与 SQLite 库的落盘位置见各文档与 `--help`。

---

## 9. 许可

[MIT](./LICENSE)。
