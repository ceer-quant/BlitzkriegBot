# 趋势跟随策略留出段回放报告（E4-a / #30）

> **结论先行**：外挂通道与分账全部达标；`trend_follow` 在留出段窗口内**亏损 -1.868 USD（5 笔平仓，2 胜 3 负）**，
> 它与 `spread_arb` **不是**互相饿死——真正的耦合点是**全局连亏熔断**：任一策略亏 3 笔，全核 300s 冻结所有入场。
> 该耦合已由 0.3 里程碑条目「策略级独立风控（连亏熔断互不影响）」覆盖，**不属于 E4-a 的缺陷**。

> **证据等级（先声明，勿混用术语）**：本报告用的是**留出段（held-out segment）**，
> 不是严格意义的"样本外"（out-of-sample）。`TrendFollowConfig::default()` 的两个阈值默认值
> （`min_move_pct=3.0`、`max_spread_pct=3.0`）是对 `data/archive/` 约 18 小时全量语料标定的，
> 而本报告的切片正是这 18 小时之内的一段——所以它对**标定语料**而言是内样本。
> 它的价值在于：策略的**逐笔成交**从未被拟合过，阈值也只有两个且来自分布统计（中位数/分位数），
> 不是网格搜索出来的参数；但真正"标定语料之外"的评测需要一段不重叠的归档，
> 归入 0.5 里程碑（全量回归 + 生产级验证）。

本报告的数据取自生产 dry 核心的真实事件归档（`data/archive/`），不是合成行情。
归档切片按事件序号截取，任何人都能用下面的命令复现同一组数字。

## 1. 留出段窗口

| 项 | 值 |
|:---|:---|
| 来源段 | `data/archive/events.20260915T041830Z.jsonl`（生产 dry 核心的当日归档段，含轮转段） |
| 切片 | 该段第 548500–1741200 行 → 1,192,701 条事件 |
| 虚拟时钟 | 1789444803822 → 1789445705439（**901.6 秒**，tick 50 ms） |
| 事件构成 | `top` 1,134,068 / `spot` 54,857 / `book` 3,774 / `round` 2 |
| 完整性 | 0 条畸形行；3,776 条乱序事件（回放按时间戳排序，不影响等价性） |

两个 15 分钟的 BTC 轮次（`round` 事件），含 BTC/ETH/SOL/XRP 四资产。

### 复现命令

```bash
# 切片（165 MB，落盘位置任意；data/ 已在 .gitignore 中）
sed -n '548500,1741200p' \
  data/archive/events.20260915T041830Z.jsonl \
  > /tmp/slice-20260915T041830Z-round2.jsonl

# 三段回放共用的一组旋钮（与生产 dry 核心一致）
COMMON="--backtest /tmp/slice-20260915T041830Z-round2.jsonl --backtest-tail-ms 0 \
  --engine --round-sec 900 --min-round-age 30 --min-time-left 180 \
  --max-positions 2 --seed-balance 1000 --max-order-notional 6 --tick-ms 50 --no-event-archive"

# 腿 1：只跑 spread_arb（默认注册状态）
blitzkrieg-core $COMMON --backtest-report leg1.json

# 腿 2：只跑 trend_follow
blitzkrieg-core $COMMON --backtest-report leg2.json \
  --disable-strategy spread_arb --enable-strategy trend_follow

# 腿 3：两个都跑
blitzkrieg-core $COMMON --backtest-report leg3.json --enable-strategy trend_follow

# 腿 3b（诊断）：两个都跑，但把仓位上限放宽到 8
blitzkrieg-core $COMMON --backtest-report leg3b.json --enable-strategy trend_follow --max-positions 8
```

回放强制 dry、不写任何 trade/order/position 日志，三段结果**逐字节确定**。

## 2. 三段结果

| | 腿 1 仅 spread_arb | 腿 2 仅 trend_follow | 腿 3 两者 |
|:---|:---|:---|:---|
| 订单 | 5（2 成交 / 3 撤单） | 10（10 成交） | 10（10 成交） |
| 平仓 | 1 | 5 | 5 |
| 胜负 | 1 胜 0 负 | 2 胜 3 负（40%） | 2 胜 3 负（40%） |
| 净盈亏 | **+0.7463** | **-1.8683** | **-1.8683** |
| 手续费 | 0.1537 | 0.3683 | 0.3683 |
| 最大回撤 | 0.00 | 2.73 | 2.73 |
| spread_arb 行 | +0.7463 / 1 平仓 / 下单 3 / 拒单 0 | 0（未启用） | **0 / 0 平仓 / 下单 0 / 拒单 655** |
| trend_follow 行 | 0（未启用） | -1.8683 / 5 平仓 / 拒单 13072 | -1.8683 / 5 平仓 / 拒单 13033 |
| 风控告警 | 无 | 熔断触发 + 恢复 | 熔断触发 + 恢复 |

明细（腿 2/3 同）：XRP up `TrailingStop` +0.2321；ETH up `TrailingStop` +0.6336；
SOL up `StopLoss` -0.8781；SOL down `StopLoss` -0.7781；BTC down `StopLoss` -1.0779。

