# DRY 证据窗预注册（[实盘准入门] dry-run 证据窗 — issue #854 第一刀）

本文是 **预注册文档**：开窗前写死，开窗后不改。之言一切以本文与 `scripts/evidence-window.mjs`
代码为准；**不现场发明标准**（#854 原话：不为凑证据缩短窗口，结论不为好看而写）。

第一刀的范围 = **证据窗 runner（观测簿 + 周报归账）**。验收（go/no-go 判断）与
**开窗时点**仍归操作者；本工具只负责把证据记全。

## 1. 窗口参数（写死，不改）

| 项 | 值 | 出处 |
|---|---|---|
| 窗口时长 | **14 天** （或 ≥ 300 合格信号轮，先到先算） | #854 交付物 |
| 净亏 kill 线 | 窗口内净 PnL ≤ **-20% × seed** | #854 预设 |
| 假死 kill 线 | 连续 **100** 个合格信号轮零成交（runner 侧粗判：连续 100 个采集周期 evaluations 增长但引擎下单 0） | #854 预设，见 §4 粒度声明 |
| 采集频率 | 默认 300s 一条快照（`--interval-sec` 可调，只影响记录密度，**不影响任何 kill 线的药性**——假死线按周期数计，改周期长度即改了线的历法，须重新预注册） | 本文 |

失败即停的对口原则：#854「违门线」（ShouldNotHappen / venue 拒单率 > 50%）依赖
`engine.stats.venueRejected`，快照已原样捕获计数器；阈值判定归操作者周报复核，
runner 不重复发明违门线判定。

## 2. runner 落盘契约（零内核 diff / 零契约面）

```
data/evidence/<strategy>/snapshots.jsonl   每 cycle 一行，原始遥测照搬
data/evidence/<strategy>/weekly.jsonl      每个「已封顶」UTC 周一桶一个信封
```

快照行捕获（全部来自 core 只读 verb，**本脚本 RPC 白名单**：
`engine.stats` / `trades.summary` / `ledger.balance` / `orders.list` /
`intent.audit.tail` / `trades.history`（仅周报折算时）：

- `engine`：evaluations / signals / rounds（+ 与上周期 delta）/ books / spots /
  resolutions / placeRejected / venueRejected / tradingFrozen /
  lastError —— **字段即 stats view 原名，不发明**；
- `summary`：`trades.summary` 原样（TradeSummary camelCase）；
- `balance`：balance / available / seed（seed 缺失=live 本金不可知，净亏线无法自动判，
  告警提示操作者手判 `--seed-balance`）；
- `orders` / `liveOrders`：orders.list 计数；
- `audit`：intent.audit.tail 增量（新于水位线的记录数、按 decision.status 分桶、
  REJECTing gates 的 gate:detail 前 5 条 verbatim）；
- `deadStreak` 与本轮 kill 线警告副本。

周报信封 `summary` 字段 = **`trade_db.rs:135 TradeSummary` camelCase 逐字对齐**：

```
totalTrades / wins / losses / winRate / totalGrossPnl / totalFees /
totalNetPnl / avgHoldTimeSec / bestTradePnl / worstTradePnl / lastUpdated
```

折算算法 = `trade_db.rs fold_into`（行 234）1:1：**net ≥ 0 记 win**、winRate =
wins/total×100、avgHoldTimeSec 为运行均值、best/worst 折净额。周归属 = 交易
`exitTime` 落在哪个 UTC 周一桶（退出周，即费与净额成为既成事实的那一周）。
归账口径防漂移：周报同时携带 `crossCheck`（快照聚合净额差 vs 行级折算差、
history tail 截断标记），不一致现场可见，**不静默吞掉**。

## 3. kill 线 = 只读判定（#854 明令：不自建 freeze）

净亏线与假死线的判定结果是 **一行 stderr 告警 + 快照内 warnings 副本**，仅此而已：

- runner **不调用** `risk.kill`；**不调用** `account.status` / `account.freeze`；
  **不下单**、不退仓。`--self-test` 断言本脚本可发 RPC verb 全集 = 只读白名单。
- 冻结动作 = 操作者通过**既有** venue 冻结（#624/#626）/ 账户冻结簿（#530）
  机制人工发起。本工具与那些机制零耦合。

## 4. 粒度声明（假死线的「零成交」是粗判）

`engine.stats.signals` 的语义是**引擎下单条数**，不是被评估信号数
（`evaluate.rs:178`：`self.stats.signals += orders.len()`）。因此 runner 的
「evaluations 增长 × N 周期而订单 0」初判**粗于** #854 的「100 个合格信号轮」：

- 它不能区分「策略没给信号」与「信号全被拒」；
- **`OrderRequest.reason` 零成交分桶是另一张票**——那要动 OrderRequest 契约面
  （#840 冻结域），本票不解决，runner 只在 audit 增量里 verbatim 带出
  REJECT gates 的 gate:detail 供人工分桶阅读；
- 周报复核时用快照 `audit.byStatus` / `topRejects` 对照判读，不得把「零成交」
  一律读成「成交路径不成立」。

## 5. 本票不解决的事

- **OrderRequest.reason 零成交分桶**：另一张票（触契约面，#840 冻结域）。
- **开窗时点 / 验收 / go-no-go**：始终归操作者；runner 只交观测簿 + 周报。
- **三个策略的开跑前置**（#849 / #851 / #848 裸腿经济性）：见 #854 主表，另票追踪。
- **结算数据源**：周报数据自 `trades.history` 尾窗拉取（≤1000 行覆盖度、
  `crossCheck.truncated` 显式标记截断）；全量精确折算等 kernel 侧按周封面值
  需求出现另票。
