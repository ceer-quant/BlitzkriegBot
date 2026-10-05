# @almach 失真还原复测报告（#355 后续 · 2026-10-05）

> **结论先行：失真 #1（引擎每资产单仓位）已移除，信号覆盖 15m 1.2% → 28%、5m 4.1% → 24.3%；
> 回放吞吐衰减根治（15m ~11.5K → 216K events/min，且不再随订单量衰减）。
> 但偏差判定仍未过线**（配对口径终值偏差 15m 96.7% / 5m 99.3%）：#2 dry 结算盲区升格为在场最大失真
> （15m 期末 **218 条腿**卡死在未结算仓位里，其链上决议价值既未变现也未计损），
> 其余失真点（#3 单腿轮、#5 回扣、#6 深度、#7 费率口径）原样在场。以下如实归因，不粉饰。

## 0. 本次复测的两个前置修复（PR #374 / #375，均已人工审核合并）

| PR | 内容 | 对还原的作用 |
|---|---|---|
| #374 | OME per-token live index（`live_for` O(live-for-token)） | 消除事件路径上随订单量线性衰减的索引扫描 |
| #375 | `--backtest-fast`：门控冻结盘口下可证明冗余的维护工作（quiet-tick Lua、holder 阶梯、脏 token 估值、settlement/escalation 扫描、kline 预览） | quiet tick 从全量估值+Lua（~ms 级）降到 O(1)，且**每个时间戳与反应瞬间与历史路径逐位一致** |

`--backtest-fast` 默认 OFF = 历史行为逐字节不变（同一构建 A/B，5m 语料 `tradesSha256 a6ae6970…` 双向命中）。
实现过程中顺带修掉两个语义缺陷并全部复验：`all_strategies_hold_to_settlement` 曾把**禁用**策略计入场
（7/9 个 Lua 库默认非 holder ⇒ 闸门全程失效），现只计启用策略；expiry 唤醒曾取"最早到期（含已过期）"，
最早到期一过门即恒开（实测占 tick 95% 的全量仓位克隆），现只看仍处于未来的最早到期。

## 1. 复现命令（与 #355 报告附录的旧命令有两处关键差异）

```bash
cd /tmp   # 关键①：execution_policy.toml 走 cwd 相对路径，仓库 cwd 下会静默加载 user_layer 规则
target/release/blitzkrieg-core \
  --no-config --engine --fee-model official \
  --round-sec 900 --min-round-age 0 --min-time-left 0 \
  --slippage-ticks 1 --seed-balance 100000 \
  --enable-strategy pair_discount_arb \
  --max-positions 10000 --max-positions-per-asset 10000 \
  --lua-strategy-dir "/…/user_layer/strategies_lua" \
  --backtest "/…/data/onchain/…-15m-act-4bf06519abf5.jsonl" \
  --backtest-report out.json --backtest-tick-ms 50 --backtest-tail-ms 100 \
  --backtest-fast
```

- **关键②**：#355 旧命令只放开了总仓位闸（`--max-positions 5000`），**没放开 per-asset 闸**
  （内核默认 `--max-positions-per-asset 1`）——这正是旧报告失真 #1 的直接来源；
  本次两闸同放（10000 = 回放窗口内事实不设限）。
- 5m 版仅 `--round-sec 300` 与语料不同。
- 产物：`/tmp/bp-session/almach-{5m-fast6,15m-fast3}.json`（SHA 锚定：5m tradesSha256 `a6ae6970…`，
  与不开 `--backtest-fast` 的同构建 A/B 逐位一致；15m `16ab61c0…`）。

## 2. 新旧对比（同一 ground truth：`activity-full.jsonl`，2026-08-05 ~ 09-30）

### 15m（实盘配对轮 4,389）

| 指标 | 旧（#355 报告） | 新（本次） | 变化 |
|---|---|---|---|
| 回测成交（closes） | 322 | **7,259** | 22.5× |
| 回测参与轮次 | 123 | **2,613** | 21.2× |
| 信号覆盖（shared/actualPairs） | 54 = **1.2%** | 1,231 = **28%** | **23×** |
| 回测净额 / PF | $9.03 / 1.32 | **$192.06** / 1.286 | 21× |
| 回测 ROI（期末） | 13.927%（幸存者切片） | **11.523%**（广覆盖） | 量级同段 |
| 配对口径终值偏差 | 99.8% | **96.7%** | 仍不过线 |
| maxAbsDiff | $6,654.65 | $6,473.53 | — |
| 回放吞吐 | ~11.5K ev/min 且衰减 | **216,126 ev/min**（96.8s 全程） | 19× 且恒定 |