原始报告：`docs/reports/data/trend_follow_holdout_leg{1,2,3,3b}_*.json`

## 3. 读法（三个必须说清楚的判断）

### 3.1 `trend_follow` 这一段是亏的，不要粉饰

5 笔里 3 笔被止损打掉，合计 -2.73 的亏损腿大于 +0.86 的盈利腿，盈亏比 0.31。
追涨的入场价（抬价吃卖单）天然拿不到 100% 的固定止盈，靠移动止盈落袋；
在 15 分钟窗口里 BTC/ETH/SOL 的突破多数是假突破，追进去就吃回撤。
**这是策略本身的样本外表现，不是通道 bug**——同一套通道跑 `spread_arb` 是 +0.75。
它恰好说明 E4-a 要交付的东西是有意义的：新策略能被真正独立地开关、独立计账、独立进化，
才能让这样的策略在 dry 环境里先亏明白，而不是在生产里亏。

### 3.2 腿 3 里 `spread_arb` 归零，原因不是饿死

腿 3 中 `spread_arb` 的 `blockedMomentum=50 / blockedTiming=217`，说明它**照常产出了候选**，
但 655 次候选全部在下单关口被拒（`ordersRejected=655`）。腿 3b 把 `--max-positions` 从 2 放宽到 8，
结果**逐字段完全相同**——所以不是仓位容量抢占。

真正的机制在代码里可以直接指出：`Core` 上只有一个 `LossBreaker`
（`service.rs:266`），每次平仓都由 `self.breaker.record(closed.net_pnl_usd, now_ms)`
喂入（`service.rs:1418`），**不分策略**；而 `place()` 对 BUY 单首先检查
`self.breaker.is_halted(now_ms)`，命中即 `Err` 并计入 `rejected`（`service.rs:1575-1580`、`991`）。
于是 `trend_follow` 连亏 3 笔触发熔断（阈值 3 / 冷却 300s，`service.rs:226-227`），
**全核所有策略的入场一起被冻结 300 秒**，`spread_arb` 的候选就都撞在这道墙上。

结论：两个策略在**候选产出**这一层互不干扰（各自独立确认、独立出信号、各自计 `blocked*`），
共享的是**风控熔断**这一个全局闸门。这正是 0.3 里程碑
「策略级独立风控（连亏熔断互不影响）」要解的问题，E4-a 不越界改它。

### 3.3 分账是真的独立

`engine.stats.strategies[]` 在三段里都是两张独立的行：各自的 `ordersPlaced` /
`ordersRejected` / `openPositions` / `closedTrades` / `wins` / `losses` / `feesUsd` / `netPnlUsd`，
按名字归属。腿 2 与腿 3 中 `trend_follow` 的数字一致（-1.8683 / 5 平仓），
说明它的行为不被同跑的另一个策略改写；腿 1 里它的行 `enabled=false` 且全零。

## 4. 与 E4-a 验收条的对应

| 验收条 | 证据 |
|:---|:---|
| 独立可启停 | `scripts/trend-follow-check.mjs` 段 1–2（默认关、运行期双向切换、`--enable-strategy` 开机即启、互不影响） |
| 独立分账 | `scripts/trend-follow-check.mjs` 段 3；本报告 §2 的两张独立策略行 |
| 单元测试 | `cargo test -p blitzkrieg-core --lib`：12 项 trend_follow 单测（旋钮自证 / 评估器 / 孪生 / 切换后追踪器同步 / 热更新与解绑） |
| 样本外回放证据 | 本报告 §1–§3，四份原始 JSON 在 `docs/reports/data/`（**证据等级见报告开头的声明**：留出段，非标定语料之外的样本外） |
| 并发时不饿死其他策略 | `scripts/trend-follow-check.mjs` 段 4（两个资产各自入场并归属正确）；本报告 §3.2 说明样本外窗口里的耦合来自全局熔断，属 0.3 范围 |

## 5. 遗留与去向

- **全局熔断耦合**（§3.2）→ 0.3 里程碑「策略级独立风控（连亏熔断互不影响）」。
  在它落地前，多策略并跑时任何一条策略的连亏都会冻结全体入场；dry 环境可接受，上小仓位实盘前必须解决。
- **`trend_follow` 的留出段表现为负**（§3.1）→ 这正是影子进化（E2-c / 0.2）的用途：
  让 `min_move_pct` / `max_entry_price` / `max_spread_pct` 这类入场旋钮在对照实验里被逐步试出来，
  而不是靠人工拍脑袋。E4-a 只负责把策略做成"可被进化的一等公民"。
- **证据等级**（报告开头）→ 真正的样本外评测需要一段与标定语料不重叠的归档，
  归入 0.5 里程碑的「全量回归测试」与生产级验证。当前策略只有两个阈值来自分布统计，
  其余四个默认值是结构性选择（如 `break_price` 必须在 `min_confirm_price` 之下），过拟合风险低但非零。
