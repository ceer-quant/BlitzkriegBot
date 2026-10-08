# OPT-v2 — 策略校准寻优 round 2（v2 语料 / 宽网格 / 组合研究）

**Issue**: #404 · **分支**: `feat/strat-opt-v2`（基于 41e54cfc）· **方法**: train/val 切分防过拟合，域内（#401 min/max）旋钮网格

## 0. 语料与切分

- v2 语料：`data/onchain/3725d52f…-15m-act-4bf06519abf5.jsonl`（356,749 events，converterVersion 2，1785888000–1790812799），只读。
- train（前 62.5%，`at < 1788480000000` = 2026-09-03T00:00Z）：222,945 events；val（后 37.5%）：133,804 events。逐行无遗漏（222,945+133,804=356,749）。
- 锚点验证：pair_discount_arb 单开全量 v2 fast 逐位复现 closed 7,398 / WR 53.79832387131657% / PF 1.1126420904411336 / net $87.5595981 / gp $864.885354015 / gl $777.325755915（base 二进制 sha256 前16 `7bbee0cfb6ee2715`）。

## 1. 普查表（各自单开，全量 v2，default 旋钮）

| 策略 | closed | WR % | PF | netPnl $ | grossProfit $ | grossLoss $ | 结论 |
|---|---:|---:|---:|---:|---:|---:|---|
| pair_discount_arb | 7,398 | 53.79832387131657 | 1.1126420904411336 | 87.5595981 | 864.885354015 | 777.325755915 | 锚点复现 |
| single_leg_pair | 3,538 | 63.736574335782926 | 1.1559570962809507 | 119.9116446675 | 888.78749275 | 768.8758480825 | 有信号 |
| spread_arb | 0 | — | — | 0 | 0 | 0 | 结构性零信号（见 §5） |
| lua_momentum | 0 | — | — | 0 | 0 | 0 | 结构性零信号（见 §5） |
| oracle_ruler | 13,971 | 5.203636103356954 | 0.24588157860005808 | -3,672.0989012825 | 1,197.2940177575 | 4,869.39291904 | 校准探针，设计性负 PF |
| 零信号五件套（flash_arb / hot_side_momentum / kline_probe / mad_dog / market_maker） | 0 | — | — | 0 | 0 | 0 | 确认 0（缺 spot/kline 事件） |

## 2. 寻优表（train 66 点网格 → val 复核 → 全量 A/B 终审）

网格规模：pair 23 点 / single_leg 23 点 / oracle 8 点 / spread 7 点（逃生门探针）/ momentum 5 点（阈值域）。全部在 #401 声明的 min/max 域内。

### 2.1 single_leg_pair —— 采纳 `min_gap: 0.02→0.10`、`min_entry_price: 0.55→0.60`

| 切片 | closed | WR % | PF | PF Δ% | net $ | net Δ% | WR Δpp |
|---|---:|---:|---:|---:|---:|---:|---:|
| train 基线 | 2,233 | 64.5320 | 1.198910 | — | 93.9440 | — | — |
| train combo | 2,363 | 72.2810 | 1.316275 | **+9.79** | 137.1416 | **+45.98** | +7.75 |
| val 基线 | 1,305 | 62.3755 | 1.087556 | — | 25.9677 | — | — |
| val combo | 1,317 | 68.5649 | 1.127922 | **+3.71** | 35.0645 | **+35.03** | +6.19 |
| 全量基线 | 3,538 | 63.7366 | 1.155957 | — | 119.9116 | — | — |
| 全量 combo | 3,681 | 70.9590 | 1.243769 | **+7.60** | 172.5210 | **+43.87** | +7.22 |

三切片全同向。单旋钮臂（全量）：min_gap=0.10 → PF +1.68% / net +73.12%；min_entry_price=0.60 → PF +12.24% / net -20.63%；combo 取中庸且 WR 最高。

### 2.2 pair_discount_arb —— 采纳 `max_pair_cost: 0.995→0.985`（PF 路径）