### 5m（实盘配对轮 10,934）

| 指标 | 旧（#355 报告） | 新（本次） | 变化 |
|---|---|---|---|
| 回测成交（closes） | 4,211 | **16,471** | 3.9× |
| 回测参与轮次 | 1,776 | **6,843** | 3.9× |
| 信号覆盖 | 446 = **4.1%** | 2,659 = **24.3%** | **6×** |
| 回测净额 / PF | $134.76 / 1.37 | **$481.93** / 1.323 | 3.6× |
| 回测 ROI | 26.885%（幸存者切片） | **15.905%**（广覆盖） | 收窄 |
| 配对口径终值偏差 | 99.8% | **99.3%** | 仍不过线 |
| maxAbsDiff | $71,185.68 | $70,840.70 | — |
| 回放吞吐 | （同管线） | **370,863 ev/min**（77.4s） | — |

**结构性改善**：旧 15m 回测被单仓位闸锁进 123 轮的"小而精"幸存者切片（ROI 虚高 13.9%、曲线形状与实盘不可比）；
新回测覆盖 2,613 轮 / 60% 配对轮宇宙，ROI 与旧切片同段（11.5%）但**曲线形状首次可与实盘叠加比较**
（overlay `backtestOutsideLedger: 0`——回测触达的每一轮都在实盘账本内，无幻影信号）。

**仍未过线的部分**：绝对口径差依旧（seed $100k、每单 $2.5 vs 实盘钱包；ROI 15m 11.5% vs 0.907% ≈ 12.7×，
5m 15.9% vs 2.583% ≈ 6.2×）——旧报告"回测侧系统性高估"的判定方向不变，且 #2（下节）解释了其中
未结算尾巴的份额。

## 3. 更新后的失真清单（按影响重排）

| # | 失真点 | 旧状态 → 新状态 | 证据（本次） |
|---|---|---|---|
| ~~1~~ | ~~引擎每资产单仓位~~ | **已移除**（两闸同放） | `positions.already_in` 拒单 5m 10,140 / 15m 4,988 → **0**（15m 剩 8 次全为 risk:breaker/exitCooldown） |
| **2** | **dry 结算盲区**（原 #2，升格第一） | **原样在场，影响放大** | 两语料各报 `no market verdict since …` 盲区错误各 1；**15m 期末 218 条开放腿（$136.28 名义）卡死**——出价消失轮次的链上 verdict 拿不到，$1/pair 的无风险收益既不实现也不计损，直接压住终值偏差分母 |
| 3 | 单腿轮在策略动作空间之外 | 不变（市场结构） | 5m 61.1% / 15m 55.1%（GT 侧，未重算） |
| 4 | 信号覆盖缺口 | 1.2%/4.1% → **28%/24.3%**，残余 = 触发线 0.995 + pair 成本上限 + 单腿轮复合 | shared 1,231 / 2,659 |
| 5 | rebate/reward 未建模 | 不变 | 实盘 MAKER_REBATE $24,696 + REWARD $4,755，回测无此现金流 |
| 6 | top-of-book 名义 1.0 深度 vs 真实 L2 | 不变（数字更新） | partialRate 5m 17.8% / 15m 32.7%；sizeFillRatio 84.3% / 73.9% |
| 7 | 费率口径 | 不变（声明） | official 0.07·p(1−p)，maker 0；feesUsd 5m $38.65 / 15m $24.69（旧 15m 仅 $0.96 是切片过小的镜像） |

## 4. 后续项（更新优先级）

1. **转换器写入链上结算 verdict（conditionId → winner）**——治 #2，现在是第一优先：
   15m 218 条卡死腿的决议直接决定终值偏差能否进入 <20% 区间；
2. per-asset 仓位闸可配置化——#1 的临时解是 CLI 大数，配置面收口仍未做；
3. rebate/reward 现金流建模（治 #5）；
4. L2 深度回放（治 #6，依赖数据源）。

## 附录：验证链

- 位精确：同一构建（`22cb568e` 变基后）5m A/B（±`--backtest-fast`）tradesSha256 `a6ae6970…` 双向一致，
  16,471 closes；变基到 #374 之后再次复验（`fast6`）。
- 吞吐测量均为 solo 进程（两个重放并发会互相拖慢 ~1.78×，测速率必须独占）。
- 全部运行参数与坑（`--max-positions` 内核默认 2、cwd→policy 文件陷阱等）存于会话记忆
  `almach-baseline-replay-commands`。
- Overlay 原始 JSON：`/tmp/bp-session/overlay-{5m-fast6,15m-fast3}.json`（不入库，命令可复现）。
