# 功能清单与完成度（FEATURES）

> **本文是「这个项目现在能做什么、做到什么程度、证据在哪」的权威盘点。**
>
> 阅读约定 —— 每个功能都标了**证据等级**，请按等级理解「完成」的含金量：
>
> | 等级 | 含义 |
> | --- | --- |
> | **✅ 已实盘验证（dry）** | 在真实 dry 内核上跑过、有运行日志/面板数据 |
> | **✅ 已验证（离线）** | 有确定性离线证据（测试 / 回放 / 门禁），**未在 dry 实况下长期运行** |
> | **✅ 已实现（测试覆盖）** | 有实现与单测/集成测试，**无实况或回放证据** |
> | **⚠️ 已实现（未验证）** | 有代码，**任何环境下都没跑通验证** |
> | **🚧 部分实现** | 做了主体，有明确缺口 |
> | **📋 规划中** | 设计已定，未实现 |
>
> 最关键的阅读提示：**整条 Live（真实下单）链路属于 ⚠️ 未验证**——全程 DRY。
> 详见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) §1。
>
> 最后核对：2026-09-18（KI-1/KI-7/KI-10/KI-11/KI-23 已关闭；**E12 全验收项闭环**：
> 崩溃上报 §60 + 一体化启动 §64 / PR #115）。

---

## 0. 一句话总览

BlitzkriegBot 是一个面向 **Polymarket 加密二元（UP/DOWN）预测市场**的自动化交易系统：
**Rust 交易内核 + 可插拔市场扩展 + 多策略宿主 + 影子进化 + Vue 面板 / TUI**。

- **核心交易链路**：已跑通并有实况 dry 证据。
- **多策略与影子进化**：已实现，有离线对照实验证据，实况下**尚未长期运行**。
- **Live 交易**：代码在位，**从未真实下过一单**，是最大的未验证面。
- **产品化程度**：Vue 面板（5 页）可用，TUI 可用，但「面向多用户 / 多插件 / 多人协作」
  仍是进行中的 Epic（#59）。

---

## 1. Rust 交易内核（`core/blitzkrieg_core`）

架构与模块职责见 [`RUST_CORE.md`](RUST_CORE.md) 与 [`blitzkrieg/ARCHITECTURE.md`](blitzkrieg/ARCHITECTURE.md)。

| 功能 | 状态 | 证据 |
| --- | --- | --- |
| **订单引擎（OME）**：意图 → 挂单 → 成交 → 撤单 全生命周期状态机 | ✅ 已实盘验证（dry） | `data/orders/orders.jsonl` 落盘（含终态）；`scripts/core-parity.mjs` 22 项 |
| **资金账本（Ledger）**：可用/冻结、费用计支出、本金＋净利润口径 | ✅ 已实盘验证（dry） | 面板余额卡与真实余额对账；`MIGRATION_LOG §7`（费用记账统一） |
| **持仓管理**：开仓 / 实时重估 / 部分平仓 / 移动止盈 | ✅ 已实盘验证（dry） | 面板持仓表实时价格与盈亏；`cycle-check.mjs` 全链路 |
| **风控引擎（RiskGate）**：单笔名义额、持仓数、单 token 去重、`NoMarkets` 结构前提 | ✅ 已实盘验证（dry） | 实况中确实拒绝过超限订单（日志 `--max-positions 2` 拒单） |
| **连亏熔断（LossBreaker）** | ✅ 已验证（离线） | **按策略分片**（KI-10 / D-18 A，`c06c4ba9`）：任一腿连亏不再冻结全核；日亏上限与 kill switch 仍为全局 |
| **配置文件（TOML）** | ✅ 已验证（离线） | `config.rs` 13 个单测（KI-11 / §59）：`user_layer/configs/*.toml` 真实生效，优先级 CLI > `BK_*` env > TOML > 代码默认，每个值带来源溯源 |
| **kill switch**（`risk.kill` / `risk.resume`） | ✅ 已实现（测试覆盖） | IPC 命令面；TUI 有醒目红条提醒 |
| **出场策略（ExitConfig）**：止损 12% / 移动止盈 arm 15% 回吐下限 8% / 时间兜底 | ✅ 已实盘验证（dry） | `dev-docs/reports/HFT_OPTIMIZATION_REPORT.md`（内部）；冻结留出段 walk-forward |
| **行情接入（`--feed-ws`）**：Polymarket 走 REST 轮询，Binance 现货走 WS | ✅ 已实盘验证（dry） | 实况行情跳动；`MIGRATION_LOG §57`（REST 轮询替代 WS 通道，流量降 95%） |
| **轮盘发现（Gamma）** | ✅ 已实盘验证（dry） | 面板轮次头部实时刷新 |
| **崩溃恢复：订单**（孤儿订单防护） | ✅ 已验证（离线） | `scripts/order-recovery-check.mjs`；启动孤儿清算（`startup sweep cancelled N orphan order(s)`） |
| **崩溃恢复：持仓**（失管防护） | ✅ 已验证（离线） | `position_db.rs` + `restore_positions()`；`scripts/position-recovery-check.mjs` |
| **内核接管（adopt）**：重复客户端不重启风暴 | ✅ 已验证（离线） | `scripts/core-adopt-check.mjs`；socket 改名后可发现并领养旧名 |
| **事件驱动回测器（`--backtest`）** | ✅ 已验证（离线） | `scripts/backtest-check.mjs` 21/21，live vs 回放**逐位相等**（净盈亏 5.12208717） |
| **行情归档（默认开启）**：分段轮转 256MB、无会话上限、<5GB 停录、单写者锁 | ✅ 已实盘验证（dry） | 真实 feed 13 分钟 / 1,025,963 事件 / 145.7 MB 重放一致 |
| **UDS + JSON-RPC 2.0 IPC** | ✅ 已实盘验证（dry） | 31 个方法（见 §3.1）；Node 侧 zod 镜像校验 |
| **C ABI v2 策略接口** | ✅ 已验证（离线） | CI 真构建并驱动两个真实 cdylib；`foreign_parity.rs` 逐信号对拍 |

