# 72 小时启用集：扣费与 1 tick 滑点后的证据

- 日期：2026-11-02
- 问题：#865
- 语料：`docs/reports/data/mean-reversion-gate/` 四个一小时窗口（sha256 与 `scripts/lib/frozen-corpus.mjs` 一致）
- 策略版本：pair_discount_arb 1.3.0，single_leg_pair 1.1.0
- 脚本：`node scripts/cost-adjusted-edge.mjs --check`

## 结论

**NO-GO。** 启用集为空。没有任何当前策略在「非乐观成交假设 + 手续费 + 1 tick」下被证明 PF ≥ 1.1。

这不是内核 `--backtest` 的逐笔回放。被删的 `scripts/backtest-check.mjs` 回放的是已经不在树里的 cdylib。本报告只给冻结窗口上的成本模型，并拒绝把乐观的「两条腿都在报价上成交」写成已实现优势。窗口里没有成交打印，测不到 maker 成交率，也测不到裸腿归零。

| 策略 | 版本 | 判定 |
| --- | --- | --- |
| `pair_discount_arb` | 1.3.0 | NO-GO |
| `single_leg_pair` | 1.1.0 | NO-GO |
| `lua_momentum` | 0.1.0 | NO-GO |
| `spread_arb` | 1.0.0 | NO-GO |
| `mad_dog` | 1.0.0 | NO-GO |
| `market_maker` | 1.1.0 | NO-GO |

## 每个二元轮的合并盈亏（1 手，合并拿回 $1）

样本：80 个 (condition, roundSlot)。两边 ask 都在，且时间戳相差不超过 3500 ms。每个轮取 ask 之和的中位数那一对。滑点把买价推离中间：taker 加 1 tick，maker 报价减 1 tick。超出 0.01–0.99 网格的一轮记为不可成交，不记成亏损。

| 模型 | 成交轮 | 胜 / 负 | 净盈亏 USD | PF | 最大回撤 USD | 能否单独 GO |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| `taker_official_slip1` | 75 | 0 / 75 | -4.22766 | 0 | 4.22766 | 否 |
| `taker_legacy_slip1` | 75 | 0 / 75 | -3.070599 | 0 | 3.070599 | 否 |
| `maker_at_ask` | 80 | 0 / 80 | -0.99 | 0 | 0.99 | 否 |
| `maker_quote_minus_1` | 80 | 61 / 0 | 0.61 | inf | 0 | 否 |

`passesGate` 只表示该模型自己达到了 PF ≥ 1.1、回撤 ≤ $5、样本 ≥ 30，且模型不是乐观成交。它不会把策略放进启用集。maker 两行被标成乐观，门禁直接否决。

## 为什么不启用 pair_discount_arb 1.3.0

v1.3.0 是 entry_maker，报价在 ask 之下。这些窗口没有成交打印，测不到 maker 成交率，也测不到裸腿归零。taker_official_slip1 是一条当前版本根本不走的路径的成本上限。docs/strategy-review-0.4.1.md 的 PF 早于 v1.3。策略自己的折扣测试看的是 ask 之和（再加费）是否低于 0.985；本语料的命中轮数见文末，中位书上并不开火。

## 为什么不启用 single_leg_pair 1.1.0

方向性单腿要有结算或 resolution 才能打分。这些窗口既没有 resolution，也没有 spot。0.4.1 的 PF 不是 1.1.0 在这份语料上的重跑。

## 分窗口样本数

| 窗口 | 轮数 |
| --- | ---: |
| `trend-20260919T1000Z` | 20 |
| `range-20260919T1600Z` | 20 |
| `trend-20260920T2100Z` | 20 |
| `range-20260920T2300Z` | 20 |

折扣测试（两边 ask 之和 < 0.985，尚未加费）命中的轮数：0。
