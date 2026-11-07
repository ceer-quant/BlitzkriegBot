# lua_momentum thesis_death_exit 接入回放报告（issue #961）

- 分支：`fix/issue-961-thesis-death-exit`
- 复现：`BK_BASE_CORE_BIN=<f3753e11 构建> node scripts/thesis-death-replay.mjs`
- 冻结语料：`scripts/lib/frozen-corpus.mjs` 的 4 个内容钉定窗口（sha256 校验后回放）
- 双臂：
  - **baseline**：core 构建于 `f3753e11`（#961 之前）+ `f3753e11` 的 lua_momentum 0.4.0 包（`git show` 逐字节出包）
  - **candidate**：core 构建于本分支 + lua_momentum 0.5.0（`bk_thesis_death` 谓词：premise flip + `bk.thesis_death_default`，band 12%/6%，stale 30s）
- 回放参数：`--backtest`，seed 1000，notional cap 12，round 900s，tick 50ms（与 exit-economics 门同一 spawn 形状）

## 净利门禁（验收 3）

| 窗口 | baseline net | candidate net | Δ |
|---|---|---|---|
| trend-20260919T1000Z | −17.4932 | −23.5445 | −6.0513 |
| range-20260919T1600Z | −28.2614 | −12.5666 | **+15.6948** |
| trend-20260920T2100Z | −30.6112 | −2.7105 | **+27.9007** |
| range-20260920T2300Z | −2.4466 | −15.2316 | −12.7850 |
| **TOTAL（closed）** | **−78.8125**（143） | **−54.0532**（151） | **+24.7593** |

**结论：接入后总净利优于基线（+24.76 USD，允许持平的下限之上），门禁 PASS。**
两升两降：单边暴跌窗（trend-2100Z）与双边窗（range-1600Z）大幅改善（死亡带把
-50% 级别的扛单截断在 −13% 附近）；一个单边窗与一个双边窗劣化（见归因）。

## 命中证据（验收 1/2 的回放面）

candidate 4 窗合计 151 笔平仓中 **130 笔 `ThesisDeathExit`**（均价 −12.7% ～ −15.9%，
= 12% 价格带 + taker 费用的落地形状），其余为 TakeProfit 11 / TrailingStop 7 /
Settlement 3 —— 基线里 ride-to-settlement / ride-to-stop 的仓位现在在论点死亡处
以受控损失出场。`thesis_death_exit` 标签贯穿到台账（`ExitReason::ThesisDeathExit`，
serde wire `thesis_death_exit`）。

「命中后不再出现加仓/续持 tick」的逐 tick 证据在 `core/blitzkrieg_core/tests/
lua_momentum_thesis_check.rs`（真实包驱动）：死亡同 tick 注入唯一 taker 平仓 intent、
同轮后续 evaluate 对该 condition 不再出现入场候选（entry budget 拒绝）。（回测模式
持久化日志被内核强制关闭 —— `backtest.rs` 的 `BacktestConfig::new` 置空三条 log 路径
—— 因此逐 tick 序列以集成测试为准，报告只承载台账聚合。）

## 劣化窗口归因（验收 3 的「劣化需归因」）

对两个劣化窗口做了带参对照臂（同 candidate core，改 manifest tunable 默认）：

| 窗口 | as-shipped (12/6) | flat-band-only (12/0) | loose (20/0) | baseline |
|---|---|---|---|---|
| trend-20260919T1000Z | −23.54 | **−20.18** | −22.79 | −17.49 |
| range-20260920T2300Z | **−15.23** | −15.94 | −17.82 | −2.45 |

- trend-1000Z：时间加权收紧（tight 6%）贡献了约 −3.4 的差额；放松到 20% 也回不到基线
  —— 该窗是 DOWN 腾内的连续阴跌，入场即接近死亡带，带内止损本身就是这批交易的形状。
- range-2300Z：放得更松**更差**（−17.82）—— 死亡带不是该窗的调参旋钮；基线在该窗的
  优势来自「扛到结算/50% 止损」的运气成分（均值回归窗里没被 50% 止损打掉）。
- 两个劣化窗共 −18.8，两个改善窗共 +43.6，总账 +24.8。带参（12/6）保持 as-shipped。

## 附带观测

- 该示例包在本语料两臂均为净负（与 RETEST-2026-10-10 普查一致 —— 语料是敌对窗口）。
- 回放产物：`summary.json` + 每 window 每 arm 的 `--backtest-report` JSON
  （本次运行目录 `/tmp/bk-thesis-death-replay-961/`，脚本可从冻结语料重导出）。