### 1.1 IPC 方法面（31 个）

```
core.ping · core.ready · core.event            books.snapshot · books.top
engine.book · engine.stats · engine.round · engine.markets · engine.round
orders.place · orders.list · orders.cancel · orders.cancel_all · orders.reconcile
positions.list · positions.exit               ledger.balance · trades.history · trades.summary
risk.kill · risk.resume                       spot.price
strategy.list · strategy.enable · strategy.load · strategy.unload · strategy.reload
extension.list · extension.enable · extension.disable
market.list
```

---

## 2. 策略

### 2.1 三个内建策略

| 策略 | 方向 | 默认状态 | 可进化旋钮 | 留出段证据 |
| --- | --- | --- | --- | --- |
| `spread_arb`（疯狗 / HFT 主策略） | 盘中抄底（趋势确认后的深跌反抽） | **启用** | 4 个 | [`HFT_OPTIMIZATION_REPORT.md`](reports/HFT_OPTIMIZATION_REPORT.md) |
| `trend_follow`（趋势跟随） | 顺势追涨（突破/动量确认） | **禁用** | 6 个 | [`TREND_FOLLOW_HOLDOUT_REPORT.md`](reports/TREND_FOLLOW_HOLDOUT_REPORT.md) |
| `mean_reversion`（逆向 / 均值回归） | 逆势接（超跌反弹） | **禁用** | 6 个 | [`MEAN_REVERSION_HOLDOUT_REPORT.md`](reports/MEAN_REVERSION_HOLDOUT_REPORT.md) |

> **默认禁用是刻意的安全属性**：升级内核**不会**改变在运行会话的交易行为。
> 启用是显式的运维动作（`strategy.enable`）。
>
> `mean_reversion` 声明了 `momentum` 门禁豁免（`timing: false, momentum: true`）——
> 「现货正逆着持仓走」正是它要入场的情形。豁免**只作用于它自己的候选单**，
> 且安全边界物理上不可豁免。

### 2.2 旋钮（可进化参数）

