# 逆向/均值回归策略留出段回放报告（E4-b / #31）

> **结论先行**：外挂通道、momentum 门禁豁免与分账全部达标；`mean_reversion` 在留出段窗口内
> **亏损 -1.814 USD（3 笔平仓，0 胜 3 负，全部 StopLoss）**。它与 `spread_arb` 的候选产出层
> 不互相干扰——腿 3 的归零同样来自**全局连亏熔断**（D-18 已登记，0.3 里程碑解决）。
> 豁免机制如设计生效：6 次 spot 逆向瞬间的候选被 momentum 门禁豁免放行（`gateExemptedMomentum=6`），
> timing 门禁一次也没有被豁免。

> **证据等级**：留出段（held-out segment），非"标定语料之外"的样本外。`min_drop_pct=10`、
> `max_spread_pct=8` 两个阈值是对本切片所出处的同一语料段标定的，其余四个默认值为结构性选择
> （`max_price=0.35` 对应旧 HFT 的 cheapThreshold、`entry_factor=0.98` 与 spread_arb 共享、
> `lookback_sec=120` 取轮次时长的可观测片段、`cooldown_sec=60` 一 token 一分钟一候选）。
> 真正的样本外评测归入 0.5 里程碑（全量回归 + 生产级验证）。

本报告的数据取自生产 dry 核心的真实事件归档（`data/archive/`），不是合成行情。
与 E4-a 的报告（`TREND_FOLLOW_HOLDOUT_REPORT.md`）使用**同一切片**，数字可以直接对照。

## 1. 留出段窗口

与 E4-a 相同：`data/archive/events.20260915T041830Z.jsonl` 第 548500–1741200 行，
1,192,701 条事件，虚拟时钟 901.6 秒，两个 BTC 15 分钟轮次，BTC/ETH/SOL/XRP 四资产。

### 复现命令

```bash
sed -n '548500,1741200p' data/archive/events.20260915T041830Z.jsonl \
  > /tmp/slice-20260915T041830Z-round2.jsonl

COMMON="--backtest /tmp/slice-20260915T041830Z-round2.jsonl --backtest-tail-ms 0 \
  --engine --round-sec 900 --min-round-age 30 --min-time-left 180 \
  --max-positions 2 --seed-balance 1000 --max-order-notional 6 --tick-ms 50 --no-event-archive"

# 腿 1：只跑 spread_arb（默认注册状态）
blitzkrieg-core $COMMON --backtest-report leg1.json

# 腿 2：只跑 mean_reversion（与 E4-a 的腿 2 命令形状一致，换成逆向腿）
blitzkrieg-core $COMMON --backtest-report leg2.json \
  --disable-strategy spread_arb --enable-strategy mean_reversion

# 腿 3：两者都跑
blitzkrieg-core $COMMON --backtest-report leg3.json --enable-strategy mean_reversion
```

## 2. 三段结果

| | 腿 1 仅 spread_arb | 腿 2 仅 mean_reversion | 腿 3 两者 |
|:---|:---|:---|:---|
| 订单 | 5（2 成交 / 3 撤单） | 13（7 成交 / 6 拒单 / 3 平仓计入回收） | 14（7 成交 / 6 拒单） |
| 平仓 | 1 | 3（0 胜 3 负） | 3（0 胜 3 负） |
| 净盈亏 | **+0.7463** | **-1.8139** | **-1.8139** |
| 手续费 | 0.1537 | 0.3139 | 0.3139 |
| 最大回撤 | 0.00 | 1.81 | 1.81 |
| mean_reversion 行 | 0（未启用） | -1.8139 / 3 平仓 / 放单 7 / 拒单 3 / `gateExemptedMomentum=6` | 同腿 2（放单 6，差 1 次是熔断窗口吞掉） |
| spread_arb 行 | +0.7463 / 1 平仓 | 0（未启用） | 0 / 0 平仓 / 放单 2（全是撤单，1 平仓都没有） |
| 风控告警 | 无 | 熔断触发 1 次 | 熔断触发 1 次 |

明细（腿 2/3 同）：BTC down `StopLoss` -0.5100（-15.94%）；XRP down `StopLoss` -0.8115（-23.87%）；
XRP up `StopLoss` -0.4924（-17.59%）。三笔全部是 shared exit policy 的 12% 止损打出去的，
没有一笔走出过移动止盈的落袋路径——设计评论里"端点中位数为负、盈利依赖回弹被 trailing 抓住"的
诚实提示在这 901.6 秒内**没有兑现**：回弹没先进新高，止损先到了。

