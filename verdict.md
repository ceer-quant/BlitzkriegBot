# A5 lane verdict — whale_follow Down 分支修复 + 三窗重跑

- 分支：`fix/whale-down-branch` @ `/Volumes/Hard Disk/bk-worktrees/a5-whale-downfix`
- 修复 commit：`c392d05`（parent `087be2a` = ceer `feat/whale-fair-signal`，PR #856 分支头）
- 身份：bk-agent `<agent@blitzkrieg.local>`；未 push、未动 live 开关/生产配置、未创建 PR

## 0. 基点偏差（如实报）

任务模板指定基点 `refs/remotes/ceer/main`（= `2209696`），但**该 main 不含 PR #856 的 fair 机制**——任务描述的 bug（`fs.at_high`/`push_fair`/`one_minus_floor`）只存在于 `feat/whale-fair-signal`（`c76101b` fair 化提交 + `087be2a` 测试冻结修复）。故 `fix/whale-down-branch` 落在 `087be2a` 之上。若主会话需要基于 ceer/main 的版本，需先合入/重放 PR #856。

## 1. 修复 diff 摘要（commit c392d05，仅动 whale_follow 包内 3 文件）

`strategy.lua`（+28/−16）：
1. `fair_state` 增加对称字段 `runmin = nil, at_low = false`（与 `runmax/at_high` 并列）。
2. `push_fair`：`at_high`/`at_low` 均在 ratchet **之前**计算（创新极值的样本即"在极值上"的样本；平手取内，`f >= runmax` / `f <= runmin` 对称），随后 `runmax`/`runmin` 双向 ratchet。
3. `bk_evaluate` 核心修复：Up 分支（fair ≥ 0.55）极值门保持 `fs.at_high`；Down 分支（fair ≤ 0.45 = one_minus_floor）极值门改用 `fs.at_low`。`at_extreme`/`extreme_txt` 随分支选择，reason 文本区分 "at fair high" / "at fair low"。
4. 注释更新：头注与 gate 3 注释明确「Up 承接在 fair 高点、Down 承接在 fair 低点（Down 形态就是下跌到 floor）」。
5. 整数微单位未动：fair 信号仍走 1e-6 整数栅格（`mean_micro`/`MICRO_ONE`），内嵌 `common/decimal.lua` 块字节级未变。

`selfcheck.lua`（+51 行区）：
- Down 正例改为真实形态：fair **下跌** 0.50→0.46→0.44（established 0.06 ≥ 0.05、60s drift 0.02 ≤ 0.10、**at fair low**），断言买入 `down-1`、ask 0.57、reason 含 "at fair low"。
- 保留反例（旧 bug 路径）：fair **上涨** 0.40→0.44→0.45 到 floor——处于 fair 高点而非低点，必须拒绝 Down。旧代码在此误触发（正是被旧用例掩盖的形状），修复后为墙。
- **反向验收**：新 selfcheck 对旧版 strategy.lua 跑出 5 行红（Down 正例 0 成交＝bug 复现；上涨反例误触发＝掩盖确认）；修复后 52/52 全绿。

`manifest.json`：
- `sha256` 重签为 `3058d12bfefe39d1476b730c84895486bc0e7487dad3f81a781d3d09ff7ce7d5`（= strategy.lua 整文件 sha256，`node scripts/lua-decimal-sync.mjs --apply` 按 门禁实际行为 执行；内嵌 decimal 块未改，无需重嵌）。
- description 更新 gate 3 语义：「sitting AT the round's fair running extreme on the favored side — Up at the running high, Down at the running low」。

门禁全绿：`lua-decimal-check` all green（13 packages 字节一致）；`lua-sandbox-check` PASS（BK_CORE_BIN 指向主仓二进制）；`lua selfcheck.lua` → `selfcheck OK: 52 rows`，exit 0。

## 2. 三窗重跑（fair 对称机制的真实检验）

命令模板（逐 chunk，语料划分与既有基线完全一致）：
```
"/Volumes/Hard Disk/BlitzkriegBot/target/release/blitzkrieg-core" --no-config --engine \
  --fee-model official --no-strategy-state --round-sec 300 --min-round-age 0 --min-time-left 0 \
  --slippage-ticks 1 --seed-balance 100000 --enable-strategy whale_follow \
  --max-positions 10000 --max-positions-per-asset 10000 \
  --lua-strategy-dir "/Volumes/Hard Disk/bk-worktrees/a5-whale-downfix/user_layer/strategies_lua" \
  --backtest <chunk> --backtest-report <out> --backtest-tick-ms 50 --backtest-tail-ms 100
```
- 全部 chunk exit=0、0 malformed、openPositions=0（每笔都结算，n = 成交笔数）。
- EV/股 = net / (10×n)（kernel 每笔 10 股，与既有 aggregate.py 口径一致）。
- 基线 = 同一二进制、同 chunk、同代码路径的既有重放：`fair` 臂 = c76101b（bugged fair），`base` 臂 = per-leg v1（#851，e606d09）。

