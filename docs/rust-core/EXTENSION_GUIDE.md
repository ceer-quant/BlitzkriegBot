# 扩展开发指南（EXTENSION_GUIDE）

> 内核是骨架，扩展是插件。新市场、新策略、新 UI、新数据源都以"扩展"接入，不改内核代码。
>
> **先读这一段**：内核有**两套**插件机制，别混用——
> - **市场插件**（`MarketPlugin`，本指南 §2.5）：接入一个**市场**，有下单/行情/发现能力，
>   经 `MarketHost` 与内核交互。Polymarket 就是它。要加新市场看 §2.5。
> - **通用扩展**（`Extension`，§2–§4）：只有**生命周期钩子**（`on_load`/`on_unload` + 日志），
>   **没有事件输入**，**无交易能力**。要加生命周期钩子看 §2–§4。

## 1. 扩展类型

| 类型 | 说明 | 示例 |
|:---|:---|:---|
| `market` | 新市场接入 | Polymarket（已实现）；Kalshi / OKX / Deribit（待做） |
| `strategy` | 新策略引擎 | 疯狗策略、趋势策略 |
| `ui` | 新界面 | Web UI、TUI、Telegram Bot |
| `data` | 新数据源 | CoinGecko、Glassnode |
| `risk` | 新风控规则 | 最大杠杆、相关性风控 |

## 2. 契约

```rust
#[async_trait]
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn extension_type(&self) -> ExtensionType;
    async fn on_load(&self, ctx: &dyn ExtensionContext) -> Result<(), String>;
    async fn on_unload(&self) -> Result<(), String>;
}

pub trait ExtensionContext: Send + Sync {
    fn emit(&self, event: Event);          // 建议一个内核事件
    fn strategy_names(&self) -> Vec<String>; // 只读
    fn log(&self, message: &str);
}
```

## 2.5 市场插件契约（`MarketPlugin`）

接入一个市场 = 实现 `blitzkrieg-market-api` 的三个 trait 并把它们打包成 `MarketPlugin`：

```rust
pub trait DataFeed: Send + Sync {          // 长连推送：盘口 / 现货
    fn name(&self) -> &str;
    fn start(&self, host: Arc<dyn MarketHost>, cfg: DataFeedConfig, tokens: Vec<TokenId>)
        -> BoxFuture<'_, CoreResult<()>>;
}
pub trait MarketDiscovery: Send + Sync {   // 轮询：回合/市场发现
    fn start(&self, host: Arc<dyn MarketHost>, cfg: DiscoveryConfig) -> BoxFuture<'_, CoreResult<()>>;
}
pub trait OrderExecutor: Send + Sync {     // 下单 / 撤单 / 成交回报 / 对账
    fn start(&self, host: Arc<dyn MarketHost>, cfg: ExecutorConfig) -> BoxFuture<'_, CoreResult<()>>;
}
pub trait MarketPlugin: Send + Sync {
    fn name(&self) -> &str;                // 也用于 --market-plugin <name>
    fn market_type(&self) -> MarketType;
    fn data_feed(&self)  -> Option<&dyn DataFeed>      { None }  // 任一可为空
    fn discovery(&self)  -> Option<&dyn MarketDiscovery> { None }
    fn executor(&self)   -> Option<&dyn OrderExecutor>  { None }
    fn info(&self) -> PluginInfo { ... }   // market.list 的能力报告（含 active）
}
```

**`MarketHost` 是插件唯一能碰内核的地方**：`on_book`/`on_top_of_book`/`on_spot`/
`on_round_markets`/`subscribe_tokens`（推数据），`take_pending_orders`/`on_order_accepted`/
`on_order_rejected`/`on_fill`/`on_order_live`/`on_order_cancelled`/`on_reconcile`/
`seed_balance`/`report_error`（取订单、报成交、报错）。
插件**拿不到** OME、账本、风控、私钥、socket。

**依赖方向（硬性）**：`market_api ← 内核`、`market_api ← 扩展`；扩展**不得**依赖内核
（否则成环）。参考实现：`extensions/polymarket/`。

**装配与选择**：扩展是 workspace 成员，用 Cargo feature 静态链接进内核；内核里**唯一**点名市场的
地方是 `market::register_builtin_markets()`。运行时用 `--market-plugin <name>` 选择（未注册则回退
第一个并告警）。

## 3. 生命周期

```
discovered → installed → enabled → (running) → disabled → uninstalled
```

- `install`：注册（state=Installed）。
- `enable`：调用 `on_load`；返回 `Err` → state=Failed。
- `disable`：调用 `on_unload`。
- `uninstall`：移除。

