# P0.6 — Polymarket SDK 插件化（分阶段实施）

## 目标
让内核（Blitzkrieg_core）**零市场代码**，Polymarket 成为一个独立、可注册的市场插件 crate。最终验收：`grep -ri polymarket Blitzkrieg_core/src` 只剩注释 / 一行 feature 注册。本轮只做**编译期静态链接**，运行期 dylib 热加载留到 P0.7。

## 现状（已核实）
- 内核**硬依赖** `polymarket-client-sdk-v2`（非 optional），SDK 集中在 3 个文件：`venue.rs`(约420行, 签名/CLOB/用户WS/对账)、`feed.rs`(Polly 盘口+price_change 循环)、`discovery.rs`(Gamma slug 轮询)，外加 `scanner.rs` 里的 Gamma slug/clock-offset。
- 已有抽象 `MarketAdapter`/`OrderIntent`/`RiskContext`/`LedgerApi` **全部孤儿**（零调用点）；`market/polymarket.rs::PolymarketAdapter` 从没被构造。
- `extensions/polymarket/` 是 1 行注释的 workspace 成员，能编译，**未被内核引用**。
- 决策与 TODO 已写在 `docs/blitzkrieg/MIGRATION_LOG.md §11`。

## 关键约束（决定结构）
Cargo 不允许依赖环。现 `extensions/polymarket → blitzkrieg-core`，故 `blitzkrieg-core → extensions/polymarket` 会成环。解法沿用现有 `user_layer/strategy_api` 先例：**trait 与 DTO 放进无内部依赖的新 crate**，扩展只依赖它，内核也依赖它；内核再 feature-gated 依赖扩展。依赖图：
```
market_api（新，无内部依赖）
   ↑                    ↑
blitzkrieg_core    extensions/polymarket（只依赖 market_api + SDK）
   ↑____________________/
```
因此**物理迁移是必需的**（扩展不能在编译期借用内核内部模块）——这也把"分步走"拆成：先在内核内抽出接缝并证明等价，再搬文件。

## 三个阶段

### Stage 1 — 接缝落地（行为零变化，可回滚）
1. 新建 crate `market_api/`（workspace 成员，crate 名 `blitzkrieg-market-api`）：
   - DTO（从内核**搬入并复用**，不新造）：`MarketType`、`OrderIntent`（来自 `src/order/mod.rs`）、`MarketDescriptor`（现 `CryptoMarket`，Poly 特有字段挪进 `metadata: HashMap<String,String>`）、`PendingOrder`、`MarketFill`、`VenueOrderId`、`RoundInfo`、各 `*Config`。
   - 三个专职 trait：`DataFeed`（`start(host, cfg, tokens)` 长连推送）、`MarketDiscovery`（`start(host, cfg)` 轮询注册）、`OrderExecutor`（`start(host, cfg)` 请求/响应 actor）。
   - `MarketHost`（内核实现，插件唯一出入口）：数据入口 `on_book/on_top_of_book/on_spot/on_round_markets/subscribe_tokens`；执行出口 `take_pending_orders/on_order_accepted/on_order_rejected/on_fill/on_order_live/on_order_cancelled/on_reconcile/report_error`。
   - `MarketPlugin`：打包三者（`feed()` / `discovery()` / `executor()`），作为一个注册单元。
2. 内核侧（仍留在包内，不动文件）：
   - `src/market/host.rs`：`impl MarketHost`，把上面每个方法**桥接**到 `Core` 现有方法（`engine_on_data`、`pending_unbound`、`bind_venue`/`reject_live`、`ingest_fill`、`confirm_live`、`cancel`、`reconcile`、`emit_error`）——零逻辑，纯转发。
   - `src/market/registry.rs`：`MarketPluginRegistry`（name→plugin，install/list/enable），与现有审计用 `ExtensionRegistry` 分开，避免扩大爆炸半径。
   - `src/market/polymarket_plugin.rs`（Stage 1 暂放内核）：三个 impl 分别**委托**给现有 `feed::spawn_feed` / `discovery::spawn` / `live::spawn_if_configured`。
   - `src/ipc/server.rs`：把 3 处直连调用换成"从 registry 取插件 → `start_data`/`start_executor`"。
3. IPC/Node：内核加 `market.list`（列出插件与状态）；`blitzkrieg-core-client.ts` 加 `listMarketPlugins()` + `schema.ts` zod。`extension.list/enable/disable` 已存在，保持不变。
4. **验收（硬门槛）**：`cargo build --release` + 99 tests + `tsc` 全绿；`scripts/core-parity.mjs` / `parity-engines.mjs` / `cycle-check.mjs` 通过；用 `scripts/dry-observe.mjs` 抓一段 dry-run 的 order/fill/round 序列，与重构前**逐笔一致**。行为不变才进 Stage 2。

### Stage 2 — 物理迁移（内核去 SDK）
1. `extensions/polymarket/src/`：搬入 `plugin.rs`、`venue.rs`、`feed.rs`(Polly 部分)、`discovery.rs`、`scanner_gamma.rs`（`duration_label`/`slug_for`/`parse_market_tokens`/`GammaMarketInput`/`market_from_gamma`/clock-offset），依赖改为仅 `market_api` + `polymarket-client-sdk-v2`。
2. 内核：删除 `venue.rs`、`discovery.rs`、`polymarket_plugin.rs`；`feed.rs` 只留通用的 `FeedEvent` 泵 + Binance spot（或一并下沉）；`scanner.rs` 只留**回合时序数学**（slot/`round_state`/`can_trade`/`observe_end_time`/`update_price`），slug 构造移出。
3. `Blitzkrieg_core/Cargo.toml`：**移除 `polymarket-client-sdk-v2` 依赖与 `clob/ws/data/gamma` features**；新增 optional `polymarket-extension` + `polymarket` feature；`market_api` 变为必需依赖。
4. 装配：feature-gated 注册（`#[cfg(feature="polymarket")]`）。若要求 **字面级** grep-clean，则把二进制装配挪到一个极薄的 assembly crate（bin 名仍为 `blitzkrieg-core`，Node 无需改路径）——这一点作为 Stage 2 内的一个子选项，默认先接受一行 `cfg` 注册并在文档标注。
5. **验收**：`grep -ri polymarket Blitzkrieg_core/src` → 仅注释/一行 cfg；`cargo build -p blitzkrieg-core --no-default-features` 能编（无市场代码）；回归 + 等价性重跑。

### Stage 3 — 验证与收尾
900s DRY soak 跑够覆盖多次成交间隔；更新 `MIGRATION_LOG.md`（把 §11 的 6 项 TODO 勾掉，记录新依赖图与验收证据）；`ARCHITECTURE.md`/`EXTENSION_GUIDE.md` 同步新 trait。

## 风险与对策
- **热路径**：feed/venue/discovery 刚修好。对策 = Stage 1 只做转发、并以逐笔等价为硬门槛；任一阶段异常即 git revert。
- **依赖环**：已用独立 `market_api` crate 规避（镜像 `strategy_api` 先例）。
- **构建产物路径**：不新增二进制 crate 前，产物仍在根 `target/release/blitzkrieg-core`；若引入 assembly crate 需重验 Node 的 `defaultBinaryPath()`（此前踩过旧路径的坑）。
- **round 时序**：`Scanner` 的 slot/`can_trade` 留在内核（通用数学），只把 Gamma slug 移出，避免动到正在工作的回合门控。

## 不在本轮范围
运行期 C-ABI dylib 热加载（P0.7）、`catch_unwind` 隔离、第二市场实现（Binance adapter 仅保持能编译）、Node 侧市场选择 UI。