| 策略 | 旋钮 |
| --- | --- |
| `spread_arb` | `trend_min_price`（0.55, [0.50,0.95]）· `trend_entry_factor`（0.98, [0.80,1.00]）· `trend_max_entry_price` · `trend_broken_price` |
| `trend_follow` | `momentum_window_sec`（30, [5,300]）· `min_move_pct`（3.0, [0.5,10]）· `min_confirm_price`（0.55, [0.50,0.95]）· `break_price`（0.45, [0.05,0.60]）· `max_entry_price`（0.88, [0.50,0.98]）· `max_spread_pct`（3.0, [0.10,20]） |
| `mean_reversion` | `lookback_sec`（120, [10,600]）· `min_drop_pct`（10, [1,50]）· `max_price`（0.35, [0.10,0.60]）· `entry_factor`（0.98, [0.80,1.00]）· `max_spread_pct`（8, [0.10,25]）· `cooldown_sec`（60, [0,600]）· `trend_window_sec`（600, [0,600]，0 = 关闭闸门）· `trend_drop_pct`（30, [5,90]）——后两个是 #176 的趋势闸门：单个 token 在 600s 窗口内自高点回落 ≥ 30% 即判定为单边下行，不再抄底 |

**旋钮由策略自证**：trait `evolvable_knobs()`（外挂为可选符号 `bk_strategy_evolvable_knobs`）。
**不声明 = 明确不可进化**。配置若落在默认取值域之外，取值域会**自动扩宽以包含现值**
（声明一个自己都不符合的box 会让每次提案都成域违规）。

### 2.3 策略工程化

| 功能 | 状态 | 证据 |
| --- | --- | --- |
| 按策略资金分配（sizing / `max_positions` / 配额） | ✅ 已验证（离线） | `node scripts/strategy-limit-check.mjs`（E2-a / #26） |
| 按策略门禁豁免（`GateExemptions` + 可选符号） | ✅ 已验证（离线） | `node scripts/strategy-gate-check.mjs`（E2-b / #27） |
| 影子进化按策略化（参数/孪生/审计/回滚四维隔离） | ✅ 已验证（离线） | `node scripts/strategy-evolution-check.mjs`（E2-c / #28） |
| 策略生命周期：`strategy.load` / `unload` / `reload`（木马式原子交换，带审计） | ✅ 已验证（离线） | E9-b / #64；PR #64 |
| 一键脚手架（零 unsafe 的 `SafeStrategy`） | ✅ 已验证（离线） | `node scripts/blitzkrieg-new-strategy.mjs`；E9-a / #60 |
| 开发者全链路门禁（模板→构建→load→enable→信号→旋钮→孪生） | ✅ 已验证（离线） | `node scripts/strategy-devcheck.mjs`；PR #63 |
| 拒绝原因分布（策略侧自助排障） | ✅ 已验证（离线） | `engine.stats.strategies[].rejectionCauses`；E9-c / #65 |
| 启动期策略选择（`--enable-strategy` / `--disable-strategy`） | ✅ 已验证（离线） | E4-a；与 IPC `strategy.enable` 同一入口 |

---

## 3. 影子进化（Shadow Evolution）

| 项 | 状态 |
| --- | --- |
| 机制 | ✅ 已验证（离线）——**默认 `enabled: false`**（opt-in） |
| 对照实验框架（A/B：对照组关 / 实验组开） | ✅ 已验证（离线）——`core/blitzkrieg_core/examples/shadow_evolution_ab.rs` |
| 无停机热更新（`ArcSwap` 原子交换，按策略独立 cell） | ✅ 已验证（离线）——`ParamRegistry`，每策略一格 |
| 安全锁一：参数渐变 ≤ ±5%（超限跳变被硬拒） | ✅ 已验证（离线）——报告 §2.3 梯度探测 |
| 安全锁二：底层风控不可进化（`ImmutableConfig` 不可达） | ✅ 已验证（离线） |
| 审计：按策略分文件 `data/evolution/<strategy>.jsonl` | ✅ 已验证（离线） |
| `apply` / `rollback` 按策略独立 | ✅ 已验证（离线） |
| **实况长期运行** | ⚠️ **未做**——生产从未开启（`enabled=false`），**在跑会话中从未真实进化过** |

**A/B 结论**（[`SHADOW_EVOLUTION_REPORT.md`](reports/SHADOW_EVOLUTION_REPORT.md)）：
修复三处建模保真度缺陷后，整机 A/B 中**进化真实触发 1 次并带来增益**——
B 组净盈亏 +12.66、胜率 +6.6pt、回撤不变，参数单调收敛一步（cap 0.45→0.4365）。

> 读这份报告务必先看它的 **§0 相对首版的更正**：首版结论「0 触发」曾被误当作工况偶然，
> 实为三处建模缺陷（D-2/D-3）所致。

---

## 4. 市场扩展