| 切片 | closed | WR % | PF | PF Δ% | net $ | net Δ% | WR Δpp |
|---|---:|---:|---:|---:|---:|---:|---:|
| train | 2,982 | 55.1979 | 1.194142 | **+4.92** | 55.0166 | -12.60 | +1.33 |
| val | 1,946 | 54.9332 | 1.123191 | **+4.33** | 22.6132 | -8.13 | +1.24 |
| 全量 | 4,928 | 55.0933 | 1.166250 | **+4.82** | 77.6298 | -11.34 | +1.30 |

PF 路径达标（≥+3%，三切片同向，WR 回撤为 0——反升）。次优臂 `max_pair_cost=0.99`（全量 PF +3.55%，net -1.43%）未采纳：改善更弱，且两臂择优即可。manifest default 变更后单开复跑与 knob 臂逐位一致（mverify-pair IDENTICAL）。

### 2.3 oracle_ruler —— 无诚实改进

PF 路径全部伴随 net 大幅恶化：entry_price=0.50（train PF +43.4% 但 net -70.3%）、0.55（PF +38.7%，net -88.2%）。该策略是测量层校准探针（每个 round 无条件挂 bid），其 PF 由 entry_price 与真实结算赔率的错配决定，调参只会改变烧钱曲线形状。**default 不动。**

### 2.4 spread_arb / lua_momentum —— 无诚实改进（结构性零信号）

网格（含 trend_entry_price 固定价逃生门、threshold 全域 0.005–1.0）全部 0 orders。根因见 §5：数据层 mid=0 使两策略的核心状态机永不推进，旋钮不可修。

## 3. 组合表（全量 v2，实测非假设）

| 组合 | closed | WR % | PF | netPnl $ |
|---|---:|---:|---:|---:|
| solo pair（default） | 7,398 | 53.7983 | 1.112642 | 87.5596 |
| solo single_leg（default） | 3,538 | 63.7366 | 1.155957 | 119.9116 |
| solo spread_arb | 0 | — | — | 0 |
| solo oracle_ruler | 13,971 | 5.2036 | 0.245882 | -3,672.0989 |
| solo lua_momentum | 0 | — | — | 0 |
| **pair+single_leg（采纳旋钮）** | **7,481** | **59.4439** | **1.237381** | **238.7432** |
| +spread_arb | 7,481 | 59.4439 | 1.237381 | 238.7432（无变化：0 信号） |
| +oracle_ruler | 17,221 | 17.0896 | 0.321414 | -3,548.6111（被 oracle 烧穿） |
| 全部 5 有信号 | 17,221 | 17.0896 | 0.321414 | -3,548.6111 |

消融归因（全量）：pair default + sl combo = $221.41 / PF 1.1828；pair 0.985 + sl default = $160.99 / PF 1.1519；两者都采纳 = $238.74 / PF 1.2374（组合层超加性：双边筛选互相让出容量）。

## 4. 与「65% WR / PF 3」的诚实差距分析

- **组合口径（最优合法解）**：pair+single_leg 采纳旋钮后 WR **59.44%** / PF **1.2374** / net $238.74。对 65%/PF3 的差距：WR 差 5.56pp，PF 差 2.43×。PF 3 意味着 grossProfit ≈ 3×grossLoss——本语料的 pair 套利是锁定毛利 $0.005–0.01/pair 的高频收租结构（毛利被 fee 下限与 0.985 成本上限双向夹死），单腿是 63–71% 胜率但赔率对称（赢亏同量级）；**没有任何域内旋钮组合能把这个结构推到 PF 3**。
- **为何不动 default 的策略确实不该动**：oracle 是校准探针（ Charter 定位），spread/momentum 是数据层断供（§5），三者参与组合只会拖累。
- **合法上探路径（未采纳，记录为证据）**：single_leg min_entry_price=0.60 单开 WR 70.99% / PF 1.2974（全量）——WR 与 PF 都创本语料新高但 net -20.6%，按「PF 或 net ≥+3%」规则走 PF 路径可采纳；combo 已含 0.60 且更平衡。距 PF 3 仍差 2.3×。
- **结构性上限判断**：以 v2 语料的 microstructure（top 每 token ~60s、locked book 普遍、fee schedule 固定），65%/PF3 在当前策略族内不可达。更高的 PF 需要新的 alpha 结构（更长持有期的事件驱动方向性、或更细粒度的 book 数据喂给 book 类策略），均超出本 round 的 Charter（禁改 .lua/.rs）。

