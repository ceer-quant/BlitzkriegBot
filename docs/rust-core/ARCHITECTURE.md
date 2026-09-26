# Blitzkrieg Quant Core — 架构说明（ARCHITECTURE）

> 版本：**P0.6**（市场插件化：内核零市场代码）
> 核心原则：**内核只负责规则，扩展负责玩法。市场细节不进内核。**

## 1. 分层

```
┌──────────────────────────────────────────────────────┐
│  用户层（User Layer）                                 │
│  ├─ 策略逻辑文件（user_layer/strategies/*.rs|*.toml） │
│  ├─ UI（ui/）                                         │
│  └─ 参数编排与日志渲染（src/skills, src/core runner） │
│  ⚠ 只表达"意图"，不执行"交易"                        │
└──────────────────────────────────────────────────────┘
                        │ UDS JSON-RPC（Signal / Config / Event）
                        ↓
┌──────────────────────────────────────────────────────┐
│  内核层（Blitzkrieg_core，Rust）                      │
│  ├─ strategy_engine/ 策略引擎（加载/执行用户策略）    │
│  ├─ market/          市场接缝（registry + MarketHost）│
│  ├─ order/           订单语义（OrderIntent）          │
│  ├─ ome/             订单管理引擎（市场无关）         │
│  ├─ risk/ + risk_context/  风控（市场无关）           │
│  ├─ ledger/ + ledger_api/  资金账本（市场无关）       │
│  ├─ marketdata/ signal/  行情与信号（市场无关）       │
│  └─ extension/       扩展系统（插件生命周期）         │
│  ✅ 执行一切"交易"，拥有所有敏感能力                  │
│  ⚠ 不含任何具体市场代码（无 Polymarket SDK 依赖）     │
└──────────────────────────────────────────────────────┘
                        │ 编译期链接（Cargo feature）
                        ▼
┌──────────────────────────────────────────────────────┐
│  market_api（blitzkrieg-market-api，无内部依赖）      │
│  = 市场契约：DTO + DataFeed/MarketDiscovery/          │
│    OrderExecutor + MarketHost + MarketPlugin          │
└──────────────────────────────────────────────────────┘
                        ▲
                        │ 实现
┌──────────────────────────────────────────────────────┐
│  extensions/polymarket（独立 crate）                  │
│  venue / live / feed / discovery / gamma / plugin     │
│  ⚠ 唯一持有 Polymarket SDK 与 POLYMARKET_* 的地方     │
└──────────────────────────────────────────────────────┘
```

## 2. 职责与禁止

| 组件 | 位置 | 职责 | 禁止 |
|:---|:---|:---|:---|
| 策略引擎 | 内核 | 加载/执行策略逻辑、计算信号、输出 Signal | 直接签名/下单 |
| 策略逻辑文件 | 用户层 | 定义参数、阈值、判断逻辑 | 私钥、网络、签名、订单 |
| UI | 用户层 | 渲染、参数编排、日志 | 交易执行路径 |
| OME | 内核 | 订单状态机、对账、幽灵单 | — |
| 风控/账本 | 内核 | 硬熔断、资金预扣/释放/结算 | — |

## 3. 市场插件契约（`market_api`）

市场无关的内核只认这套接口；具体市场由独立 crate 实现（Polymarket 是第一个）。

### 3.1 三个专职 trait + 门面
```rust
pub trait DataFeed: Send + Sync {          // 长连推送：盘口 / 现货
    fn name(&self) -> &str;
    fn start(&self, host: Arc<dyn MarketHost>, cfg: DataFeedConfig, tokens: Vec<TokenId>) -> BoxFuture<'_, CoreResult<()>>;
}
pub trait MarketDiscovery: Send + Sync {   // 轮询：回合发现
    fn start(&self, host: Arc<dyn MarketHost>, cfg: DiscoveryConfig) -> BoxFuture<'_, CoreResult<()>>;
}
pub trait OrderExecutor: Send + Sync {     // 请求/响应：下单/撤单/成交/对账
    fn start(&self, host: Arc<dyn MarketHost>, cfg: ExecutorConfig) -> BoxFuture<'_, CoreResult<()>>;
}
pub trait MarketPlugin: Send + Sync {      // 把三者打包为一个注册单元
    fn name(&self) -> &str;
    fn market_type(&self) -> MarketType;
    fn data_feed(&self) -> Option<&dyn DataFeed> { None }
    fn discovery(&self) -> Option<&dyn MarketDiscovery> { None }
    fn executor(&self) -> Option<&dyn OrderExecutor> { None }
    fn info(&self) -> PluginInfo { ... }   // market.list 的能力报告
}
```

### 3.2 `MarketHost`（内核唯一出入口）
插件只能通过它推数据、取订单、回报成交——**拿不到 OME/账本/风控/私钥/socket**：
`on_book` / `on_top_of_book` / `on_spot` / `on_round_markets` / `subscribe_tokens`（入站）；
`take_pending_orders` / `on_order_accepted` / `on_order_rejected` / `on_fill` / `on_order_live`
/ `on_order_cancelled` / `on_reconcile` / `seed_balance` / `report_error`（出站）。
内核实现 `market::host::CoreHost`（纯转发到 `Core`）。

