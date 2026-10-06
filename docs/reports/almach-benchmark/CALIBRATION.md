# 校准 A/B 报告：回测 vs @almach 实盘账本（Issue #389）

> **结论先行：5% 判据整体不过线——现金流管道轴 1:1 通过（0.0% 偏差），其余校准量如实记 fail，逐项归因，不粉饰、不 assert away。**
> 三轴对齐后：pair-only 臂 ROI 配对口径偏差 169.7%、双臂（pair+single-leg）收敛到 -31.6%（全窗口 -16.1%）；
> 期末净值偏差 98.5% → 97.3%（配对口径），受仓位规模轴的结构性残差支配，在对齐该轴之前不可能过线。
> 现金流轴 MAKER_REBATE / REWARD 偏差 0.0%（行数与金额逐分对齐）；TAKER_REBATE $3,889.99 为结构残差
> （实盘存在、语料不含，回放永远无法入账）。所有数字可从原始 JSON 工件复算（附录 A）。

## 0. 设置与产物链

| 项 | 值 |
|---|---|
| 窗口 | 2026-08-05 ~ 2026-09-30 UTC（epoch 1785888000–1790812799，与语料名一致） |
| 账本 | `data/almach/activity-full.jsonl`（240MB，/activity 全量导出，完整宇宙） |
| 语料 | `3725d52f…_1785888000_1790812799-15m-act-4bf06519abf5.jsonl`（verdict 语料，356,749 events）+ `.cashflows.jsonl` / `.redemptions.jsonl` / `.trades.jsonl` / `.manifest.json` 全套 |
| 二进制 | `bk-session/bin/core-389-base`（基线自产，基线方法论：先建 base 再改） |
| 臂 A（pair-only） | `--enable-strategy pair_discount_arb`（= #386 治理后基线形态） |
| 臂 B（pair-plus-single） | `--enable-strategy pair_discount_arb --enable-strategy single_leg_pair`（#394 引入的第二臂） |
| 判据 | 每校准量 `passAt5Pct = |偏差| ≤ 5%`，逐项显式判定 |
| 工具 | `scripts/almach-calib.mjs`（本 issue 交付，`--help` 自带说明；`--gt` 机械互检 live 侧） |

**基线锚定**：臂 A 重放结果与历史 fast6 治#2 终版数字逐位一致（closed 7,398 / netPnl $87.5595981 /
cashflows rebates 56 行 $20,806.5602 + rewards 28 行 $4,755.207）——基线无漂移，无需重建。

**live 侧互检（--gt）**：与 `almach-ground-truth.mjs` dump 全 delta = 0（fills / pairRounds / rebates /
rewards / buyUsd；pairCost 中位数差 1.7e-11 为十进制舍入尾差）。live 侧计为同一 ground truth。

## 1. 轴一：同仓位规模（分布层对齐）

内核 sizing 面是**全局**的（share band `--min-shares/--max-shares` + 单单名义上限
`--max-order-notional`），策略无 per-strategy sizing 面（报告 `sizingSource: "global"`）。因此对齐只能做在
**分布层**：live 侧从账本累计每轮名义额/份额分位数，回测侧从 tradeLines 以 overlay 精确投入基
`|netPnlUsd / netPnlPct × 100|` 重建每轮投入分布。

| 分位 | live 配对轮 $ | live 配对轮份 | live 单腿轮 $ | bt-A 每轮投入 $ | bt-B 每轮投入 $ |
|---|---|---|---|---|---|
| p10 | 8.21 | 5.99 | 0.14 | 0.93 | 0.56 |
| p25 | 19.99 | 15.00 | 2.60 | 0.955 | 0.585 |
| **p50** | **48.31** | **34.02** | **13.54** | **0.98** | **0.695** |
| p75 | 186.24 | 132.44 | 34.00 | 1.91 | 0.99 |
| p90 | 396.44 | 300.00 | 93.94 | 1.96 | 1.885 |
| p95 | 566.79 | 429.32 | 185.28 | 2.12 | 1.95 |
| p99 | 1,082.53 | 845.08 | 427.99 | 2.85 | 2.66 |
| max | 3,033.70 | 2,611.97 | 917.28 | 3.96 | 3.62 |
| mean | 144.80 | 109.06 | 39.25 | 1.37 | 0.94 |
| n | 4,389 | 4,389 | 5,391 | 2,603 | 5,121 |

（live 配对轮份额 = 较小腿份额；live 单腿轮 = 钱包自定动作空间的单腿轮。bt 份额分位：
per-close shares p50 = 1、p99 = 2、max = 3——`--max-shares 10` 名义面在 0.5 附近价位 × $2.5 预算下
实际只出 1 股。）

**对齐了什么**：两侧分布同表呈现、同口径（每轮、已投入名义额）、可逐格复算。
**残差（结构，写明）**：
1. live 每轮中位 $48.31 vs 回测 p50 $0.98（臂 A）——**每轮名义额差 ~49×**，是全局 sizing 面
   （$2.5 预算 × 10 股 lot）与钱包自身仓位决策之间的差距，无 per-strategy sizing 面可消除；
