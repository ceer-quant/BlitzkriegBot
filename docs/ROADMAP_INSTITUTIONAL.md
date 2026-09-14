# 机构级量化系统路线图 — Blitzkrieg 能力矩阵与差距分析

- **日期**：2026-09-14
- **性质**：纯思考文档（**不含代码改动**）
- **现状口径**：DRY-only（`.env` `DRY_RUN=true`），单市场（Polymarket）、单策略（`spread_arb`）、单机
- **方法**：以机构级量化系统的标准能力集逐项对照**实际代码**，给出差距与下一步。
- **一句话结论**：项目在「**单体架构的工程质量**」上已远超同龄项目（干净的抽象分层、真实崩溃恢复、安全锁、确定性回放），但在「**多策略/多市场/组合风控/回测/密钥治理**」这些机构能力的**广度**上仍是早期。最高杠杆的三件事：**打通多策略到下单路径** → **组合级风控+资金分配** → **事件驱动回测框架**。

---

## 1. 能力矩阵（机构级系统对照）

| 能力 | 当前状态 | 差距 | 下一步（优先级） |
|:---|:---|:---|:---|
| **多市场接入** | 🟡 契约完备（`market_api`：DataFeed/Discovery/OrderExecutor/MarketHost + `MarketPluginRegistry`），但**仅实现 1 个** venue（Polymarket）；`binance_spot` 仅文档 | 无第二个可交易 venue；无多 venue **并行**（`active_market_plugin` 返回单个）；无跨 venue 标的映射 | **P-2** 落地第二个 venue 插件（Kalshi/OKX），验证契约；**P-5** 多 venue 并发 |
| **多策略并行** | 🟢 **已打通（P-1.1，`MIGRATION_LOG §34`）**：`engine.rs` 改为多策略宿主，遍历已注册/启用策略并经 `Core::place` 下单；订单/持仓/平仓全链带 `strategy` 标签；`engine.stats.strategies[]` 按策略分账（敞口 + 会话 PnL）；`--strategy-limit` 支持 per-strategy 限额（默认无限制，行为与单策略时代逐位等价，parity 硬门槛通过）。加载的用户策略默认禁用，需显式 enable | 策略晋级/金丝雀/回滚仍缺（见「策略生命周期管理」行）；无按策略资金分配（P-3） | **P-3** 策略注册表持久化 + 晋级流程；**P-3** 资金分配器 |
| **组合级风控** | 🟡 订单级 `RiskGate`（kill/价格带/名义上限）+ `LossBreaker`；持仓级 `can_open`（仓位数、日亏、冷却）。`risk_context.rs`（含杠杆/清算）**已定义未接线** | 无跨策略/跨资产聚合暴露、无 gross/net 敞口上限、无相关性/波动率调整、无 VaR/压力测试、无保证金 | **P-2** 组合风控层：敞口聚合 + 上限；**P-4** 相关性/VaR |
| **资金效率优化** | 🟡 `Ledger` 预留/释放/结算（单抵押品 USDC）；`available=balance-reserved`；size 由 `size_usd/price` 夹取 | 无闲置资金管理、无按权益/波动率缩放、无保证金/杠杆、无跨策略资金分配、预留未含费用/滑点缓冲 | **P-3** 资金分配器 + 波动率目标 sizing |
| **回测框架** | 🟡 Rust `--replay`（仅**出场策略**网格+walk-forward）；`data/shadow/near-miss.jsonl` 路径回放；Node 侧有独立 `src/trading/backtest.ts`（**与内核不通**） | **无**事件驱动、全链路（行情→信号→风控→账本→OME→出场）回测；无历史数据抽象/L2 归档；无滑点/延迟/成交概率模型 | **P-1（核心基建）** 事件驱动回测器 + 数据抽象 |
| **影子引擎** | 🟢 两套：near-miss 记录（被门禁拦截的信号+后续路径）；Shadow Evolution 虚拟变体（同 tick、虚拟账本、`catch_unwind` 隔离、ArcSwap 热切换）。**详见 `SHADOW_EVOLUTION_REPORT.md`** | 变体每回合重建导致样本难达阈值；变体等比例缩放冲淡决策差异；影子按 tick 滞后一档；影子出场用 SL50 而 live 用 SL12（保真度缺陷） | **P-1** 修保真度（定向变异/同时刻盘口/统一 ExitConfig） |
| **策略生命周期管理** | 🔴 ABI 有 name/version + 版本协商；`strategy.load/enable/list`；仅影子参数有审计+回滚 | 无策略注册持久化、无晋级/金丝雀/灰度、无策略回滚、无兼容矩阵、无健康自动降级 | **P-3** 策略注册表 + 晋级流程（paper→canary→live） |
| **低延迟基础设施** | 🟡 UDS JSON-RPC、tokio 异步、Rust-native WS feed、本地 L2 簿+新鲜度门、ArcSwap 无锁读参数 | 无任何**延迟测量**（无 Instant/histogram）；下单/决策路径共用一个 `AsyncMutex<Core>` 串行；每条消息 serde 编解码；无 CPU 绑定/内核旁路 | **P-4** 延迟埋点（P50/P99 决策与往返）；**P-6** 关键路径无锁化 |
| **高可用与灾备** | 🟢 **单机崩溃恢复很好**：订单 append-only 恢复+compact、持仓快照恢复（含 HWM/出场状态）、启动孤儿清算、单实例探测、客户端接管退避 | 无 HA/failover、无主备、无状态复制、无 leader 选举；持久化是**尽力而为**明文写，无 fsync/原子重命名/校验和/WAL | **P-2** 持久化加固（原子写+fsync+校验）；**P-6** 主备/复制 |
| **监控与告警** | 🟡 内核 `engine.stats/round/positions/orders/balance/trades` + 推送事件；`scripts/soak-monitor.mjs` 主动巡检；Node 侧 `/health`、Prometheus、alerts | `/health` 反映 Node 网关**非内核**；内核计数无 Prometheus/OTel 导出；`RiskAlert/Error` **未接入**告警通道；`dispatch_to_extensions` 从不调用 | **P-2** 内核指标导出 + 事件接告警；内置 trading `/health` |
| **密钥管理（KMS/HSM）** | 🔴 私钥从进程环境直读（`POLYMARKET_PRIVATE_KEY`）；`.env` 明文落盘；Node 另有通用凭据库 AES-256-GCM（HFT 未用） | 无 KMS/HSM/MPC、无密钥隔离/轮换/使用审计、无信封加密 | **P-5** 密钥托管（KMS）+ 轮换 + 使用审计 |
| **合规与审计** | 🟡 多份 JSONL 审计：trades/orders/positions/near-miss/**evolution**（含 from→to、reason、confidence、gradient/immutable 检查、拒绝原因、rollback）；订单带 strategy/key/asset/slot | 无防篡改（明文、无哈希链/签名/WORM）、无监管报表/对账单、无最优执行与市场滥用监控、无留存/合规保留、无「谁改了参数」访问审计 | **P-5** 哈希链审计 + 报表导出 |

图例：🟢 已具备／🟡 部分具备／🔴 缺失或名义存在。

---

## 2. 抽象层次对照（通用量化底座）

| 抽象 | 状态 | 位置 | 备注 |
|:---|:---:|:---|:---|
| 市场抽象 | ✅ 已做 | `core/market_api/src/plugin.rs` | `MarketPlugin`/`DataFeed`/`MarketDiscovery`/`OrderExecutor`/`MarketHost` |
| 订单抽象 | ✅ 已做 | `core/market_api/src/types.rs`、`core/blitzkrieg_core/src/order/mod.rs` | `OrderIntent`/`FillPolicy`/`OrderStatus`/`TimeInForce` |
| 风控抽象 | 🟡 已做未接线 | `core/blitzkrieg_core/src/risk_context.rs` | `RiskContext` trait + Prediction/Futures 上下文；live 路径未使用 |
| 账本抽象 | ✅ 已做 | `core/blitzkrieg_core/src/ledger_api.rs` | `LedgerApi`；单抵押品 |
| 策略抽象 | ✅ 已做 | `strategy_engine/mod.rs` + `user_layer/strategy_api` | `Strategy`/`Signal`/冻结 C ABI |
| 扩展抽象 | 🟡 已做未调用 | `core/blitzkrieg_core/src/extension/mod.rs` | 生命周期存在；`dispatch_to_extensions` 生产未调用 |
| **数据抽象** | ❌ **待做** | — | 无历史/参考数据源 trait；无 bar/tick/L2 归档接口；`marketdata.rs` 只是 L2 簿 |
| **回测抽象** | ❌ **待做** | — | 无 `Backtester`/模拟 trait；回放是出场专用 |
| 组合/账户抽象 | ❌ 待做 | — | 无组合/多账户/多资产资金抽象 |
| 执行算法抽象 | ❌ 待做 | — | `OrderExecutor` 仅 Polymarket 具体实现；无 SOR/执行算法层 |
| 配置抽象 | ❌ 待做（且当前为**死配置**） | `extensions/*/config.toml`、`user_layer/configs/*.toml` | 内核**不解析任何配置文件**，全走 CLI/env（见 D-1） |
| 通知/告警抽象 | ❌ 待做 | — | 内核有 `RiskAlert` 事件但无通道抽象 |
| 持久化抽象 | ❌ 待做 | `order_db`/`position_db`/`trade_db` | 具体文件包装，无 trait，无原子/校验 |

**观察**：六大抽象（市场/订单/风控/账本/策略/扩展）**已在契约层定义**——这是本项目最机构化的部分；
缺的是**数据/回测/组合**这三大抽象，以及「已定义但未接线」的风控/扩展抽象。

---

## 3. 差距分析（横切）

1. **两套分裂的注册表/路径**：`extension/` vs `market/`；`strategy_engine/` vs `engine.rs`。
   生产只走 `engine.rs`（单硬编码策略），抽象层（Strategy/Extension/RiskContext）**处于休眠**。
   → 「抽象已写好但没人用」是当前**最大的结构性债务**，也是 P-1 的根因。
   **进展（P-1.1）**：策略侧已统一——`engine.rs` 成为多策略宿主，用户策略经
   `strategies::UserStrategyAdapter` 进入同一调度与下单路径，`strategy_engine/` 退为加载器/独立注册表
   （`MIGRATION_LOG §34`）。**`extension/`（`dispatch_to_extensions` 未接线）与 `risk_context` 仍休眠**，待 P-2。
2. **DRY-only、实盘未验证**：启动孤儿清算、live executor、余额播种均已实现但**未经真实下单验证**
   （`MIGRATION_LOG §29/§30/§32`）。任何机构化路线都必须先把 live 链路跑通一遍小额验证。
3. **持久化全是明文尽力而为**：orders/positions/trades/audit 均 `std::fs` 直写、无 fsync/原子/校验。
   同时削弱 DR（§HA）与合规（§审计）。
4. **无延迟可观测性**：自称 HFT，但**零**延迟埋点；无法回答「决策耗时多少」。
5. **影子进化保真度**：见 `SHADOW_EVOLUTION_REPORT.md` §4（三处建模缺陷）。

---

## 4. 路线图（P1–P6）

> 排序原则：先补「让已有抽象真正生效」和「能验证一切的回测地基」，再扩广度，最后做 HA/密钥/合规等重资产。

### P-1 让抽象生效 + 回测地基（最高杠杆，2–4 周）
**目标**：策略抽象真正能下单；有真正的回测能验证策略。
- **交付物**
  1. 多策略执行 ✅ **已完成（P-1.1，`MIGRATION_LOG §34`）**：宿主化 `engine.rs` 遍历已注册/启用策略，
     订单带 `strategy` 标签，按策略分账 + 可选 per-strategy 限额；用户策略经适配器进入同一调度（默认禁用）。
     遗留：加载的 dylib 策略与生产同进程内的「策略晋级/回滚」属 P-3。
  2. 事件驱动回测器：新增 `Backtester` trait + 全链路重放（行情→信号→风控→账本→OME→出场），支持滑点/延迟/成交概率模型。
  3. 数据抽象：`DataSource` trait + L2/tick 归档格式（本地落盘），回测与 live 共用同一 feed 接口。
  4. 影子进化保真度修复 ✅ **已完成（`MIGRATION_LOG §33`）**：定向变异、同时刻盘口、统一 `ExitConfig`。
- **验收**：同一策略在 live 与回测上对同一历史区间给出一致 PnL（±容差）；两种策略可同时运行并各自记账
  （多策略 ✅ 已满足：`strategy.list` 可见两策略、各自 `strategies[]` 账本互不混淆；live/回测一致性待 P-1.2/1.3）。

### P-2 组合风控 + 多市场（2–3 周）
- **交付物**：组合风控层（跨资产/跨策略敞口聚合、gross/net 上限、相关性粗筛）；接线 `risk_context.rs`；落地**第二个 venue 插件**验证 `market_api` 契约；内核指标 Prometheus 导出 + `RiskAlert/Error` 接告警；持久化加固（原子写+fsync+校验和）。
- **验收**：超过组合上限的订单被拒并可解释；第二 venue 能跑通 dry 全链路。

### P-3 资金效率 + 策略生命周期（2–3 周）
- **交付物**：资金分配器（按策略/波动率分配、闲置资金策略）；波动率目标 sizing；策略注册表持久化 + 晋级流程（paper→canary→live）+ 策略级回滚；保证金/杠杆模型（若扩到合约）。
- **验收**：给定权益与波动率，sizing 自动缩放；策略可从 paper 晋到 canary 并观测。

### P-4 组合级风险量化 + 延迟可观测（2–3 周）
- **交付物**：组合 VaR/压力测试/相关性矩阵；延迟埋点（决策、IPC 往返、行情→决策 P50/P99 直方图）；异常检测与 SLO 告警。
- **验收**：能输出组合 VaR 与延迟分位；SLO 违规触发告警。

### P-5 密钥治理 + 合规审计（3–4 周）
- **交付物**：KMS/HSM 密钥托管 + 轮换 + 使用审计；哈希链/签名审计日志（trades/orders/evolution/参数变更 WORM）；对账单/监管报表导出；参数变更的访问审计。
- **验收**：审计可验证不可篡改；密钥不经明文环境；可导出合规报表。

### P-6 低延迟 + 高可用（持续）
- **交付物**：关键路径去 `AsyncMutex` 化（分片/无锁队列）、消息零拷贝、CPU 亲和；主备/状态复制/fencing；多 venue 并发执行。
- **验收**：决策路径延迟达标；主节点故障后备用接管且无重复下单。

---

## 5. 优先级排序（收敛建议）

| 序 | 事项 | 理由 |
|---|---|---|
| 1 | **多策略执行打通（P-1.1）** ✅ 已完成（`MIGRATION_LOG §34`） | 抽象已就绪却无法影响交易——投入最小、解锁最多 |
| 2 | **事件驱动回测 + 数据抽象（P-1.2/1.3）** ← 下一项 | 没有它，任何策略/参数改动都不可验证 |
| 3 | **组合风控 + 资金分配（P-2/P-3）** | 规模化的前提；单笔风控已够当前小资金 |
| 4 | **实盘小额验证（P-1 前置/并行）** | 全流程 DRY，live 链路未证；先小额打通再放量 |
| 5 | **密钥治理 + 合规审计（P-5）** | 涉及真实资金后即为刚需 |
| 6 | **HA + 延迟优化（P-6）** | 单体 + 单市场阶段收益有限，规模上来再做 |

---

## 6. 结论

- 项目的**架构底子**（市场/订单/账本/策略/扩展抽象、单机崩溃恢复、安全锁、确定性回放）已具备机构级的**形态**。
- 真正的差距在**广度与运转**：抽象层未接线（多策略/风控/扩展）、缺回测与数据抽象、缺组合/密钥/合规。
- **不做广度扩张之前，必须先把 P-1（多策略生效 + 回测地基）做完并跑通一次实盘小额验证**——
  否则后续每一层都建在无法验证的沙地上。
