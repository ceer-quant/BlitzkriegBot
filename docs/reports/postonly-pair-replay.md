# postonly_pair 保守回放

- 日期：2026-11-02
- 问题：#872
- 语料：`docs/reports/data/mean-reversion-gate/` 四个一小时窗口
- 脚本：`node scripts/postonly-pair-replay.mjs --check`

## 模型

报价是 ask 下一档。两边报价之和必须低于 0.985 才挂单。一条腿只有在之后的盘口把 ask 打到报价或更低时才算成交，而且必须还剩 20 秒以上。两条腿都打到，盈亏是 `1 - 报价之和`，maker 费按 0，不算 20% rebate。只打到一条腿，按这条腿的报价全额亏损（裸腿可以结算成 0；Chainlink TWAP 不在这里被猜成更有利的价格）。没打到的轮次不是平仓。

## 结果

| 指标 | 值 |
| --- | ---: |
| 挂单的轮 | 80 |
| 有成交的轮 | 69 |
| 配成对 | 6 |
| 裸腿 | 63 |
| 净盈亏 USD | -3.89 |
| PF | 0.037129 |
| 逆向选择 USD | 4.04 |
| 腿成交率 | 0.574514 |
| 判定 | NO-GO |

策略默认不启用。`DRY_RUN` 没有改。成交日志上的平仓仍写 `netPnlUsd`；`postonly_pair` 的 maker 平仓另外写 `adverseSelectionUsd` 和 `makerFillRate`，只读 `netPnlUsd` 的打分脚本会忽略这两个字段。