2. live 每轮跨整轮生命周期持续加单（成交 45,813 笔 / 9,780 轮 ≈ 4.7 笔/轮），回测每腿每轮一次
   入场尝试（top-of-book 深度 1.0 决定股数）——**入场次数 × 深度**两个自由度都缺；
3. 结论：sizing 轴只在分布层对齐，per-round 级残差为结构项。ROI 口径（无量纲）因此是本报告的
   主判据，期末净值口径的偏差被本轴残差支配（见 §4）。

## 2. 轴二：同动作空间（pair-only vs pair+single-leg 双臂）

live 参照系（账本自证）：9,780 触达轮 = 配对 4,389（44.9%）+ 单腿 5,391（55.1%）。pair-only 臂按定义
无法触及 55.1% 的单腿轮；`single_leg_pair`（#394）补上第二臂。

| 信号覆盖 | 臂 A（pair-only） | 臂 B（pair+single） |
|---|---|---|
| shared（回测与实盘同轮参与） | 1,229（配对口径覆盖 28.0%） | 2,252（覆盖 51.3%） |
| backtest-only（实盘未参与的轮） | 1,368 | 2,859 |
| actual-only（实盘参与、回测未触发） | 3,136 | 2,113 |
| 回测触达轮总数 | 2,603 | 5,121 |
| live 触达轮 | 9,780 | 9,780 |

**对齐了什么**：动作空间覆盖从 28.0% 提升到 51.3%，单腿轮从结构不可参与变为可参与（+1,023 配对口径
shared 轮）。
**残差（写明）**：
1. **shared 是二元事实，不按 5% 判**：实盘钱包在 46.1% 的配对轮里 pair cost 低于触发线 0.995
   （中位 1.0257）仍进场——它不挑折价，或用回测没有的信息；触发门（max_pair_cost）与信号层差异
   不是 harness 能对齐的对象，如实计数；
2. **backtest-only 轮反而增多**（1,368 → 2,859）：第二臂按自己的触发进场，实盘未参与——双向覆盖
   差距都存在，不掩盖；
3. 结论：动作空间轴达成「同动作空间」的声明（双臂 enable 且各有实绩：A 臂 5,772 closes + B 臂
   single_leg 3,515 closes），但信号选择层残差（覆盖率、bt-only 增量）为策略行为差，非结构缺口。

## 3. 轴三：同费率现金流（by type 对表）

#388 管道：语料 sidecar 携带钱包级现金流行（`conditionId` 空、带 timestamp），内核回放时入账。
报告 `cashflows` 块对账本窗口内同 type 行：

| type | live 行数 | live $ | 回测（A/B 同） $ | 偏差 | 5% 判据 |
|---|---|---|---|---|---|
| MAKER_REBATE | 56 | 20,806.56 | 20,806.5602 | **0.0%** | ✅ pass |
| REWARD | 28 | 4,755.21 | 4,755.207 | **0.0%** | ✅ pass |
| TAKER_REBATE | 56 | 3,889.99 | 0（不存在） | 结构缺口 | ❌ fail（结构） |

（任务注记修正：sidecar 实测为 MAKER 56 行 / REWARD 28 行；旧报告的 77/7 分裂是错误——77/7 是把
REWARD 行误并进 MAKER 再错分的结果，本轴以 sidecar 逐行计数为准。）

**对齐了什么**：MAKER_REBATE 与 REWARD 行数、金额逐分 1:1——#388 管道把语料携带的钱包现金流
无漂移重放。**注意口径**：这证明的是回放管道 1:1，不是策略挣到了这些钱（rebate 是做市行为回报，
回测策略满仓吃单不产生 rebate——这笔钱是 live 侧行为产生的，语料把它带上车，回放原样入账）。
**结构残差（写明）**：TAKER_REBATE 实盘存在（$3,889.99 / 56 行）但语料不含该类现金流事件——
转换器在 #388 只收编了 MAKER_REBATE + REWARD。回放**永远**无法入账这笔钱，除非重转换语料。
两臂同表同值（同一语料管道），per-arm 无差异。

## 4. ROI / 期末净值校准量（5% 判据逐项）

ROI 口径：realized / invested，两侧同规则（结算轮；live realized = REDEEM+MERGE − buy，**不含
rebate**——现金流轴单列；回测 realized = Σ netPnlUsd，**含 official 费率**——该费率不对称是声明过的
口径差，非隐藏项）。

| 校准量 | 臂 A live→bt | 偏差 | 5% | 臂 B live→bt | 偏差 | 5% |
|---|---|---|---|---|---|---|
| roiPair（配对结算轮 ROI %） | 0.907 → 2.446 | **+169.7%** | ❌ | 0.907 → 0.620 | **-31.6%** | ❌ |
| roiWhole（全轮结算 ROI %） | 3.842 → 2.448 | **-36.3%** | ❌ | 3.842 → 3.223 | **-16.1%** | ❌ |
| finalValuePair（配对口径期末净 $） | 5,762.54 → 87.56 | **98.5%** | ❌ | 5,762.54 → 155.90 | **97.3%** | ❌ |
| finalValueWhole（全轮口径期末净 $） | 32,455.41 → 87.56 | **99.7%** | ❌ | 32,455.41 → 155.90 | **99.5%** | ❌ |
| cashflowMakerRebate | 20,806.56 → 20,806.5602 | 0.0% | ✅ | 同 | 0.0% | ✅ |
| cashflowReward | 4,755.21 → 4,755.207 | 0.0% | ✅ | 同 | 0.0% | ✅ |
| cashflowTakerRebate | 3,889.99 → 0 | 结构 | ❌ | 同 | 结构 | ❌ |