## 4. 边界（硬性）

- 扩展只能通过 `ExtensionContext` 交互（只读策略名 + 日志 + 建议意图）。
- 扩展**绝不能**获得：私钥、签名器、venue 客户端、OME、UDS socket、内核内部状态。
- 扩展是**进程内静态链接的 Rust 代码**（workspace 成员 + Cargo feature），不是动态加载，
  **没有沙箱**：`on_load`/`on_unload` 在调用方任务上、**持着 core 锁**内联执行。
  所以 panic 或卡死**会**波及内核；只有「返回 `Err`」被隔离（标记 `Failed`，内核继续跑）。

## 5. 配置

> **状态：`[meta]` 已生效（KI-11 / `MIGRATION_LOG` §59）。**
> 内核启动时读取 `extensions/<name>/config.toml` 的 `[meta]`，并与已链接的扩展做
> 漂移校验（`name` / `version` / `extension_type` 任一不符都会报告）。
> `[market]` / `[risk]` / `[dependencies]` 会被**识别但不生效**，各自带原因打印为
> `declared_only`：市场装配走 Cargo feature + `--market-plugin`、风控限额的来源是
> `RiskConfig`（第二个来源会与它静默矛盾）、依赖在构建期由 Cargo 解析。
> 文件缺失不是错误；**无热加载**——改配置需重启内核。

每个扩展一个目录 `extensions/<name>/config.toml`：

```toml
[meta]
name = "binance_spot"
version = "0.1.0"
extension_type = "market"
enabled = false
description = "Binance spot market adapter (extension-point proof)"

[dependencies]
blitzkrieg_core = ">=0.1.0"

[market]
data_endpoint = "wss://stream.binance.com:9443/ws"
symbols = ["BTCUSDT", "ETHUSDT"]
trading_enabled = false

[risk]
max_position_size = 1.0
max_daily_loss = 500.0
```

（规划中）配置需支持：热加载、依赖声明。
**已实现**：`[meta]` 解析 + 与代码的漂移校验（KI-11/§59）。
`[market]`/`[risk]`/`[dependencies]` 目前是 `declared_only`——见本节开头的说明。

## 6. IPC

| method | 说明 |
|:---|:---|
| `extension.list` | `{ version, extensions: [{ name, type, state }] }` |
| `extension.enable` | 启用通用扩展（执行 `on_load`）；返回 `{ name, enabled: true }` |
| `extension.disable` | 停用通用扩展（执行 `on_unload`）；返回 `{ name, enabled: false }` |
| `market.list` | `{ version, active, plugins: [{ name, type, hasDataFeed, hasDiscovery, hasExecutor, enabled, active }] }` |

内建示例扩展 `binance_spot`（`Extension`）默认 installed，可 `extension.enable`/`disable`。
市场插件（`MarketPlugin`）走 `market.list` 只读查询；选择用 `--market-plugin`（暂无 enable/disable IPC）。

## 7. 最小示例

- **市场插件**：`extensions/polymarket/`——`PolymarketPlugin` 组装 `feed`/`discovery`/`live` 三组件，
  经 `MarketHost` 驱动内核。这是新增市场应照抄的形状。
- **通用扩展**：`extension/builtins.rs::BinanceSpotExtension`——只在 `on_load` 写一行日志，演示
  生命周期切换与 `Err` → `Failed` 的隔离（**无交易能力**）。

## 8. 验收清单

- [x] `MarketPlugin` + 三组件 trait 定义完成，注册表可装配、可选 `active`
- [x] 至少一个真实市场插件（`extensions/polymarket`），内核零市场代码
- [x] `Extension` trait 定义完成，注册表可装配与生命周期切换
- [x] 扩展 `on_load` 返回 `Err` 不影响内核（→ state=Failed，内核继续跑）
- [ ] 扩展 panic / 卡死的隔离（沙箱或独立任务）——**未实现**，见 §4
- [x] 插件无法访问私钥/OME/socket（只能经 `MarketHost`）
- [x] 配置读取与版本校验（`config.toml` 的 `[meta]` 解析 + 与代码漂移校验）——**已实现**（KI-11/§59）
- [ ] 配置**热加载**（不重启即生效）——**未实现**；改配置需重启内核
- [ ] `[market]`/`[risk]`/`[dependencies]` 段被适配器消费——**未实现**（当前明确报告为 `declared_only`）
- [ ] 运行期 dylib 热加载（P0.7 C-ABI）——**暂缓**