| 扩展 | 提供 | 状态 |
| --- | --- | --- |
| `extensions/polymarket/` | CLOB 下单、行情（REST `POST /books` 轮询）、Gamma 轮盘发现、Poly1271 签名 | ✅ 实盘已验证：下单/撤单全生命周期（2026-09-20 真实落所）+ 5s 余额对账；成交（fill）捕获已修复（maker 成交上报 + 全量 user-WS 订阅），待下一笔真实成交复验 |
| `extensions/binance_spot/` | 现货行情（供趋势腿的动量过滤使用） | ✅ 已实盘验证（dry） |

**扩展契约**（`core/market_api`，**内核零市场代码**）：
`DataFeed` · `MarketDiscovery` · `OrderExecutor` · `MarketPlugin` · `MarketHost`。
新增交易所 = 写一个 crate + 用 Cargo feature 注册，**内核不改一行**。
指南：[`blitzkrieg/EXTENSION_GUIDE.md`](blitzkrieg/EXTENSION_GUIDE.md)。

**扩展配置**（`extensions/<name>/config.toml`）：`[meta]` 已被内核读取并与已链接的
扩展做漂移校验（KI-11 / §59）；`[market]`/`[risk]`/`[dependencies]` 被识别但
**无适配器读取**，启动时明确报告为 `declared_only`。**无热加载**。

---

## 5. 界面

### 5.1 Vue 面板（`ui/webapp/webui`，服务在 `/panel`）

技术栈：Vue 3 + TS + Vite + Tailwind v4 + shadcn-vue 风格本地组件 + Reka UI +
ECharts + Pinia + VueUse。设计基调：Apple 风格、金橙主调、liquid glass（数据密集页降级为实心卡片）。

| 页面 | 内容 | 状态 |
| --- | --- | --- |
| **总览** `Overview.vue` | 余额卡（dry 模拟 / live 交易所）、权益曲线、引擎统计、策略与持仓摘要 | ✅ 已实盘验证 |
| **行情面板** `HftPage.vue` | 轮次头部（倒计时 + 状态固定槽位）、筹码价格、持仓表、历史订单（分页/筛选/重置） | ✅ 已实盘验证 |
| **回放复盘** `BacktestPage.vue` | `--backtest` 报告：KPI、策略表、拒单归因、极值、风险/错误徽标 | ✅ 已验证（离线） |
| **策略** `Strategies.vue` | 按策略分账、配额、旋钮 | ✅ 已实盘验证 |
| **插件** `Plugins.vue` | 策略 / 市场插件 / 扩展 三类状态与启停 | ✅ 已实盘验证 |

**面板能力**

| 功能 | 状态 | 说明 |
| --- | --- | --- |
| 会话制鉴权（登录页 + CSPRNG 凭证 + 可吊销） | ✅ 已验证 | 参照 freqtrade 设计；`/api/snapshot` 未鉴权返回 401 |
| 凭证只来自 env（不由程序生成） | ✅ 已验证 | 环境变量提供；PR #83 |
| 主题跟随系统（深浅双主题） | ✅ 已验证 | `check:theme`（含 Safari <14 旧 API 分支） |
| 会话失效恢复 | ✅ 已验证 | `check:session`——401 是**状态**不是**错误**，自动回登录页 |
| 数字滚动动画（几乎所有数字） | ✅ 已验证 | `check:round` 33 项；实测基线误差 0.00px |
| 响应式（360→1440 八档无横向溢出） | ✅ 已验证 | 手机端内容上边距 32px |
| 行情存活检测（feed-dead 提示） | ✅ 已验证 | `check:feed` |
| 拒单归因可视化 | ✅ 已验证 | `check:rejections` + `RejectionChart.vue` |
| 余额口径（本金＋净利润、费用计支出） | ✅ 已验证 | `check:balance` 15 项 |
| 引擎启停控制 | ✅ 已验证 | `check:lifecycle` |
| 告警音效 | ✅ 已验证 | `composables/alertSounds.ts` |
| **策略/插件管理入口的完全对等（E9-g）** | 🚧 部分 | 面板已有 Plugins 页；Epic #59 要求与 TUI Plugins 页**完全对等**（列表/启停/审计/热参查看） |
| **Tauri 桌面打包的端到端验收** | ✅ 已验证 | 壳窗口指向内嵌只读 WebServer 的 `/panel/`；`desktop_snapshot` / `desktop_command` 对真 dry 内核全链路绿（`ui:webapp` 的 `cargo test --test chain`，测试自带内核） |