### 3.3 OrderIntent（`order/`，已并入 market_api）
市场无关的订单意图：`market` / `symbol` / `side` / `order_kind` / `price` / `size` /
`time_in_force` / `metadata`。`MarketType` 覆盖 Prediction/Spot/Futures/Options。内核路径仍用
`model::OrderRequest`（含 token/condition），插件在边界做映射。

### 3.4 RiskContext / LedgerApi
`RiskContext` 与 `LedgerApi` 保留为市场无关抽象；当前交易路径直接调 `Ledger` 固有方法，
trait 作为后续多市场（含杠杆/强平）的占位。

## 4. 策略引擎（`strategies/` 宿主 + `strategy_engine/` 加载器）

**P-1.1 起：自驱动引擎是多策略宿主。** `engine::Engine` 遍历已注册且启用的策略产生候选单，
再统一执行共享闸门（回合时序、现货动量、每 token 每周期最多一单、按 `compute_shares` 定仓）与
per-strategy 限额/分账。策略标签贯穿订单（`OrderRequest.strategy`）→ 持仓 → 平仓账本，
`engine.stats` 的 `strategies[]` 按策略给出敞口与会话 PnL。

- `strategies::EngineStrategy`（**唯一**的全功能宿主契约，E7/ABI v2 起内树与外挂共用）：
  `on_book`/`on_round`/`find_candidates`/`take_exit_intents`/`take_breaks`/
  `confirmed_tokens`/`diagnostics`/`set_hot_params`/`config_view_json`/`on_config`；只读视图
  `StrategyCtx`（markets、回合剩余、`fresh_book`）——**策略无法绕过宿主闸门**
  （sizing/风控/限额都在宿主与 Core 侧；入场不带张数、出场不带价格）。
- 内核**自身不注册任何策略**：`strategies/` 下只剩契约（`mod.rs`）、影子孪生契约
  （`shadow_twin.rs`）、外挂适配（`foreign.rs`）与 `cfg(test)` 适配器（`test_support.rs`）。
  出厂状态下 `strategy.list` 为空，`Engine` 因此不产生任何候选单。
- `strategies::foreign::ForeignStrategy`（feature `strategy-loading`，**默认开启**）：把 dlopen 来的
  C ABI **v2** 策略适配为同一个 `EngineStrategy`——全档位盘口、逐回调 `on_book`、出场意图、热参、
  诊断全部过界；**注册后默认 `enabled=false`**，需显式 `strategy.enable` 才开始交易。
- 单策略注册表：`Engine::supported_strategies()`（注册序）/ `enabled_strategies()` / `set_strategy_enabled()` /
  `register_user_strategy()` / `strategy_source()`。`load_strategy_lib` 协商通过后把动态库策略
  **注册进引擎调度（disabled）**；引擎未挂载时返回失败。

`strategy_engine/loader.rs` 是**外挂策略的加载/协商器**（E7 重写；旧的精简 trait/独立注册表已删除）：

- 协商顺序：`policy_allows`（拒绝凭据样式文件名 `.env/private/secret/key/credential` 与非
  `.so/.dylib/.dll`，**在 dlopen 之前**）→ dlopen → 强制 `bk_strategy_abi_version()==2`
  （无 v1 兼容层，D-15）→ vtable abi/min + 必需钩子（create/destroy/on_book/on_round/evaluate）
  校验 → `create()` → `ForeignStrategy`。
- 结构化出参（entries/exits/breaks/confirmed/diagnostics/knobs）一律是**本库分配的堆 JSON**，
  内核复制后经同一库的 `bk_strategy_free_string` 归还，分配器不跨边界。
- 外挂边界只传只读借用视图与意图数据：策略拿不到凭证/下单管理器/UDS/网络，无法绕过
  RiskGate/ImmutableConfig/kill switch。出场意图经 `ExitReason::StrategySignal` 与策略出场合流到
  同一条提交路径（见 `STRATEGY_GUIDE.md`、`ABI_V2_DESIGN.md`）。

## 5. 两套"插件"系统（务必区分）

内核里有**两个互不相干**的注册表，名字都像"扩展"，职责完全不同：

| | `extension/`（`ExtensionRegistry`） | `market/`（`MarketPluginRegistry`） |
|:---|:---|:---|
| 目的 | 生命周期钩子（无事件输入） | 接入一个**市场**（下单+行情+发现） |
| 契约 | `Extension`（`on_load`/`on_unload`） | `MarketPlugin`（`DataFeed`/`MarketDiscovery`/`OrderExecutor`） |
| 能力 | 只能 `emit`/`log`/读策略名——**无交易能力** | 经 `MarketHost` 推行情、取订单、报成交 |
| IPC | `extension.list/enable/disable` | `market.list`（含 `active`） |
| 现存 | 内建示例 `BinanceSpotExtension`（installed，默认未启用） | 官方 `polymarket`（默认编译并 active） |