| 窗 | 臂 | 成交 n | Up 笔 (net) | Down 笔 (net) | WR | 净 PnL | EV/股 | PF |
|---|---|---|---|---|---|---|---|---|
| **EVID 450 轮** | **downfix（修复）** | **45** | 26 (+20.82) | **19 (−21.32)** | 80.0% | **−0.50** | **−0.0011** | 0.99 |
| EVID 450 | fair c76101b（bugged） | 27 | 26 (+20.82) | 1 (−6.36) | 85.2% | +14.46 | +0.0536 | 1.48 |
| EVID 450 | base（per-leg v1） | 71 | 40 (+23.96) | 31 (−23.96) | 77.5% | −0.01 | −0.0000 | 1.00 |
| **H1 291 轮** | **downfix（修复）** | **31** | 14 (+7.89) | **17 (−15.48)** | 77.4% | **−7.59** | **−0.0245** | 0.85 |
| H1 291 | fair c76101b（bugged） | 14 | 14 (+7.89) | 0 | 85.7% | +7.89 | +0.0564 | 1.59 |
| H1 291 | base（per-leg v1） | 52 | 24 (+18.19) | 28 (−24.66) | 78.8% | −6.47 | −0.0124 | 0.92 |
| **H2 818 轮** | **downfix（修复）** | **63** | 36 (−34.33) | **27 (−13.98)** | 71.4% | **−48.30** | **−0.0767** | 0.65 |
| H2 818 | fair c76101b（bugged） | 38 | 37 (−31.62) | 1 (+3.05) | 71.1% | −28.57 | −0.0752 | 0.66 |
| H2 818 | base（per-leg v1） | 132 | 66 (+26.40) | 66 (−19.88) | 78.0% | +6.52 | +0.0049 | 1.03 |

语料/报告：
- EVID 450 轮 = `/Volumes/Hard Disk/bk-session/live-readiness/whale/chunks/chunk-000..012.jsonl`（13 块、distinct conditionId=450，逐块复核）；报告 `oos-whale-fair-20261102/evid/out-chunk-*.json`。
- H1 291 轮 = `oos-whale-fair-20261102/chunks/{pre-00..03,post-00..03}.jsonl`；报告 `replay-downfix/`。
- H2 818 轮 = `oos-whale-fair-20261102/chunks-early/e-00..e-22.jsonl`；报告 `replay-early-downfix/`。
- 汇总脚本/输出：`/tmp/a5-aggregate.py`、`oos-whale-fair-20261102/a5-downfix-aggregate.txt`。

### Up 侧一致性（修复无副作用的直接证据）
- EVID：Up 26 笔、net +20.82 —— 与 bugged 臂**分毫不差**；common 27 轮无 side 错配、fair-only 0。
- H1：common 14 轮 side 全同；downfix 新增 17 轮 Down，fair-only 0。
- H2：common 37 轮中 36 轮 side 全同；唯一翻转轮 `wf-1793252700`（chunk e-14）经语料重建核实：fair 先在 t+95s 探底 0.434（运行低点、窗口内、已建立）→ 修复臂合法触发 Down（−6.36）并**消耗该轮唯一开仓预算**，随后 fair 飙至 0.99（bugged 臂捕的就是这个上涨形态 +2.71）。这是「同轮两个 qualifying 形态 × 每轮一次开仓」的排序效应，非 Up 逻辑变化。另 bugged 臂唯一那笔 Down（+3.05，`wf-1793181900`）正是「fair 上涨到 floor」的人造路径，修复后正确拒绝。
- 结论：修复 = 纯增 Down 触发 + 拒绝上涨型 Down；Up 路径 bit 级不变（1 轮预算排序除外）。

## 3. 结论：【fair 对称机制：负】

预注册判据（任务给定）：修复后任意窗 EV > +3¢ 且三窗同号 →【候选 GO】。

实测：
- EVID 450：EV/股 **−0.11¢**，H1 291：**−2.45¢**，H2 818：**−7.67¢** —— 无任何窗 > +3¢。
- 三窗同号但**全负**；修复后的 Down 腿在三个窗全部净亏（−21.32 / −15.48 / −13.98，每笔 −1.12 / −0.91 / −0.52）。
- 结构性佐证：per-leg v1 的 Down 腿同样三窗全负（−23.96 / −24.66 / −19.88）——在这批 tape 上「中段买入 down 腿（ask 0.55–0.88）」本身就 EV<0，不是实现 bug，而是该形状在该费率（official 0.07·p(1−p)）与该语料（2026-10-25 早期 + 10-29/10-30 OOS）上不成立。
- 顺带如实记录：bugged 臂的实际行为 ≈ Up-only fair 机制，其 EV 在 EVID +5.36¢ / H1 +5.64¢ 为正、但 H2 −7.52¢ 为负——不满足「三窗同号」，同样不构成 GO 依据。

**判定：【fair 对称机制：负】——不标候选 GO。** Down 分支修复是正确且必要的（bug 确凿、反向验收有牙、Up 侧零副作用），但修复暴露的真相是：fair 对称机制的 Down 侧在全部三个窗 EV 为负，整体机制不满足预注册 GO 判据。建议主会话处置时将 whale_follow 的 GO 讨论收缩到「Up-only + fair」形态（其自身也因 H2 负号不达标），或寻找 Down 侧独立的边际来源（如 down 腿 ask 溢价、费率差）后再议。

## 4. 交付清单

- commit `c392d05`（fix，whale_follow 3 文件）@ 分支 `fix/whale-down-branch`（本地，未 push）
- 本 verdict.md（docs commit 落于同分支）
- 回放证据：`/Volumes/Hard Disk/bk-session/oos-whale-fair-20261102/{evid,replay-downfix,replay-early-downfix}/`（36 份 out-*.json + log）+ `a5-downfix-aggregate.txt`
- 门禁输出：selfcheck 52/52、lua-decimal-check all green、lua-sandbox-check PASS（见上）