（结算轮数：live 配对 4,365 / 全轮 9,045；bt-A 配对 1,229 / 全轮 2,603；bt-B 配对 2,252 / 全轮 5,121。
臂 B 双轮口径轮数覆盖过半，ROI 已从 A 臂的「幸存者切片高估」翻转为其自身的保守低估——方向翻转
本身就是动作空间对齐起效的证据。）

**归因（按支配度排序）**：
1. **期末净值偏差 ~97-99% 由轴一残差支配**（每轮名义额 ~49×），不是信号错误——ROI 口径已把规模
   除掉，偏差从 99.8%（#355 原始 A/B）降到两位数；该口径要过线必须先有 per-strategy sizing 面；
2. **ROI 配对口径 ±30-170%**：臂 A 高估（1,229 轮幸存切片，pair-trigger 挑高赔率轮），臂 B 低估
   （单腿臂接了实盘不接的轮 + 1 股 lot 的离散化 + 回测成交在 top-of-book vs 实盘跨整轮扫单）；
3. **roiWhole -16.1%（臂 B）** 是全部 ROI 校准量里离 5% 最近的，但仍不过线，如实记 fail；
4. **费率口径差**（live 账本无逐笔费行 vs 回测 official 0.07·p(1-p)，全程 fees $20-26）混入 ROI，
   方向为压低回测 ROI，量级远小于上述主因。

## 5. 判定与遗留

**5% 判据：不过线（overall fail）。** 通过项仅现金流管道 2/3（1:1）；ROI×2、期末净值×2 全部 fail，
残差逐项在案。**这不是粉饰空间**：accepted-residual 原则下，本报告把「对齐能修的」和「对齐不能修的」
分开：

- **harness/管道层已修**：verdict 结算盲区（#392 语料带 verdict）、单仓位闸门（本回放
  `--max-positions 10000 / per-asset 10000`）、动作空间半边（#394 第二臂）、钱包级现金流重放（#388）。
- **结构残差（本 issue 范围内不可修，如实列条）**：
  1. TAKER_REBATE 不在语料 → 回放永久少计 $3,889.99（需重转换语料收编该 type）；
  2. 内核无 per-strategy sizing 面 → 每轮名义额 ~49× 差距不可消除（需内核改动）；
  3. top-of-book 深度 1.0 vs 实盘跨轮扫单 → 股数分布（p50 1 股 vs live 34 股）不可消除（需 L2）；
  4. 信号层触发差异（覆盖率 51.3%、bt-only 2,859 轮）→ 策略行为差，harness 只能计量不能消除。

## 附录 A：复算路径

原始 JSON 工件（bk-agent389/，不入库）：
- `almach-calib-final.json` — 本报告唯一数字源（harness stdout，含 live 全块 + GT 互检 + 双臂全块）
- `arm-a-baseline.json` / `arm-b-dual.json` — 双臂回放报告（tradeLines 级，未截断
  tradeLinesTruncated=0，fail-closed 保证在位）
- `arm-a-baseline.log` / `arm-b-dual.log` — 回放控制台日志

逐项复算：
```bash
# live 侧（fills/pairRounds/cashflow/sizing/ROI）
node scripts/almach-calib.mjs data/almach/activity-full.jsonl 2026-08-05 2026-09-30 15m \
  pair-only=<arm-a.json> pair-plus-single=<arm-b.json> \
  --gt docs/reports/almach-benchmark/almach-gt-15m.json

# 双臂回放（cwd 钉死 bk-session）
cd /Volumes/Hard Disk/bk-session && bin/core-389-base --no-config --engine --fee-model official \
  --round-sec 900 --min-round-age 0 --min-time-left 0 --slippage-ticks 1 --seed-balance 100000 \
  --enable-strategy pair_discount_arb [--enable-strategy single_leg_pair] \
  --max-positions 10000 --max-positions-per-asset 10000 \
  --lua-strategy-dir <worktree>/user_layer/strategies_lua \
  --backtest <corpus>.jsonl --backtest-report <out>.json --backtest-fast --backtest-tick-ms 50 --backtest-tail-ms 100

# 独立实现互检（Python，账本直读）：roiPair/roiWhole 与分位数表逐位一致已验证
```

JSON 路径速查：`live.sizing.pairRoundUsd.p50`（轴一）、`arms[].deviations.signals`（轴二）、
`arms[].deviations.cashflow`（轴三）、`arms[].deviations.roiPair.relDevPct`（§4）、
`liveCrossCheck.*Delta`（互检）。