`Extension` 是**生命周期钩子**，**刻意不给交易能力**；`MarketPlugin` 才是"市场接入点"。
新增一个市场 = 实现 `MarketPlugin` 并加一个 Cargo feature；新增一个生命周期钩子 = 实现 `Extension`。

> `extensions/<name>/config.toml` 的 `[meta]` **已被内核读取并与已链接扩展校验**
> （KI-11，见 `EXTENSION_GUIDE.md §5`）；`[market]`/`[risk]`/`[dependencies]`
> 被识别但**无适配器读取**，启动时明确报告为「已声明未生效」。
> 装配仍走 Cargo feature，选择仍走 `--market-plugin <name>`，无热加载。

## 6. 通用化边界（不做的事）

- 内核不得依赖 Node 侧代码。
- **内核不得有具体市场代码**：Polymarket 细节全在 `extensions/polymarket/`；内核里唯一的
  名字出现在 `market::register_builtin_markets()` 的 `#[cfg(feature="polymarket")]` 注册行。
- 内核 Cargo 不含 `polymarket-client-sdk-v2`；扩展不依赖 `blitzkrieg-core`（无依赖环）。
- 扩展不得直接访问内核内部状态或私钥（只能经 `MarketHost`）。

## 7. 与既有 P0–P5 的关系

P0.6 **不改变交易语义**：`ome / ledger / risk / position / exit_policy / marketdata / signal /
engine / scanner（回合时序）/ reconcile / shadow` 全部保持原实现与行为；Polymarket 的
venue/feed/discovery/gamma 从内核**物理迁出**到扩展，行为逐笔等价（见 `MIGRATION_LOG.md §19`）。

## 7.5 裁决流水线（E25 / #331，DEV_V0_3 §3）

策略建议、内核裁决：策略是 0 信任组件，只提交建议；`arbitration/` 的
`process_intent` 对每条建议跑四道关卡，产出唯一 `Decision`：

| 关卡 | 复用的既有实现（不重写） |
|:--|:--|
| Gate 1 合法性 | `market_api::OrderIntent::validate()` + 本轮 token 检查 |
| Gate 2 系统风控 | `RiskGate::check_with_equity` + 既有 `LossBreakers`（平仓 intent 豁免——既有语义） |
| Gate 3 资金预扣 | `Ledger::reserve` 探针（reserve→release，复用既有拒绝逻辑，账本净零） |
| Gate 4 生存绑定 | `exit_policy::effective_stop_pct` 等的**投影**（只记录，不加触发路径） |

三条纪律，违反任何一条的 PR 拒绝合入：

1. **不重复判定**——每道关卡只调用既有实现，绝不复制阈值；风控的真相只有
   `RiskGate` 里那一份。
2. **无短路**——被拒的建议同样逐条落审计（`data/audit/intents.jsonl`，每条
   一行；审计写失败只 `warn!` 一次，绝不阻塞交易）。
3. **审计是旁路**——`--no-intent-audit` 只关审计**写入**，关卡永远在跑；
   有/无审计两种情形都必须通过既有经济基线（P1）。

`Rejected` 的建议不进入 `place()`/OME；`Approved` 的建议交给**既有**提交路径。
`INTENT_DECISION` 推送按 `(strategy, gate, reason)` 折叠到 ≤1 条/秒（折叠的是
推送，不是审计）；面板的「裁决流」（WebUI `Decisions.vue` / TUI Decisions tab）
逐字打印内核写的 `GateTrace.detail`，不自行编文案。

## 8. 目录

```
market_api/               # 市场契约（无内部依赖；内核与扩展共享）
Blitzkrieg_core/          # Rust 内核（零市场代码）
├── src/
│   ├── strategies/       # 多策略宿主契约（内核 0 策略：契约 + 外挂适配 + test 适配器）
│   ├── strategy_engine/  # 用户策略加载/校验/独立注册表（mod/loader）
│   ├── market/           # 接缝：mod（选择/注册）/ host / registry
│   ├── order/            # 订单语义（re-export market_api）
│   ├── extension/        # 扩展系统（生命周期/审计，与 market 插件分离）
│   ├── scanner.rs        # 回合时序数学（市场无关）
│   ├── ledger_api.rs risk_context.rs
│   └── （既有 P0–P5 模块）
extensions/polymarket/    # 官方 Polymarket 市场插件（独立 crate：rlib+cdylib）
├── src/ venue.rs live.rs feed.rs discovery.rs gamma.rs plugin.rs
ui/                       # Rust 呈现层，分三个 crate：
├── ui_kit/               #   纯展示层 + 网关（ui_kit_web 服务面板与 /api）；无交易逻辑
├── ui_kit_panel/         #   终端面板（TUI）
└── webapp/               #   Tauri 桌面壳（独立 workspace）+ webui/（Vue 3 + Vite 前端源码）
user_layer/               # 用户层策略逻辑（strategy_api / parity_* / strategies）与配置
scripts/                  # 门禁与运维脚本（裸 Node，零 npm 依赖）
docs/rust-core/           # 本文档集
```