### 5.2 TUI（`ui/ui_kit_panel`，ratatui + crossterm）

| 功能 | 状态 |
| --- | --- |
| 4 个 tab：Overview / Positions / Trades / Plugins | ✅ 已验证（PTY 冒烟） |
| `:` 命令行、滚动日志 | ✅ 已验证 |
| 首启 self-check 清单（逐项点亮，失败给排查命令） | ✅ 已验证（E9-f / #61） |
| 底部 hint 条轮播、`?` 帮助浮层 | ✅ 已验证 |
| 命令历史（↑/↓）与补全（Tab） | ✅ 已验证 |
| 失败命令带原因 + 建议动作；`risk.kill` 全屏红条 | ✅ 已验证 |
| 内核生命周期管理（`--manage`） | ✅ 已验证 |

一键启动：`bash scripts/tui-demo.sh` / `bash scripts/tui-demo.sh --manage`；门禁 `node scripts/tui-demo-check.mjs`。

### 5.3 UI Kit（`ui/ui_kit`）

| 契约 | 状态 |
| --- | --- |
| 纯展示层 + 网关命令通道（core / web / tui / app 四层，**无交易逻辑**） | ✅ |
| **零 GUI 依赖**（不许引 tauri） | ✅ 有断言（`ui:webapp`） |
| 命令通道：`Supervisor`（spawn/stop/**adopt**，绝不重复起核）+ `Dispatcher` | ✅ `scripts/ui-kit-gateway-check.mjs` PASS |
| 崩溃上报与替换：区分「崩溃 / 主动停止」、有限预算（5 次）指数退避、面板显示崩溃横幅 | ✅ E12-c / #94，`scripts/gateway-crash-recovery-check.mjs` PASS（16 项） |
| **无任何下单 API** | ✅ 设计约束 |
| 事件推送（`EventBus` 取代纯轮询，保留轮询兜底） | ✅ E5-b / #33 |

### 5.3.1 单二进制分发与一体化启动（`blitzkrieg`，E12 / #94）

| 契约 | 状态 | 证据 |
| --- | --- | --- |
| **单二进制多命令分发**（`blitzkrieg [core|tui|web|run|--help]`） | ✅ 已验证 | `scripts/unified-launcher-check.mjs` |
| **部分启动模式**（`run` 默认 Web、`run --tui`/`-tui` 仅 TUI、`tui --attach` 只连接） | ✅ 已实现 | 统一启动器参数与共享 Dispatcher |
| **一体化默认托管**（`blitzkrieg run` 一键同时起内核与 UI，默认 `lifecycle: on`） | ✅ 已验证 | PPID 严格归属 launcher，孤儿守护 |
| **终端显式接管**（`blitzkrieg tui --attach`） | ✅ 已验证 | 仅监视现有内核，绝不杀死非本进程拉起的内核 |
| **父进程监控与零僵尸**（`SIGINT`/`SIGTERM` 级联清理，`Supervisor::stop()`） | ✅ 已验证 | SIGTERM 优雅退出后无僵尸进程、socket 自动解绑 |
| **结构性只读模式**（`--readonly` 穿透至内核） | ✅ 已验证 | 不构造出网桥梁，`mode: "readonly"` |
| **发布包体积预算**（≤ 50MB 单二进制，≤ 500MB 发布包） | ✅ 已验证 | `blitzkrieg` 1.81 MB (3.6%)，发布包 20.40 MB (4.1%) |

### 5.4 Node 层（**已删除**，见 §4 / D-24）

**此处不再有 Node 应用外壳。** 0.2 期间旧 Node 交易域与随后残留的源码层
（`src/`、`tests/`、`package.json`、`tsconfig.json`）已整体删除（`81dd253e`、
`62b16c88`）。生产栈 100% 是 Rust：`blitzkrieg-core` 引擎与 Polymarket 扩展由
`ui_kit_web` 拉起并监管，后者同时服务 Vue 面板及其 API。

Node **仅**作为验收门禁的驱动存在（`scripts/*.mjs`，**零依赖、只用 stdlib**，
根目录无需 `npm install`）。这些脚本会拉起临时隔离的内核并通过 UDS JSON-RPC 与其
通信——它们是「验证工具」，不是产品的一部分。详见
[`../scripts/README.md`](../scripts/README.md)。


---

## 6. 运维与可观测性

| 功能 | 状态 | 证据 |
| --- | --- | --- |
| 行情归档常开（分段轮转 + 磁盘护栏 + 单写者锁） | ✅ 已实盘验证 | `MIGRATION_LOG §36/§37` |
| 账本/成交/订单/持仓落盘（JSONL + SQLite） | ✅ 已实盘验证 | `data/` 下各文件 |
| 引擎统计（`engine.stats`）：按策略分账、配额占用、拒单归因、门禁计数 | ✅ 已实盘验证 | 面板策略页 |
| 巡检脚本（`scripts/soak-health.sh`，含归档新鲜度） | ✅ 已验证 | 零 token 巡检；可挂 launchd/cron |
| soak 监控（`scripts/soak-monitor.mjs`） | ✅ 已验证 | 长跑采样 |
| 微信 CI 通知（`WECHAT_WEBHOOK`，未配则安全跳过） | 🚧 未配置 | `notify (wechat)` 作业已在 CI 中 |
| **性能基准（延迟 P50 < 10ms / P99 < 100ms）** | ⚠️ **未做** | 属 0.5 里程碑，见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) §5 |
| **故障注入测试**（断线重连 / 部分成交 / 幽灵单 / 限频） | 🚧 部分 | 崩溃恢复已覆盖（订单/持仓/接管）；断线重连、限频仅部分覆盖 |

---

## 7. 仓库治理与工程基建

| 项 | 状态 | 证据 |
| --- | --- | --- |
| CI 四类门禁 + 通知（6 项检查） | ✅ 全绿 | `main` 上 6/6 success |
| 密钥扫描（工作树阻塞 + 每周全历史 + gitleaks） | ✅ | `scripts/secret-scan.sh` |
| Issue / PR 模板 + 标签体系（44 个） | ✅ | `.github/ISSUE_TEMPLATE/`、`.github/labels.yml` |
| 分支保护 / 规则集 / Push Protection | ❌ **平台限制** | GitHub Free 私有仓库不可用（D-8） |
| npm 依赖漏洞（86 个生产漏洞） | ❌ 未修（建议性） | D-9——随 D-4 下线旧 Node 模块，不做破坏性升级 |
| rustfmt / clippy 清零 | ❌ 未做（建议性） | D-6——99 文件 fmt diff / 135 clippy warning |
| 文档层（规范 / 状态 / 架构 / 历史） | ✅ | [`DEVELOPMENT.md §11`](DEVELOPMENT.md) |

---

## 8. 0.1 里程碑（E1–E7）

GitHub Milestone **v0.1 #1 已关闭**（22 个 Issue 全关）。

| 史诗 | 内容 | 状态 |
| --- | --- | --- |
| **E1** Blitzkrieg 清零 | 运行时零遗留品牌相关性（pre-takeover brand） | ✅ 完成（socket/env/协议/化妆项/remote 全部处理；加密盐与链上备注按用户裁决**有意保留**，见 D-13） |
| **E2** 多策略底座 | 按策略资金 / 门禁 opt-out / 影子进化按策略化 | ✅ 完成（#26/#27/#28） |
| **E3** 策略重构（疯狗 / HFT） | 用影子进化优化主策略 | ✅ 完成——**结论是「保持基线不改」**：F1/F3/F4 三个候选在留出段上**均不落地**（#29） |
| **E4** 对冲策略 | ≥1 趋势 + ≥1 逆向 | ✅ 完成（trend_follow #30、mean_reversion #31；两者默认禁用） |
| **E5** TUI + 插件管理器 | 完整 TUI 与插件管理 | ✅ 完成（#32/#33/#34） |
| **E6** Tauri WebUI | Tauri 完成 WebUI | 🚧 脚手架 + 最小鉴权边界已交付（#35）；端到端打包验收未做 |
| **E7** 策略接口全功能化 | C ABI v2 + 外挂标准化 | ✅ 完成（#38） |

> **E3 的「不改」是交付，不是失败。** `ROADMAP_V0_1.md §4.2` 明确写了
> 「若无稳健改进，**如实报告「不改」**——不为了交付而调参」。三项候选均被留出段证伪：
> F1 确认窗下调决策中性、F3 自适应 trailing 压回吐但毛利和降 21%、F4 子轮再入场整体毛利为负。

---

## 9. 进行中的 Epic（**开放，勿关**）

### 9.1 #57 — E8 Web 前端重构

shadcn-vue + ECharts + Pinia + VueUse 重建 Web 前端，替换旧的 `ui/hft.html` 单文件面板
（该文件已随 0.2 的遗留清理删除，见 §4）。
子任务 E8-a（骨架）/ b（快照驾驶舱）/ c（图表层）/ d（指令面 & 打包）。

**实际进度**：主体已落地（Vue 面板 5 页 + ECharts + 主题 token + liquid glass），
但作为 Epic **未正式收口**——旧 hft.html 的信息面对照清单（该文件现已不存在，
只能对照 git 历史）未逐项验收，
Tauri 打包（`frontendDist` 指 `dist/`，窗口运行时指向内嵌只读服务器的 `/panel/`）与
`desktop_snapshot` / `desktop_command` 全链路已验（`ui:webapp` 门禁 `cargo test --test chain`）。

### 9.2 #59 — E9 产品化补完

「API 必须全功能、易于开发；TUI/WebUI 必须傻子都会用。」原则：
**任何新面世面 must 全功能且有人用得起来的完整闭环 = API + 文档 + 示例 + 门禁 + UI 呈现 五件套。**

| 子项 | 内容 | 状态 |
| --- | --- | --- |
| E9-a | 策略 SDK 脚手架（一键生成 + 门禁） | ✅ 完成（#60 / PR #63） |
| E9-b | 开发回环：`reload` / `unload` 原子交换带审计 | ✅ 完成（PR #64） |
| E9-c | 调试可见性：`rejectionCauses` | ✅ 完成（PR #65） |
| E9-d | 类型安全：`SafeStrategy` trait 包装层 | ✅ **已实现**（`user_layer/strategy_api/src/safe.rs`） |
| E9-e | TUI hint 条 + `?` 帮助浮层 + 历史/补全 | ✅ 完成（E9-f / #61 同批） |
| E9-f | 错误反馈带 next-step + 首启 self-check | ✅ 完成（#61 / PR #62） |
| E9-g | WebUI 策略/插件管理与 TUI **完全对等** | 🚧 部分 |
| E9-h | 策略开发者签名页（浏览器加载本地 dylib） | 📋 **延期到 0.3**（浏览器无法直连 UDS） |

> ⚠️ **E9 的验收标准里有一条未完全达成**：`dog_strategy` 改为 `SafeStrategy` 示例后
> LOC ≤ 40。当前 `dog_strategy.rs` 仍是 **327 行手写 unsafe FFI**。
> `SafeStrategy` 已实现，但**示例尚未改写**——这是 Epic 未收口的实质证据之一。
> 详见 [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) §3.3。

### 9.3 规模化压测

| 门禁 | 目标 | 状态 |
| --- | --- | --- |
| `node scripts/scale-plugins-check.mjs` | 50 策略 × 100 插件注册表读取 < 100ms | ✅ 已有门禁 |
| `node scripts/feed-scale-check.mjs` | 40 连接 10 分钟行情推送丢包率 0 | ✅ 已有门禁 |

---

## 10. 功能缺口速查（按重要性）

| # | 缺口 | 影响 | 详见 |
| --- | --- | --- | --- |
| 1 | **Live 链路从未验证** | 所有「能赚钱」的结论都建立在 dry 之上 | [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) §1 |
| 2 | **影子进化实况从未开启** | 进化的真实收益只有离线 A/B 证据 | §3.2 |
| 3 | **E6 Tauri 端到端未验** | 桌面形态不可交付 | §3.4 |
| 4 | **性能基准未做** | 无法证明「低延迟」 | §5 |
| 5 | **E9-g / E9-h 未完成** | 面板与 TUI 能力不对等 | §3.3 |
| 6 | ~~**E12 一体化启动**~~ | ✅ 已完成（#94，单二进制 `blitzkrieg` 多命令、默认生命周期管理、零僵尸退出） | §5.3.1 |
| 7 | **fmt/clippy 债未清** | 独立专项 PR，勿夹进功能变更 | KNOWN_ISSUES KI-13 |

---

_维护者：ceer_quant · 相关（内部，`dev-docs/` 不公开）：`dev-docs/KNOWN_ISSUES.md` · `dev-docs/DECISIONS_PENDING.md` · `dev-docs/ROADMAP_V0_1.md`_