原始报告：`docs/reports/data/mean_reversion_holdout_leg{1,2,3}_*.json`

## 3. 读法

### 3.1 腿 2/3 的亏损与豁免都是"按设计运作"的

三笔全止损不是通道缺陷（同一套通道跑 spread_arb +0.75），而是策略本段的真实表现：
fade 入场价低于 mid（`entry_factor=0.98` 折价挂单），仍挡不住 12% 止损价的快速击穿。
豁免机制本身行为完全符合设计：

- `gateExemptedMomentum=6`：spot 逆向时的 6 次候选未被 momentum 门禁拦下（对照组：腿 1 里
  spread_arb 被同一门禁拦 422 次/腿 3 拦 327 次，即这条门禁在窗口内确实在认真工作）；
- `gateExemptedTiming=0`：timing 豁免从未被申请过，`blockedTiming` 该拦的照拦（腿 2 拦 12 次）；
- `blockedMomentum=0`：豁免后的 fade 候选乙方从未再需要穿越 momentum 门禁。

### 3.2 腿 3 里 spread_arb 归零：与 E4-a 的同款耦合

腿 3 中 spread_arb 仍有 327 次 momentum 拦截（照常产出候选）但放单 2、平仓 0——
和 E4-a §3.2 完全是同一个机制：`mean_reversion` 连亏 3 笔触发全局 `LossBreaker`（阈值 3 /
冷却 300s），全核入场冻结，spread_arb 的后续候选撞墙。**不是饿死**：候选产出层各算各的
（`blocked.byStrategy` 两行独立）。D-18 已在 `docs/DECISIONS_PENDING.md` 登记该耦合，
0.3 里程碑「策略级独立风控」解决，E4-b 不越界改它。

### 3.3 分账独立

`engine.stats.strategies[]` 三行独立；腿 2 与腿 3 中 `mean_reversion` 的盈亏、平仓、
手续费逐字段一致（-1.8139 / 3 平仓 / fees 0.3139），说明同跑不改变它的行为；
腿 1 中它的行 `enabled=false` 且全零。腿 3 相比腿 2 只少 1 次放单——第三笔入场被
熔断窗口直接吞掉，属于风控语义而非分账缺陷。

## 4. 与 E4-b 验收条的对应

| 验收条 | 证据 |
|:---|:---|
| 独立可启停 | `scripts/mean-reversion-check.mjs` 段 1–2（默认关、双向切换、`--enable-strategy` 开机即启） |
| 独立分账 | 同脚本段 2；本报告 §2 的独立策略行 |
| momentum 豁免声明 + 生效 | 同脚本段 5；本报告 §2（`gateExemptedMomentum=6` 带 spot 逆向、timing 从未豁免） |
| 不饿死其他策略 | 同脚本段 4（三 built-in 各吃各的 setup）；腿 3 的同跑行为与腿 2 逐字段一致 |
| 单元测试 | `cargo test -p blitzkrieg-core --lib` 210/210（含 12 项 mean_reversion 单测） |
| 影子进化可进化 | 同脚本段 6（6 个旋钮注册在先，开关不增删 cell） |
| 留出段回放证据 | 本报告 §1–§3，JSON 在 `docs/reports/data/`（证据等级见开头声明） |

## 5. 遗留与去向

- **本段亏损 -1.814**：与 trend_follow 的 -1.868 一样，是真实表现而非通道问题。
  接下来由影子进化（E2-c）在对照实验里逐步调整 `min_drop_pct` / `max_price` / `max_spread_pct`
  / `cooldown_sec` 这些入场旋钮——E4-b 只负责把逆向腿做成"可被进化的一等公民"。
- **全局熔断耦合**（§3.2）→ 0.3 里程碑，与 E4-a 同一条。
- **证据等级** → 0.5 里程碑做与标定语料不重叠的真正样本外评测。
- **ops 侧注意**：`bin` 上的 `--strategy-limit` / per-strategy sizing 未与 mean_reversion
  联调过专项场景，按 E2-a 的通用路径覆盖；若 dry 观察期发现异常再拆 issue。