## 5. 结构性发现（记档；§5.1/§5.2 根因已由 fix/locked-top-book 修复——BOOKFIX）

1. **v2 语料无 book 快照事件**（只有 `top`，per-token ~60s 一个；`feed.books=0`，tops 34,260/90h 段）。引擎 `update_top`（marketdata.rs）对 locked top（bb==ba，本语料普遍）先按 bid 清 asks 再按 ask 清 bids → 双边全清 → `mid_price=0` → 依赖 `mid>0` 的 book 类策略（spread_arb tracker、lua_momentum classify）状态机永不推进 = 结构性 0 信号。交易组（pair/single_leg/oracle）用 best_ask（locked 时幸存）故能交易。**修复需动 .rs/.lua，本 round 不做。**
   > **【已修复，见 BOOKFIX.md / PR fix/locked-top-book】**：`update_top` 改为 inclusive bounds——locked top 双边保留共享价位、mid=该价；crossed 输入显式 clamp，snapshot 永不呈现 crossed spread。8 个新测试锁定语义。复活实证（train 中段 5×20K 切片，base→fixed）：spread_arb 0→782 closed、lua_momentum 0→5,687 closed。逐位回归红线：pair_discount_arb / single_leg_pair 全量 8/8 分块 trades JSON 与修复前全等（4,921 / 3,675 笔）。本 round §1–§3 的「结构性零信号」记录自此由数据层断供改为已修复；本 PR 的 default 变更（§7）不受影响（成交流逐位一致）。
2. **0.00 价格 intent → 内核 physics 除零 panic**（rust_decimal Division by zero，进程崩溃）。诊断期间由 throwaway 策略包触发；建议内核对 price<=0 的 intent 在 LEGALITY gate 拒绝。
   > **【已修复，#406】**：panic 实际位置不在 physics（`apply_physics` 只有乘法）也不在 LEGALITY gate，而在更早的 execution-policy Place 臂 `budget_usd / req.price`（service.rs，policy verdict 先于 Gate 1 执行）。修复：placement loop 头部对 `Buy && price<=0` entry fail-closed 拒绝（error 日志 + `illegalPriceRejected` 计数器 + per-strategy `illegal.price` 归因桶 + panic 回归测试）。
3. `--backtest-knob` 的域校验与 #401 声明一致工作；`--enable-strategy` 需重复 flag 传多策略（单 arg 逗号串不解析）。

## 6. 复算路径

- 工件根：`/Volumes/Hard Disk/bk-agentopt/`（split-corpus.cjs、gen-train-jobs.py、poolrun.py/poolrun2.py、jobs-*.tsv、reports/*.json、logs/*.log、bin/core-opt-base）。
- 每个数字 = `reports/<name>.json` 的 `trades` 块字段；命令模板见 bk-session/README.md；回放前必须 `rm -f data/strategy-state.json data/positions.daily-loss.json`。
- PROGRESS.md 是完整审计日志（含失败尝试与诊断）。

## 7. 本 PR 的 default 变更声明

**改变默认回测结果**：`single_leg_pair`（min_gap 0.02→0.10，min_entry_price 0.55→0.60）与 `pair_discount_arb`（max_pair_cost 0.995→0.985）。单开与组合效应见 §2/§3 表；charter 门禁 `strategy-no-stop-loss-check.mjs` PASS；未引入止损类参数；未改任何 .lua/.rs。
