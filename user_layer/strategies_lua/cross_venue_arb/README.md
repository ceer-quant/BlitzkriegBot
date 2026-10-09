# cross_venue_arb

跨平台配对套利（issue #427 的「跨平台套利蓝图」），Lua 策略包（§6.5 合同）。

一句话：当同一个真实世界事件在两个 venue 上都有 confirmed mapping（`bk.unified_events()`，`status == "paired"` 且**零 discrepancy**），并且跨 venue 取腿后的

```
ask_UP(便宜腿所在 listing) + ask_DOWN(另一 listing) + 双腿 taker 费
```

比 $1.00 低超过 `spread_open_pct`，就**按各自 ask 买齐两腿**（等份额，declared shares）——配对在任何 venue 结算都值 $1.00，利润在入场时锁定。

## 蓝图参数（tunables，#393 语义：声明了 min/max 域才可被进化改写）

| 旋钮 | 默认 | 域 | 含义 |
| --- | --- | --- | --- |
| `spread_open_pct` | 2.00 | 0.10 – 20 | 「价差 > X% 开仓」：全含对价 ≤ 1 − X/100 才开 |
| `spread_close_pct` | 0.50 | 0 – 10 | 「回落 < Y% 平仓」：持仓中全含对价回到 ≥ 1 − Y/100 就出 |
| `min_leg_price` | 0.01 | 0.01 – 0.10 | 腿的 ask 下限（腐蚀报价不接） |
| `max_leg_price` | 0.99 | 0.50 – 0.995 | 腿的 ask 上限 |
| `min_pair_shares` | 1 | 1 – 100 | 容量下限：低于它不开 |
| `min_time_left_sec` | 30 | 0 – 300 | 开仓时间下限（回撤/平仓不受限） |

## 容量（issue #427：容量 = 两平台各自深度的最小值）

declared shares = `floor(min(UP 腿 ask 深度, DOWN 腿 ask 深度), 0.01)`，并在**每一笔建议的 reason 里命名**（`cap=min(depth)=…`），面板无需解析即可展示容量来源。内核仍会按自己的 max-shares 预算二次封顶。

## 费用

双腿费都走 `bk.fees()`（内核唯一费率表，`rate·(p·(1−p))^exponent`/股），**进开仓与平仓两个决策的同一套经济学**。没有费率表 → 一律不出建议（fail-closed：费是与 edge 同量级的成本参数，猜零就是造假利润）。

## 取腿与 venue-as-data

UP 腿来自 **UP ask 更便宜**的 listing，DOWN 腿来自另一条 listing。venue 身份只是数据（`venue` 字符串只进 reason 文案），包内没有任何按 venue 名分支的逻辑——`no-venue-branch-check` 门禁（#427 反向验收）可静态证明。

## 平仓

持仓中每轮重估配对全含对价：回升到 `1 − spread_close_pct` 之上（折价被重新定价掉，头寸不再值得为它的风险持有），就对两腿出 `{ token, reason }` 退出建议（seal：不带任何保留键）。内核拥有退出阶梯、成交与最终结算；退出建议只是意见，不是止损。

## 验收

`core/blitzkrieg_core/tests/cross_venue_arb_strategy_check.rs` — 11 用例：跨 venue 取腿、容量=min(深度)（含「容量永不超任一腿深度」反向行）、discrepancy/single-leg/无 mapping 拒价、无费率表拒价、折价不足拒开、单腿 stale 拒开、时间下限、一轮一 attempt、回落平仓。

```bash
cargo test -p blitzkrieg-core --test cross_venue_arb_strategy_check
```
